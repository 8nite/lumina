//! Core tool registry: read/edit/write/bash/grep/glob.
//! Mirrors `internal/tools` tool surface (M1: no subagents, no MCP — those
//! claim their own milestones). Every call is gated by `security::decide`.

use crate::{
    approve::ApprovalGate,
    plugins::Host,
    rules::Bundle,
    security::{self, Agent, Decision, PermissionMode},
};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ToolError {
    #[error("tool: denied: {0}")]
    Denied(String),
    #[error("tool: approval required for {0} (re-run with --auto-approve or --bypass)")]
    ApprovalRequired(String),
    #[error("tool: {0}")]
    Msg(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone)]
pub struct SecCtx {
    pub workspace: PathBuf,
    pub session: String,
    pub agent: Agent,
    pub mode: PermissionMode,
    /// Fail-closed for unknown tools when true (headless `run`).
    pub headless: bool,
    /// Live approval broker in server/TUI mode; None in headless `run`.
    pub gate: ApprovalGate,
    /// Glob/command rules from `[permissions.bash]`.
    pub bash_rules: HashMap<String, String>,
    /// Opt-in writes to skills//instructions docs (default false).
    pub allow_tooling_write: bool,
    /// Subagent nesting depth (children may not delegate further).
    pub depth: u8,
}

#[derive(Debug, Clone)]
pub struct Outcome {
    pub output: String,
    pub truncated: bool,
}

pub struct Registry {
    /// Attached KB engine (agent wires `kb::query` here; absent → clean error).
    pub kb_query: Option<KbQueryFn>,
    /// Loaded skill/agent/command bundle for the `skill` tool.
    pub bundle: Option<Bundle>,
    /// `[websearch] provider` selection ("" = auto across keyed providers).
    pub websearch_provider: String,
    /// Lumina-compatible JS/TS plugin worker (None when unconfigured).
    pub plugins: Option<Arc<Host>>,
    /// Subagent executor (wired by the agent to avoid a tools↔agent cycle).
    pub subagent: Option<SubagentFn>,
    /// Per-agent tool matrix (`tools: {bash: false}` in agents/*.md).
    pub disabled_tools: HashSet<String>,
}

/// Subagent runner: (parent ctx, task, agent name) -> final text.
pub type SubagentFn = Arc<dyn Fn(&SecCtx, &str, &str) -> Result<String, String> + Send + Sync>;

/// `kb::query` adapter: (workspace, query, budget) -> context text.
pub type KbQueryFn = Arc<dyn Fn(&Path, &str, usize) -> Result<String, String> + Send + Sync>;

impl Registry {
    pub fn new() -> Self {
        Self { kb_query: None, bundle: None, websearch_provider: String::new(), plugins: None, subagent: None, disabled_tools: HashSet::new() }
    }

    pub fn call(
        &self,
        sc: &SecCtx,
        name: &str,
        args: &HashMap<String, serde_json::Value>,
    ) -> Result<Outcome, ToolError> {
        let mut args = args.clone();
        // Lumina hook: tool.execute.before may rewrite args or block.
        if let Some(h) = &self.plugins {
            let payload = serde_json::json!({"tool": name, "args": args, "sessionID": sc.session});
            if let Some(out) = h.dispatch("tool.execute.before", &payload, Duration::from_secs(5)) {
                if let Some(block) = out.get("block").and_then(|b| b.as_str()) {
                    return Err(ToolError::Denied(format!("{name}: {block}")));
                }
                if let Some(patch) = out.get("patch").and_then(|p| p.get("args")) {
                    if let Some(obj) = patch.as_object() {
                        args = obj.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
                    }
                }
            }
        }
        let target = tool_target(name, &args);
        let destructive = name == "bash" && is_destructive(&target);
        let eval = crate::security::Eval {
            agent: sc.agent,
            mode: sc.mode,
            tool: name.into(),
            target: target.clone(),
            workspace: sc.workspace.clone(),
            destructive,
            bash_rules: sc.bash_rules.clone(),
            allow_tooling_write: sc.allow_tooling_write,
        };
        match security::decide(&eval) {
            Decision::Deny => return Err(ToolError::Denied(format!("{name} {target}"))),
            Decision::Ask => {
                let mut plugin_allow = false;
                if let Some(h) = &self.plugins {
                    let payload = serde_json::json!({"tool": name, "target": target, "sessionID": sc.session});
                    if let Some(out) = h.dispatch("permission.ask", &payload, Duration::from_secs(5)) {
                        match plugin_verdict(&out).as_str() {
                            "allow" => plugin_allow = true,
                            "deny" => return Err(ToolError::Denied(format!("{name} {target} (plugin)"))),
                            _ => {}
                        }
                    }
                }
                if !plugin_allow {
                    if sc.gate.is_live() {
                        // Interactive server/TUI mode: block on the approval modal.
                        match sc.gate.ask(&sc.session, name, &target) {
                            Ok(true) => {}
                            Ok(false) => return Err(ToolError::Denied(format!("{name} {target} (denied)"))),
                            Err(e) => return Err(ToolError::Msg(e)),
                        }
                    } else if sc.headless {
                        return Err(ToolError::ApprovalRequired(format!("{name} {target}")));
                    } else {
                        return Err(ToolError::Denied(format!("{name} {target} (ask)")));
                    }
                }
            }
            Decision::Allow => {}
        }
        // Per-agent tool matrix (agents/*.md `tools: {bash: false}`).
        if self.disabled_tools.contains(name) {
            return Err(ToolError::Denied(format!("{name}: disabled by the active agent")));
        }
        let started = std::time::Instant::now();
        let result = match name {
            "read" => do_read(sc, &args),
            "edit" => do_edit(sc, &args),
            "write" => do_write(sc, &args),
            "bash" | "shell" => do_bash(self, sc, &args),
            "grep" | "lsp" => do_grep(sc, &args),
            "glob" => do_glob(sc, &args),
            "kb_query" => do_kb_query(self, sc, &args),
            "memory_write" => do_memory_write(sc, &args),
            "skill" => do_skill(self, &args),
            "question" => do_question(sc, &args),
            "websearch" => do_websearch(self, &args),
            "webfetch" => do_webfetch(&args),
            "mcp" => do_mcp(sc, &args),
            "subagent" => do_subagent(self, sc, &args),
            "todowrite" => do_todowrite(sc, &args),
            "patch" => do_patch(sc, &args),
            _ => Err(ToolError::Msg(format!("unknown tool {name}"))),
        };
        // Lumina hook: tool.execute.after observes (never blocks).
        if let Some(h) = &self.plugins {
            let summary = match &result {
                Ok(o) => format!("ok {}b", o.output.len()),
                Err(e) => format!("error: {e}"),
            };
            h.notify(
                "tool.execute.after",
                &serde_json::json!({
                    "tool": name, "args": args, "sessionID": sc.session,
                    "result": summary, "elapsed_ms": started.elapsed().as_millis() as u64,
                }),
            );
        }
        result
    }

}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

fn tool_target(name: &str, args: &HashMap<String, serde_json::Value>) -> String {
    let s = |keys: &[&str]| {
        keys.iter()
            .filter_map(|k| args.get(*k).and_then(|v| v.as_str()))
            .next()
            .unwrap_or("")
            .to_string()
    };
    match name {
        "read" | "edit" | "write" => s(&["path", "file", "target"]),
        "memory_write" => {
            let t = s(&["topic", "name"]);
            if t.is_empty() { String::new() } else { format!("memory/{t}") }
        }
        "bash" | "shell" => s(&["command", "cmd"]),
        "grep" | "lsp" => s(&["pattern", "query"]),
        "glob" => s(&["pattern", "glob"]),
        "skill" => s(&["name", "skill"]),
        "kb_query" => s(&["query", "q"]),
        "websearch" => s(&["query", "q"]),
        "webfetch" => s(&["url"]),
        "mcp" => format!("{}:{}", s(&["server"]), s(&["tool", "name"])),
        "subagent" => s(&["task", "prompt"]),
        "patch" => first_patch_path(&s(&["patch", "diff"])),
        "todowrite" => String::new(),
        _ => String::new(),
    }
}

fn is_destructive(cmd: &str) -> bool {
    // Delegate to the security floor (Go IsDestructiveBash parity).
    crate::security::is_destructive_bash(cmd)
}

fn int_arg(args: &HashMap<String, serde_json::Value>, key: &str, d: u64) -> u64 {
    args.get(key)
        .and_then(|v| v.as_u64().or_else(|| v.as_str()?.parse().ok()))
        .unwrap_or(d)
}

fn str_arg(args: &HashMap<String, serde_json::Value>, keys: &[&str]) -> String {
    keys.iter()
        .filter_map(|k| args.get(*k))
        .map(|v| match v {
            serde_json::Value::String(s) => s.clone(),
            v => v.to_string(),
        })
        .next()
        .unwrap_or_default()
}

fn resolve(sc: &SecCtx, p: &str) -> Result<PathBuf, ToolError> {
    let cand = if Path::new(p).is_absolute() {
        PathBuf::from(p)
    } else {
        sc.workspace.join(p)
    };
    // Lexical containment (no symlink chase on missing files).
    let mut norm = PathBuf::new();
    for comp in cand.components() {
        use std::path::Component::*;
        match comp {
            RootDir => norm.push("/"),
            Prefix(p) => norm.push(p.as_os_str()),
            CurDir => {}
            ParentDir => {
                norm.pop();
            }
            Normal(c) => norm.push(c),
        }
    }
    if !norm.starts_with(&sc.workspace) {
        return Err(ToolError::Denied(format!("escape {p}")));
    }
    Ok(norm)
}

const MAX_BYTES: u64 = 256 * 1024;

fn cap(s: String) -> Outcome {
    if s.len() as u64 > MAX_BYTES {
        let mut t = s;
        t.truncate(MAX_BYTES as usize);
        Outcome { output: t, truncated: true }
    } else {
        Outcome { output: s, truncated: false }
    }
}

fn do_read(sc: &SecCtx, args: &HashMap<String, serde_json::Value>) -> Result<Outcome, ToolError> {
    let p = str_arg(args, &["path", "file"]);
    let abs = resolve(sc, &p)?;
    let meta = std::fs::metadata(&abs).map_err(|_| ToolError::Msg(format!("read: no such file {p}")))?;
    if meta.len() > MAX_BYTES {
        // Paged read: offset/limit in lines (ponytail: head/tail math inline).
        let text = std::fs::read_to_string(&abs)?;
        let offset = args.get("offset").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
        let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(200) as usize;
        let lines: Vec<&str> = text.lines().collect();
        let end = (offset + limit).min(lines.len());
        let mut out = format!("{p} (lines {}-{}/{}, {} bytes total)\n", offset + 1, end, lines.len(), meta.len());
        out.push_str(&lines.get(offset..end).unwrap_or(&[]).join("\n"));
        return Ok(cap(out));
    }
    Ok(cap(std::fs::read_to_string(&abs)?))
}

fn do_write(sc: &SecCtx, args: &HashMap<String, serde_json::Value>) -> Result<Outcome, ToolError> {
    let p = str_arg(args, &["path", "file"]);
    let content = str_arg(args, &["content", "text"]);
    let abs = resolve(sc, &p)?;
    if let Some(dir) = abs.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&abs, content)?;
    Ok(Outcome { output: format!("wrote {p}"), truncated: false })
}

fn do_edit(sc: &SecCtx, args: &HashMap<String, serde_json::Value>) -> Result<Outcome, ToolError> {
    let p = str_arg(args, &["path", "file"]);
    let old = str_arg(args, &["old_string", "old", "needle"]);
    let new = str_arg(args, &["new_string", "new", "replacement"]);
    if old.is_empty() {
        return Err(ToolError::Msg("edit: old_string is empty".into()));
    }
    let abs = resolve(sc, &p)?;
    let text = std::fs::read_to_string(&abs).map_err(|_| ToolError::Msg(format!("edit: no such file {p}")))?;
    let matches = text.matches(&old).count();
    if matches != 1 {
        return Err(ToolError::Msg(format!("edit: needle matches {matches}x (need exactly 1) in {p}")));
    }
    std::fs::write(&abs, text.replacen(&old, &new, 1))?;
    Ok(Outcome { output: format!("edited {p}"), truncated: false })
}

fn do_bash(reg: &Registry, sc: &SecCtx, args: &HashMap<String, serde_json::Value>) -> Result<Outcome, ToolError> {
    let cmd_str = str_arg(args, &["command", "cmd"]);
    if cmd_str.trim().is_empty() {
        return Err(ToolError::Msg("bash: empty command".into()));
    }
    // Lumina hook: shell.env injects/overrides environment variables.
    let mut extra_env: Vec<(String, String)> = Vec::new();
    if let Some(h) = &reg.plugins {
        let payload = serde_json::json!({"sessionID": sc.session, "workspace": sc.workspace.display().to_string()});
        if let Some(out) = h.dispatch("shell.env", &payload, Duration::from_secs(5)) {
            let env_obj = out.get("env").or_else(|| out.get("patch")).and_then(|p| p.get("env")).unwrap_or(&serde_json::Value::Null);
            if let Some(map) = env_obj.as_object() {
                for (k, v) in map {
                    if let Some(v) = v.as_str() {
                        extra_env.push((k.clone(), v.to_string()));
                    }
                }
            }
        }
    }
    let timeout = args
        .get("timeout_ms")
        .and_then(|v| v.as_u64())
        .map(Duration::from_millis)
        .unwrap_or(Duration::from_secs(120));
    // ponytail: std Command + scoped kill via wait_timeout emulation (poll loop).
    let mut cmd = std::process::Command::new("sh");
    cmd.arg("-c")
        .arg(&cmd_str)
        .current_dir(&sc.workspace)
        // Per-session TMPDIR hygiene (cleaned on session.deleted).
        .env("TMPDIR", std::env::temp_dir().join(format!("lumina-ses-{}", sc.session)))
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    for (k, v) in &extra_env {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn()?;
    let start = std::time::Instant::now();
    loop {
        match child.try_wait()? {
            Some(status) => {
                let out = child.wait_with_output()?;
                let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
                let err = String::from_utf8_lossy(&out.stderr);
                if !err.is_empty() {
                    text.push_str("\n[stderr]\n");
                    text.push_str(&err);
                }
                if !status.success() {
                    text.push_str(&format!("\n[exit {}]", status.code().unwrap_or(-1)));
                }
                return Ok(cap(text));
            }
            None if start.elapsed() > timeout => {
                let _ = child.kill();
                return Err(ToolError::Msg(format!("bash: timed out after {timeout:?}")));
            }
            None => std::thread::sleep(Duration::from_millis(25)),
        }
    }
}

fn do_grep(sc: &SecCtx, args: &HashMap<String, serde_json::Value>) -> Result<Outcome, ToolError> {
    let pattern = str_arg(args, &["pattern", "query"]);
    if pattern.is_empty() {
        return Err(ToolError::Msg("grep: empty pattern".into()));
    }
    let include = str_arg(args, &["include", "glob"]);
    let re = regex::Regex::new(&pattern).map_err(|e| ToolError::Msg(format!("grep: bad regex: {e}")))?;
    let mut hits = Vec::new();
    let mut files = 0usize;
    // ponytail: small recursive walker, no ignore-crate until it earns its keep.
    fn walk(dir: &Path, cb: &mut dyn FnMut(&Path)) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            if p.file_name().map(|n| n == ".git" || n == "target" || n == "node_modules").unwrap_or(false) {
                continue;
            }
            if p.is_dir() {
                walk(&p, cb);
            } else {
                cb(&p);
            }
        }
    }
    walk(&sc.workspace, &mut |p: &Path| {
        if hits.len() >= 100 {
            return;
        }
        if !include.is_empty() {
            let pat = glob::Pattern::new(&include).unwrap_or(glob::Pattern::new("*").unwrap());
            if !pat.matches(p.file_name().and_then(|n| n.to_str()).unwrap_or("")) {
                return;
            }
        }
        files += 1;
        if files > 5000 {
            return;
        }
        let Ok(text) = std::fs::read_to_string(p) else { return };
        for (i, line) in text.lines().enumerate() {
            if re.is_match(line) {
                hits.push(format!("{}:{}: {}", p.strip_prefix(&sc.workspace).unwrap_or(p).display(), i + 1, line.trim()));
                if hits.len() >= 100 {
                    return;
                }
            }
        }
    });
    Ok(cap(if hits.is_empty() { "(no matches)".into() } else { hits.join("\n") }))
}

fn do_glob(sc: &SecCtx, args: &HashMap<String, serde_json::Value>) -> Result<Outcome, ToolError> {
    let pattern = str_arg(args, &["pattern", "glob"]);
    if pattern.is_empty() {
        return Err(ToolError::Msg("glob: empty pattern".into()));
    }
    let full = sc.workspace.join(&pattern);
    let mut hits: Vec<String> = glob::glob(full.to_str().unwrap_or(""))
        .map_err(|e| ToolError::Msg(format!("glob: {e}")))?
        .flatten()
        .filter_map(|p| p.strip_prefix(&sc.workspace).ok().map(|r| r.display().to_string()))
        .take(200)
        .collect();
    hits.sort();
    Ok(cap(if hits.is_empty() { "(no matches)".into() } else { hits.join("\n") }))
}


fn do_kb_query(reg: &Registry, sc: &SecCtx, args: &HashMap<String, serde_json::Value>) -> Result<Outcome, ToolError> {
    let Some(f) = &reg.kb_query else {
        return Err(ToolError::Msg("kb_query: engine not attached".into()));
    };
    let q = str_arg(args, &["query", "q"]);
    let budget = int_arg(args, "budget", 4000) as usize;
    f(&sc.workspace, &q, budget)
        .map(|s| {
            let truncated = s.len() > 6000;
            Outcome { output: s, truncated }
        })
        .map_err(ToolError::Msg)
}

fn do_memory_write(sc: &SecCtx, args: &HashMap<String, serde_json::Value>) -> Result<Outcome, ToolError> {
    let topic = str_arg(args, &["topic", "name"]);
    let content = str_arg(args, &["content", "body"]);
    crate::kb::memory_write(&sc.workspace, &topic, &content)
        .map(|s| Outcome { output: s, truncated: false })
        .map_err(|e| ToolError::Msg(e.to_string()))
}

fn do_skill(reg: &Registry, args: &HashMap<String, serde_json::Value>) -> Result<Outcome, ToolError> {
    let name = str_arg(args, &["name", "skill"]);
    if name.is_empty() {
        return Err(ToolError::Msg("skill: missing name".into()));
    }
    let Some(b) = &reg.bundle else {
        return Err(ToolError::Msg("skill: bundle not attached".into()));
    };
    match b.skills.iter().find(|s| s.name == name) {
        Some(sk) => std::fs::read_to_string(&sk.path)
            .map(|body| {
                let truncated = body.len() > 8000;
                Outcome { output: body, truncated }
            })
            .map_err(|e| ToolError::Msg(format!("skill {name}: {e}"))),
        None => {
            let avail: Vec<&str> = b.skills.iter().map(|s| s.name.as_str()).collect();
            Err(ToolError::Msg(format!("skill {name:?} not found. Available: {}", avail.join(", "))))
        }
    }
}

fn do_question(sc: &SecCtx, args: &HashMap<String, serde_json::Value>) -> Result<Outcome, ToolError> {
    if !sc.gate.is_live() {
        return Err(ToolError::Msg("question: interactive gate not attached".into()));
    }
    let raw = args.get("questions").cloned().unwrap_or(serde_json::Value::Null);
    let list = raw.as_array().cloned().unwrap_or_default();
    if list.is_empty() {
        return Err(ToolError::Msg("question: no questions provided".into()));
    }
    let mut qs = Vec::new();
    for q in list.iter().take(4) {
        qs.push(crate::approve::Question {
            text: q.get("text").and_then(|t| t.as_str()).unwrap_or("?").to_string(),
            options: q
                .get("options")
                .and_then(|o| o.as_array())
                .map(|a| a.iter().filter_map(|o| o.as_str().map(|s| s.to_string())).collect())
                .unwrap_or_default(),
        });
    }
    let answers = sc.gate.ask_question(&sc.session, qs).map_err(ToolError::Msg)?;
    Ok(Outcome {
        output: format!("user answered: {}", answers.join(" | ")),
        truncated: false,
    })
}

fn do_websearch(reg: &Registry, args: &HashMap<String, serde_json::Value>) -> Result<Outcome, ToolError> {
    let q = str_arg(args, &["query", "q"]);
    let provider = str_arg(args, &["provider"]);
    let want = if provider.is_empty() { reg.websearch_provider.clone() } else { provider };
    crate::websearch::search(&want, &q)
        .map(|s| {
            let truncated = s.len() > 8000;
            Outcome { output: s, truncated }
        })
        .map_err(|e| ToolError::Msg(e.to_string()))
}

fn do_webfetch(args: &HashMap<String, serde_json::Value>) -> Result<Outcome, ToolError> {
    let url = str_arg(args, &["url"]);
    let format = str_arg(args, &["format"]);
    let format = if format.is_empty() { "markdown" } else { &format };
    let timeout_ms = int_arg(args, "timeout_ms", 30_000).min(120_000);
    crate::websearch::fetch(&url, format, std::time::Duration::from_millis(timeout_ms))
        .map(|(s, t)| Outcome { output: s, truncated: t })
        .map_err(|e| ToolError::Msg(e.to_string()))
}

fn do_mcp(sc: &SecCtx, args: &HashMap<String, serde_json::Value>) -> Result<Outcome, ToolError> {
    let server = str_arg(args, &["server"]);
    let tool = str_arg(args, &["tool", "name"]);
    if server.is_empty() || tool.is_empty() {
        return Err(ToolError::Msg("mcp: missing server/tool (see .lumina/mcp.json)".into()));
    }
    let cfg = crate::mcp::ConfigFile::load(&sc.workspace);
    let Some(conf) = cfg.servers.get(&server) else {
        return Err(ToolError::Msg(format!("mcp: no server {server:?} configured (see .lumina/mcp.json)")));
    };
    let mut client = crate::mcp::Client::dial(conf.clone()).map_err(|e| ToolError::Msg(e.to_string()))?;
    let call_args = args.get("args").cloned().unwrap_or(serde_json::Value::Null);
    client
        .call_tool(&tool, call_args)
        .map(|s| {
            let truncated = s.len() > 8000;
            Outcome { output: s, truncated }
        })
        .map_err(|e| ToolError::Msg(e.to_string()))
}

/// Extract an allow/deny/ask verdict from a `permission.ask` worker reply.
/// Accepts {verdict|decision}, {output: "..."}, and {patch: {...}} shapes.
fn plugin_verdict(out: &serde_json::Value) -> String {
    let get = |v: &serde_json::Value| {
        v.get("verdict")
            .or_else(|| v.get("decision"))
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_lowercase()
    };
    let direct = get(out);
    if !direct.is_empty() {
        return direct;
    }
    if let Some(o) = out.get("output").and_then(|o| o.as_str()) {
        let t = o.trim().to_lowercase();
        if matches!(t.as_str(), "allow" | "deny" | "ask") {
            return t;
        }
    }
    if let Some(p) = out.get("patch") {
        let v = get(p);
        if !v.is_empty() {
            return v;
        }
    }
    String::new()
}

fn do_subagent(reg: &Registry, sc: &SecCtx, args: &HashMap<String, serde_json::Value>) -> Result<Outcome, ToolError> {
    if sc.depth >= 1 {
        return Err(ToolError::Msg("subagents cannot launch subagents".into()));
    }
    let task = str_arg(args, &["task", "prompt"]);
    if task.is_empty() {
        return Err(ToolError::Msg("subagent: missing task".into()));
    }
    let name = str_arg(args, &["agent", "name"]);
    let name = if name.is_empty() { "general" } else { name.as_str() };
    let Some(f) = &reg.subagent else {
        return Err(ToolError::Msg("subagent: executor not attached".into()));
    };
    f(sc, &task, name)
        .map(|s| {
            let truncated = s.len() > 8000;
            Outcome { output: s, truncated }
        })
        .map_err(ToolError::Msg)
}

/// Per-session todo lists (daemon-lifetime; mirrors Go's in-memory registry map).
type TodoStore = Mutex<HashMap<String, Vec<(String, String)>>>;

fn do_todowrite(sc: &SecCtx, args: &HashMap<String, serde_json::Value>) -> Result<Outcome, ToolError> {
    use std::sync::OnceLock;
    static TODOS: OnceLock<TodoStore> = OnceLock::new();
    let store = TODOS.get_or_init(|| Mutex::new(HashMap::new()));
    let raw = args.get("todos").cloned().unwrap_or(serde_json::Value::Null);
    let mut list = Vec::new();
    if let Some(arr) = raw.as_array() {
        for t in arr {
            let content = t.get("content").and_then(|c| c.as_str()).unwrap_or("").to_string();
            let status = t.get("status").and_then(|s| s.as_str()).unwrap_or("pending").to_string();
            if content.is_empty() {
                continue;
            }
            if !matches!(status.as_str(), "pending" | "in_progress" | "completed" | "cancelled") {
                return Err(ToolError::Msg(format!("todowrite: bad status {status:?}")));
            }
            list.push((content, status));
        }
    }
    let summary = if list.is_empty() {
        store.lock().unwrap().remove(&sc.session);
        "(todo list cleared)".to_string()
    } else {
        let mut s = String::from("todos:\n");
        for (c, st) in &list {
            let mark = match st.as_str() {
                "completed" => "[x]",
                "in_progress" => "[>]",
                "cancelled" => "[-]",
                _ => "[ ]",
            };
            s.push_str(&format!("{mark} {c}\n"));
        }
        store.lock().unwrap().insert(sc.session.clone(), list);
        s
    };
    Ok(Outcome { output: summary, truncated: false })
}

/// Apply a unified diff (---/+++ file headers, @@ hunks) with exact context
/// matching. Multi-file; fails closed on ambiguous or missing context.
fn do_patch(sc: &SecCtx, args: &HashMap<String, serde_json::Value>) -> Result<Outcome, ToolError> {
    let diff = str_arg(args, &["patch", "diff"]);
    if diff.trim().is_empty() {
        return Err(ToolError::Msg("patch: empty diff".into()));
    }
    let files = parse_unified(&diff)?;
    let mut edited = Vec::new();
    for (rel, hunks) in files {
        let abs = resolve(sc, &rel)?;
        let original = std::fs::read_to_string(&abs).map_err(|_| ToolError::Msg(format!("patch: no such file {rel}")))?;
        let mut lines: Vec<String> = original.lines().map(|l| l.to_string()).collect();
        // Apply bottom-up so earlier offsets stay valid.
        let mut ordered = hunks;
        ordered.sort_by_key(|h| std::cmp::Reverse(h.old_start));
        for h in ordered {
            let start = h.old_start.checked_sub(1).ok_or_else(|| ToolError::Msg("patch: bad hunk header".into()))?;
            let mut cur = start;
            let mut replacement: Vec<String> = Vec::new();
            for ln in &h.lines {
                let (kind, text) = ln.split_at(1);
                match kind {
                    " " => {
                        if lines.get(cur).map(|l| l.as_str()) != Some(text) {
                            return Err(ToolError::Msg(format!("patch: context mismatch in {rel} near {text:?}")));
                        }
                        replacement.push(text.to_string());
                        cur += 1;
                    }
                    "-" => {
                        if lines.get(cur).map(|l| l.as_str()) != Some(text) {
                            return Err(ToolError::Msg(format!("patch: removal mismatch in {rel} near {text:?}")));
                        }
                        cur += 1;
                    }
                    "+" => replacement.push(text.to_string()),
                    _ => {
                        if !kind.trim().is_empty() {
                            return Err(ToolError::Msg(format!("patch: bad hunk line {ln:?} in {rel}")));
                        }
                    }
                }
            }
            lines.splice(start..cur, replacement);
        }
        let mut out = lines.join("\n");
        if original.ends_with('\n') {
            out.push('\n');
        }
        std::fs::write(&abs, out)?;
        edited.push(rel);
    }
    Ok(Outcome { output: format!("patched {}", edited.join(", ")), truncated: false })
}

struct Hunk {
    old_start: usize,
    lines: Vec<String>,
}

fn parse_unified(diff: &str) -> Result<Vec<(String, Vec<Hunk>)>, ToolError> {
    let err = |m: &str| ToolError::Msg(format!("patch: {m}"));
    let mut files: Vec<(String, Vec<Hunk>)> = Vec::new();
    let mut cur: Option<(String, Vec<Hunk>)> = None;
    let mut hunk: Option<Hunk> = None;
    let flush_hunk = |cur: &mut Option<(String, Vec<Hunk>)>, hunk: &mut Option<Hunk>| {
        if let Some(h) = hunk.take() {
            if let Some((_, hs)) = cur {
                hs.push(h);
            }
        }
    };
    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("--- ") {
            if let Some(c) = cur.take() {
                files.push(c);
            }
            let path = rest.trim().trim_start_matches("a/").to_string();
            cur = Some((path, Vec::new()));
            continue;
        }
        if let Some(rest) = line.strip_prefix("+++ ") {
            // New-file path wins when it differs (dev/null on one side).
            if rest.trim() != "/dev/null" {
                if let Some((p, hs)) = cur.take() {
                    let np = rest.trim().trim_start_matches("b/").to_string();
                    cur = Some((if p == "/dev/null" || p.is_empty() { np } else { p }, hs));
                }
            }
            continue;
        }
        if let Some(hdr) = line.strip_prefix("@@ ") {
            flush_hunk(&mut cur, &mut hunk);
            let old = hdr.split_whitespace().next().ok_or_else(|| err("bad hunk header"))?;
            let start: usize = old.trim_start_matches('-').split(',').next().unwrap_or("0").parse().map_err(|_| err("bad hunk header"))?;
            hunk = Some(Hunk { old_start: start.max(1), lines: Vec::new() });
            continue;
        }
        if line.starts_with('\\') {
            continue; // "\ No newline at end of file"
        }
        if let Some(h) = hunk.as_mut() {
            if line.is_empty() {
                h.lines.push(" ".to_string());
            } else {
                h.lines.push(line.to_string());
            }
        }
    }
    flush_hunk(&mut cur, &mut hunk);
    if let Some(c) = cur.take() {
        files.push(c);
    }
    if files.is_empty() {
        return Err(err("no files in diff"));
    }
    Ok(files)
}


/// First +++ path of a unified diff (fence authorization granularity).
fn first_patch_path(diff: &str) -> String {
    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("+++ ") {
            let p = rest.trim();
            if p != "/dev/null" {
                return p.trim_start_matches("b/").to_string();
            }
        }
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(dir: &Path, agent: Agent, mode: PermissionMode) -> SecCtx {
        SecCtx { workspace: dir.to_path_buf(), session: "t".into(), agent, mode, headless: true, gate: ApprovalGate::default(), bash_rules: HashMap::new(), allow_tooling_write: false, depth: 0 }
    }

    fn args(pairs: &[(&str, &str)]) -> HashMap<String, serde_json::Value> {
        pairs.iter().map(|(k, v)| ((*k).into(), serde_json::Value::String((*v).into()))).collect()
    }

    #[test]
    fn read_write_roundtrip() {
        let d = tempfile::tempdir().unwrap();
        let reg = Registry::new();
        let sc = ctx(d.path(), Agent::Build, PermissionMode::Bypass);
        reg.call(&sc, "write", &args(&[("path", "a.txt"), ("content", "hi")])).unwrap();
        let out = reg.call(&sc, "read", &args(&[("path", "a.txt")])).unwrap();
        assert_eq!(out.output, "hi");
    }

    #[test]
    fn plan_cannot_edit_source() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("x.rs"), "old").unwrap();
        let reg = Registry::new();
        let sc = ctx(d.path(), Agent::Plan, PermissionMode::Bypass);
        let err = reg.call(&sc, "edit", &args(&[("path", "x.rs"), ("old_string", "old"), ("new_string", "new")])).unwrap_err();
        assert!(matches!(err, ToolError::Denied(_)), "{err}");
    }

    #[test]
    fn headless_ask_becomes_approval_error() {
        let d = tempfile::tempdir().unwrap();
        let reg = Registry::new();
        let sc = ctx(d.path(), Agent::Build, PermissionMode::Ask);
        let err = reg.call(&sc, "bash", &args(&[("command", "ls")])).unwrap_err();
        assert!(matches!(err, ToolError::ApprovalRequired(_)), "{err}");
    }

    #[test]
    fn destructive_bash_asks() {
        assert!(is_destructive("rm -rf /tmp/x"));
        assert!(!is_destructive("go test ./..."));
    }

    #[test]
    fn escape_denied() {
        let d = tempfile::tempdir().unwrap();
        let reg = Registry::new();
        let sc = ctx(d.path(), Agent::Build, PermissionMode::Bypass);
        let err = reg.call(&sc, "read", &args(&[("path", "../secret")])).unwrap_err();
        assert!(matches!(err, ToolError::Denied(_)), "{err}");
    }
}

#[cfg(test)]
mod extended_tests {
    use super::*;

    fn ctx2(dir: &Path) -> SecCtx {
        SecCtx {
            workspace: dir.to_path_buf(), session: "t".into(), agent: Agent::Build,
            mode: PermissionMode::Bypass, headless: true, gate: ApprovalGate::default(),
            bash_rules: HashMap::new(), allow_tooling_write: false, depth: 0,
        }
    }

    fn args(pairs: &[(&str, &str)]) -> HashMap<String, serde_json::Value> {
        pairs.iter().map(|(k, v)| ((*k).into(), serde_json::Value::String((*v).into()))).collect()
    }

    #[test]
    fn patch_applies_and_rejects_bad_context() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("f.txt"), "a\nb\nc\n").unwrap();
        let reg = Registry::new();
        let good = "--- a/f.txt\n+++ b/f.txt\n@@ -1,3 +1,3 @@\n a\n-b\n+B\n c\n";
        let out = reg.call(&ctx2(d.path()), "patch", &args(&[("patch", good)])).unwrap();
        assert!(out.output.contains("f.txt"));
        assert_eq!(std::fs::read_to_string(d.path().join("f.txt")).unwrap(), "a\nB\nc\n");
        let bad = "--- a/f.txt\n+++ b/f.txt\n@@ -1,3 +1,3 @@\n a\n-zzz\n+Z\n c\n";
        assert!(reg.call(&ctx2(d.path()), "patch", &args(&[("patch", bad)])).is_err());
    }

    #[test]
    fn todos_roundtrip_and_validate() {
        let d = tempfile::tempdir().unwrap();
        let reg = Registry::new();
        let mut a = HashMap::new();
        a.insert("todos".into(), serde_json::json!([{"content": "x", "status": "in_progress"}]));
        let out = reg.call(&ctx2(d.path()), "todowrite", &a).unwrap();
        assert!(out.output.contains("[>] x"), "{}", out.output);
        let mut bad = HashMap::new();
        bad.insert("todos".into(), serde_json::json!([{"content": "x", "status": "bogus"}]));
        assert!(reg.call(&ctx2(d.path()), "todowrite", &bad).is_err());
    }

    #[test]
    fn subagent_depth_guard() {
        let d = tempfile::tempdir().unwrap();
        let reg = Registry::new();
        let mut sc = ctx2(d.path());
        sc.depth = 1;
        let err = reg.call(&sc, "subagent", &args(&[("task", "hi")])).unwrap_err();
        assert!(matches!(err, ToolError::Msg(_)));
    }

    #[test]
    fn shell_and_lsp_aliases_dispatch() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.txt"), "hello\n").unwrap();
        let reg = Registry::new();
        // shell runs like bash.
        let out = reg.call(&ctx2(d.path()), "shell", &args(&[("command", "echo hi")])).unwrap();
        assert!(out.output.contains("hi"), "{}", out.output);
        // lsp greps like grep.
        let out = reg.call(&ctx2(d.path()), "lsp", &args(&[("pattern", "hello")])).unwrap();
        assert!(out.output.contains("a.txt"), "{}", out.output);
        // tool_target mirrors the alias.
        assert_eq!(tool_target("shell", &args(&[("command", "echo hi")])), "echo hi");
        assert_eq!(tool_target("lsp", &args(&[("pattern", "hello")])), "hello");
    }
}
