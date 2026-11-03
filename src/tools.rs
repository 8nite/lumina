//! Core tool registry: read/edit/write/bash/grep/glob/skill/mcp/kb_query/
//! memory_write/question/websearch/webfetch/subagent/task_output/todowrite/patch.
//! Mirrors `internal/tools` tool surface. Every call is gated by `security::decide`.

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
    /// Opt-in writes to skills//rules docs (default false).
    pub allow_tooling_write: bool,
    /// Subagent nesting depth (children may not delegate further).
    pub depth: u8,
    /// Bubblewrap confinement for `bash` (off by default; see `sandbox`).
    pub sandbox: crate::sandbox::Profile,
}

#[derive(Debug, Clone)]
pub struct Outcome {
    pub output: String,
    pub truncated: bool,
}

/// `lsp::query` adapter: (workspace, op, path, line, character, filter)
/// -> context text. Wired by the agent to the shared `lsp::Manager`;
/// absent (unit tests) the `lsp` tool fails closed with guidance.
pub type LspQueryFn = Arc<dyn Fn(&Path, &str, &str, u64, u64, &str) -> Result<String, String> + Send + Sync>;

pub struct Registry {
    /// Attached KB engine (agent wires `kb::query` here; absent → clean error).
    pub kb_query: Option<KbQueryFn>,
    /// Attached language-server manager (agent wires `lsp::query` here).
    pub lsp_query: Option<LspQueryFn>,
    /// Explicit KB dir override (colon-separated, from `[kb] memory_dir`).
    /// Empty = rule-md + `LUMINA_MEMORY_DIR` + legacy resolution in `kb`.
    pub kb_memory_dir: String,
    /// Loaded skill/agent/command bundle for the `skill` tool.
    pub bundle: Option<Bundle>,
    /// `[websearch] provider` selection ("" = auto across keyed providers).
    pub websearch_provider: String,
    /// Kill-switch: when true the websearch tool is refused (Go parity).
    pub websearch_disabled: bool,
    /// Lumina-compatible JS/TS plugin worker (None when unconfigured).
    pub plugins: Option<Arc<Host>>,
    /// Subagent executor (wired by the agent to avoid a tools↔agent cycle).
    pub subagent: Option<SubagentFn>,
    /// Per-agent tool matrix (`tools: {bash: false}` in agents/*.md).
    pub disabled_tools: HashSet<String>,
}

/// Subagent runner: (parent ctx, request) -> final text.
pub type SubagentFn = Arc<dyn Fn(&SecCtx, &SubagentReq) -> Result<String, String> + Send + Sync>;

/// One scoped child delegation (tools.SubagentRequest parity).
#[derive(Debug, Clone, Default)]
pub struct SubagentReq {
    pub task: String,
    pub agent: String,
    pub description: String,
    pub model: String,
}

/// Delegation cap (default 1; `LUMINA_SUBAGENT_DEPTH` overrides).
pub fn subagent_max_depth() -> u8 {
    std::env::var("LUMINA_SUBAGENT_DEPTH")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .filter(|n: &u8| *n >= 1)
        .unwrap_or(1)
}

/// `kb::query` adapter: (workspace, query, budget) -> context text.
pub type KbQueryFn = Arc<dyn Fn(&Path, &str, usize) -> Result<String, String> + Send + Sync>;

impl Registry {
    pub fn new() -> Self {
        Self { kb_query: None, lsp_query: None, kb_memory_dir: String::new(), bundle: None, websearch_provider: String::new(), websearch_disabled: false, plugins: None, subagent: None, disabled_tools: HashSet::new() }
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
            "grep" => do_grep(sc, &args),
            "lsp" => do_lsp(self, sc, &args),
            "glob" => do_glob(sc, &args),
            "kb_query" => do_kb_query(self, sc, &args),
            "memory_write" => do_memory_write(self, sc, &args),
            "skill" => do_skill(self, &args),
            "question" => do_question(sc, &args),
            "websearch" => do_websearch(self, &args),
            "webfetch" => do_webfetch(&args),
            "mcp" => do_mcp(sc, &args),
            "subagent" => do_subagent(self, sc, &args),
            "task_output" => do_task_output(&args),
            "todowrite" => do_todowrite(sc, &args),
            "patch" => do_patch(sc, &args),
            _ => Err(ToolError::Msg(format!("unknown tool {name}"))),
        };
        // Lumina hook: tool.execute.after observes full args + result
        // (never blocks).
        if let Some(h) = &self.plugins {
            let (output, error) = match &result {
                Ok(o) => (o.output.clone(), String::new()),
                Err(e) => (String::new(), e.to_string()),
            };
            h.notify(
                "tool.execute.after",
                &serde_json::json!({
                    "tool": name, "args": args, "sessionID": sc.session,
                    "output": output, "error": error,
                    "elapsed_ms": started.elapsed().as_millis() as u64,
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

pub(crate) fn tool_target(name: &str, args: &HashMap<String, serde_json::Value>) -> String {
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
        "grep" => s(&["pattern", "query"]),
        "lsp" => {
            let t = s(&["path", "file", "target"]);
            let op = s(&["op"]);
            if op.is_empty() {
                t
            } else if t.is_empty() {
                op
            } else {
                format!("{t}:{op}")
            }
        }
        "glob" => s(&["pattern", "glob"]),
        "skill" => s(&["name", "skill"]),
        "kb_query" => s(&["query", "q"]),
        "websearch" => s(&["query", "q"]),
        "webfetch" => s(&["url"]),
        "mcp" => format!("{}:{}", s(&["server"]), s(&["tool", "name"])),
        "subagent" => s(&["task", "prompt"]),
        "task_output" => {
            let t = s(&["task_id", "id"]);
            if t.is_empty() { name.into() } else { format!("task:{t}") }
        }
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
    int_arg_any(args, &[key], d)
}

fn int_arg_any(args: &HashMap<String, serde_json::Value>, keys: &[&str], d: u64) -> u64 {
    keys.iter()
        .filter_map(|k| args.get(*k))
        .filter_map(|v| v.as_u64().or_else(|| v.as_f64().map(|f| f as u64)).or_else(|| v.as_str()?.parse().ok()))
        .next()
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
    resolve_write(sc, p)
}

/// Read path: workspace or the external read allowlist (home/tmp/system).
fn resolve_read(sc: &SecCtx, p: &str) -> Result<PathBuf, ToolError> {
    if crate::security::is_read_allowed(&sc.workspace, p) {
        return Ok(wash(sc, p));
    }
    Err(ToolError::Denied(format!("escape {p}")))
}

/// Write path: workspace or temp only.
fn resolve_write(sc: &SecCtx, p: &str) -> Result<PathBuf, ToolError> {
    if crate::security::is_write_allowed(&sc.workspace, p) {
        return Ok(wash(sc, p));
    }
    Err(ToolError::Denied(format!("escape {p}")))
}

/// Lexical wash: `~`-expand, join, clean `..` (no symlink chase on missing files).
fn wash(sc: &SecCtx, p: &str) -> PathBuf {
    let e = crate::security::expand_home(p);
    let cand = if Path::new(&e).is_absolute() {
        PathBuf::from(&e)
    } else {
        sc.workspace.join(&e)
    };
    let mut norm = PathBuf::new();
    for comp in cand.components() {
        use std::path::Component::*;
        match comp {
            RootDir => norm.push("/"),
            Prefix(pr) => norm.push(pr.as_os_str()),
            CurDir => {}
            ParentDir => {
                norm.pop();
            }
            Normal(c) => norm.push(c),
        }
    }
    norm
}

/// Byte-cap a string without panicking: `String::truncate` panics when
/// `max_bytes` splits a multi-byte char, and tool outputs routinely carry
/// them (→, ─, …). A 256KB `cap` of CJK/log output killed the daemon's
/// turn worker on exactly this (`is_char_boundary` assertion). Floor to
/// the nearest char boundary instead.
pub fn truncate_to(s: &mut String, max_bytes: usize) {
    if s.len() <= max_bytes {
        return;
    }
    let mut end = max_bytes;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s.truncate(end);
}

const MAX_BYTES: u64 = 256 * 1024;

fn cap(s: String) -> Outcome {
    if s.len() as u64 > MAX_BYTES {
        let mut t = s;
        truncate_to(&mut t, MAX_BYTES as usize);
        Outcome { output: t, truncated: true }
    } else {
        Outcome { output: s, truncated: false }
    }
}

fn do_read(sc: &SecCtx, args: &HashMap<String, serde_json::Value>) -> Result<Outcome, ToolError> {
    let p = str_arg(args, &["path", "file"]);
    if p.is_empty() {
        return Err(ToolError::Msg("read: missing path".into()));
    }
    let abs = resolve_read(sc, &p)?;
    let meta = std::fs::metadata(&abs).map_err(|_| ToolError::Msg(format!("read: no such file {p}")))?;
    let mut offset = int_arg_any(args, &["offset"], 1) as usize;
    if offset < 1 {
        offset = 1;
    }
    let mut limit = int_arg_any(args, &["limit"], 2000) as usize;
    if limit < 1 {
        limit = 2000;
    }
    if limit > 2000 {
        limit = 2000;
    }
    // Directory: one entry per line, paginated like text. Workspace-internal
    // credential entries are filtered; external whitelisted dirs list all
    // dotfiles per explicit user override.
    if meta.is_dir() {
        let mut names: Vec<String> = std::fs::read_dir(&abs)
            .map_err(|e| ToolError::Msg(format!("read: {e}")))?
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| {
                let full = abs.join(n);
                !(crate::security::is_credential_path(&full.to_string_lossy())
                    && full.starts_with(&sc.workspace))
            })
            .collect();
        names.sort();
        let end = (offset - 1 + limit).min(names.len());
        if offset - 1 > names.len() {
            return Ok(Outcome { output: String::new(), truncated: false });
        }
        let truncated = end < names.len();
        return Ok(Outcome { output: names[offset - 1..end].join("\n"), truncated });
    }
    if meta.len() > MAX_BYTES {
        // Paged read for huge files (offset/limit in lines).
        let text = std::fs::read_to_string(&abs)?;
        let lines: Vec<&str> = text.lines().collect();
        let end = (offset - 1 + limit).min(lines.len());
        if offset - 1 > lines.len() {
            return Ok(Outcome { output: String::new(), truncated: false });
        }
        let mut out = format!("{p} (lines {}-{}/{}, {} bytes total)\n", offset, end, lines.len(), meta.len());
        out.push_str(&lines[offset - 1..end].join("\n"));
        return Ok(Outcome { output: out, truncated: end < lines.len() });
    }
    let b = std::fs::read(&abs)?;
    if b.contains(&0) {
        return Err(ToolError::Msg(format!("read: binary file not supported ({p})")));
    }
    let text = String::from_utf8_lossy(&b);
    let lines: Vec<&str> = text.split('\n').collect();
    let end = (offset - 1 + limit).min(lines.len());
    if offset - 1 > lines.len() {
        return Ok(Outcome { output: String::new(), truncated: false });
    }
    // lines and edits stay prefix-free.
    let out: Vec<String> = (offset - 1..end).map(|i| format!("{}: {}", i + 1, lines[i])).collect();
    Ok(Outcome { output: out.join("\n"), truncated: end < lines.len() })
}

fn do_write(sc: &SecCtx, args: &HashMap<String, serde_json::Value>) -> Result<Outcome, ToolError> {
    let p = str_arg(args, &["path", "file"]);
    let content = str_arg(args, &["content", "text"]);
    let abs = resolve(sc, &p)?;
    if let Some(dir) = abs.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&abs, content)?;
    Ok(Outcome { output: format!("wrote {}", abs.display()), truncated: false })
}

fn do_edit(sc: &SecCtx, args: &HashMap<String, serde_json::Value>) -> Result<Outcome, ToolError> {
    let p = str_arg(args, &["path", "file"]);
    let old = str_arg(args, &["old_string", "oldString", "old"]);
    let new = str_arg(args, &["new_string", "newString", "new"]);
    if p.is_empty() || old.is_empty() {
        return Err(ToolError::Msg("edit: missing path/old_string".into()));
    }
    let abs = resolve(sc, &p)?;
    let text = std::fs::read_to_string(&abs).map_err(|_| ToolError::Msg(format!("edit: no such file {p}")))?;
    if !text.contains(&old) {
        return Err(ToolError::Msg("edit: old_string not found".into()));
    }
    let updated = if is_truthy(args.get("replaceAll")) || is_truthy(args.get("replace_all")) {
        text.replace(&old, &new)
    } else {
        let matches = text.matches(&old).count();
        if matches > 1 {
            return Err(ToolError::Msg(format!(
                "edit: old_string matches {matches} locations; add context or set replaceAll:true"
            )));
        }
        text.replacen(&old, &new, 1)
    };
    std::fs::write(&abs, updated)?;
    Ok(Outcome { output: format!("edited {}", abs.display()), truncated: false })
}

fn do_bash(reg: &Registry, sc: &SecCtx, args: &HashMap<String, serde_json::Value>) -> Result<Outcome, ToolError> {
    let cmd_str = str_arg(args, &["command", "cmd"]);
    if cmd_str.trim().is_empty() {
        return Err(ToolError::Msg("bash: empty command".into()));
    }
    // workdir runs in the workspace or a whitelisted external dir.
    let mut dir = sc.workspace.clone();
    if !str_arg(args, &["workdir", "work_dir", "cwd"]).is_empty() {
        let w = str_arg(args, &["workdir", "work_dir", "cwd"]);
        let expanded = crate::security::expand_home(&w);
        let resolved = if std::path::Path::new(&expanded).is_absolute() {
            PathBuf::from(&expanded)
        } else {
            sc.workspace.join(&expanded)
        };
        if !resolved.starts_with(&sc.workspace) && !crate::security::is_whitelisted_read(&resolved.to_string_lossy()) {
            return Err(ToolError::Denied("bash: workdir escapes workspace".to_string()));
        }
        if !resolved.is_dir() {
            return Err(ToolError::Msg(format!("bash: workdir is not a directory: {w}")));
        }
        dir = resolved;
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
    // Background: no timeout unless one is supplied. Foreground keeps
    // the 2-minute cap when timeout<=0 (doBash parity).
    let bg = is_truthy(args.get("background"));
    let timeout = {
        let ms = int_arg_any(args, &["timeout", "timeout_ms", "timeoutMs"], 0);
        if ms > 0 {
            Some(Duration::from_millis(ms))
        } else if bg {
            None
        } else {
            Some(Duration::from_secs(120))
        }
    };
    if bg {
        let mut label = cmd_str.clone();
        if label.len() > 80 {
            truncate_to(&mut label, 80);
            label.push('…');
        }
        let id = new_task(&sc.session, "bash", &label);
        let slot = task_slot(&id);
        let session = sc.session.clone();
        let id2 = id.clone();
        let sandbox = sc.sandbox.clone();
        let dir2 = dir.clone();
        let ws = sc.workspace.clone();
        std::thread::spawn(move || {
            let out = match exec_bash(&cmd_str, &dir2, &session, &extra_env, timeout, false, &ws, &sandbox) {
                Ok(s) => s,
                Err(e) => format!("error: {e}"),
            };
            finish_task(&id2, &slot, &out);
        });
        return Ok(Outcome { output: format!("started background task {id} (poll with task_output)"), truncated: false });
    }
    exec_bash(&cmd_str, &dir, &sc.session, &extra_env, timeout, true, &sc.workspace, &sc.sandbox).map(cap)
}

/// Synchronous shell run shared by foreground `bash` and background tasks.
/// ponytail: std Command + scoped kill via wait_timeout emulation (poll loop).
fn exec_bash(
    cmd_str: &str,
    workspace: &Path,
    session: &str,
    extra_env: &[(String, String)],
    timeout: Option<Duration>,
    kill_on_stop: bool,
    sandbox_root: &Path,
    sandbox: &crate::sandbox::Profile,
) -> Result<String, ToolError> {
    // Per-session TMPDIR hygiene (cleaned on session.deleted). Ensure the
    // dir exists so tools (and test suites run via bash) can use it.
    let tmpdir = std::env::temp_dir().join(format!("lumina-ses-{session}"));
    let _ = std::fs::create_dir_all(&tmpdir);
    let mut cmd = if sandbox.enabled {
        // Fail closed: no silent unsandboxed fallback. The workspace root
        // for binds is the session workspace (the fence already confined
        // `dir` inside it or an explicit allowlist).
        crate::sandbox::wrap(sandbox, sandbox_root, workspace, &tmpdir, cmd_str).map_err(ToolError::Msg)?
    } else {
        let mut cmd = std::process::Command::new("sh");
        cmd.arg("-c").arg(cmd_str).current_dir(workspace);
        cmd
    };
    cmd.env("TMPDIR", &tmpdir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    for (k, v) in extra_env {
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
                    if text.trim().is_empty() {
                        return Err(ToolError::Msg(format!(
                            "bash: command failed with exit {}",
                            status.code().unwrap_or(-1)
                        )));
                    }
                    text.push_str(&format!("\n[exit {}]", status.code().unwrap_or(-1)));
                }
                return Ok(text);
            }
            None if timeout.is_some_and(|t| start.elapsed() > t) => {
                let _ = child.kill();
                return Err(ToolError::Msg(format!("bash: timed out after {:?}", timeout.unwrap())));
            }
            // Cooperative stop lands mid-tool: kill the child so the
            // turn ends now instead of after the command finishes.
            // Foreground only — background tasks outlive the turn.
            None if kill_on_stop && stop_requested(session) => {
                let _ = child.kill();
                return Err(ToolError::Msg("bash: stopped by user".into()));
            }
            None => std::thread::sleep(Duration::from_millis(25)),
        }
    }
}

/// Background tasks (`bash background:true`, `subagent background:true`):
/// daemon-lifetime registry polled via `task_output`. Mirrors Go's
/// Registry.tasks map + `task-N` ids (global here: Registry is per-turn).
struct TaskState {
    done: bool,
    output: String,
}

/// One background task: owning session + kind/label + shared completion slot.
struct TaskEntry {
    session: String,
    kind: String,
    label: String,
    slot: Arc<Mutex<TaskState>>,
}

/// Fires when a background task finishes (OnBackgroundDone parity).
/// Daemon-lifetime: the server installs a broker hook at startup, headless
/// `run` installs a stderr hook; unset in tests.
pub type BackgroundHook = Arc<dyn Fn(&str, &str, &str, &str) + Send + Sync>;

fn background_hook() -> &'static Mutex<Option<BackgroundHook>> {
    use std::sync::OnceLock;
    static HOOK: OnceLock<Mutex<Option<BackgroundHook>>> = OnceLock::new();
    HOOK.get_or_init(|| Mutex::new(None))
}

/// Install the process-wide background-completion hook.
pub fn set_background_hook(hook: BackgroundHook) {
    *background_hook().lock().unwrap() = Some(hook);
}

fn notify_background_done(session: &str, id: &str, kind: &str, label: &str) {
    if let Some(h) = background_hook().lock().unwrap().clone() {
        h(session, id, kind, label);
    }
}

/// Cooperative-stop probe: foreground `bash` kills the child when the
/// session's stop flag is set, so the web Stop button lands in ~ms
/// instead of after the tool finishes (a `sleep 60` used to eat the
/// stop). Daemon-only: the server installs it at startup from its
/// `Arc<Store>`; unset in headless `run` and tests (prior behavior).
pub type StopHook = Arc<dyn Fn(&str) -> bool + Send + Sync>;

fn stop_hook() -> &'static Mutex<Option<StopHook>> {
    use std::sync::OnceLock;
    static HOOK: OnceLock<Mutex<Option<StopHook>>> = OnceLock::new();
    HOOK.get_or_init(|| Mutex::new(None))
}

/// Install the process-wide stop probe.
pub fn set_stop_hook(hook: StopHook) {
    *stop_hook().lock().unwrap() = Some(hook);
}

fn stop_requested(session: &str) -> bool {
    stop_hook().lock().unwrap().as_ref().is_some_and(|h| h(session))
}

fn task_registry() -> &'static Mutex<HashMap<String, TaskEntry>> {
    use std::sync::OnceLock;
    static TASKS: OnceLock<Mutex<HashMap<String, TaskEntry>>> = OnceLock::new();
    TASKS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn new_task(session: &str, kind: &str, label: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let id = format!("task-{}", SEQ.fetch_add(1, Ordering::Relaxed) + 1);
    let slot = Arc::new(Mutex::new(TaskState { done: false, output: String::new() }));
    task_registry().lock().unwrap().insert(
        id.clone(),
        TaskEntry { session: session.into(), kind: kind.into(), label: label.into(), slot },
    );
    id
}

fn task_slot(id: &str) -> Arc<Mutex<TaskState>> {
    task_registry().lock().unwrap().get(id).map(|e| e.slot.clone()).unwrap()
}

fn finish_task(id: &str, slot: &Arc<Mutex<TaskState>>, output: &str) {
    {
        let mut st = slot.lock().unwrap();
        st.done = true;
        st.output = output.to_string();
    }
    if let Some(e) = task_registry().lock().unwrap().get(id) {
        notify_background_done(&e.session.clone(), id, &e.kind.clone(), &e.label.clone());
    }
}

fn do_task_output(args: &HashMap<String, serde_json::Value>) -> Result<Outcome, ToolError> {
    let id = str_arg(args, &["task_id", "id", "taskId"]);
    if id.is_empty() {
        return Err(ToolError::Msg("task_output: missing task_id".into()));
    }
    let reg = task_registry().lock().unwrap();
    let Some(entry) = reg.get(&id) else {
        return Err(ToolError::Msg(format!("task_output: unknown task {id:?}")));
    };
    let (kind, label, slot) = (entry.kind.clone(), entry.label.clone(), entry.slot.clone());
    drop(reg);
    let st = slot.lock().unwrap();
    if !st.done {
        return Ok(Outcome { output: format!("task {id} ({kind}) still running: {label}"), truncated: false });
    }
    Ok(cap(st.output.clone()))
}

fn is_truthy(v: Option<&serde_json::Value>) -> bool {
    match v {
        Some(serde_json::Value::Bool(b)) => *b,
        Some(serde_json::Value::String(s)) => matches!(s.to_lowercase().as_str(), "true" | "1" | "yes"),
        Some(serde_json::Value::Number(n)) => n.as_u64().unwrap_or(0) != 0,
        _ => false,
    }
}

fn do_grep(sc: &SecCtx, args: &HashMap<String, serde_json::Value>) -> Result<Outcome, ToolError> {
    let pattern = str_arg(args, &["pattern", "query"]);
    if pattern.is_empty() {
        return Err(ToolError::Msg("grep: missing pattern".into()));
    }
    // Optional path base: workspace or the external read allowlist.
    let mut root = sc.workspace.clone();
    if !str_arg(args, &["path"]).is_empty() {
        let p = str_arg(args, &["path"]);
        let cand = if std::path::Path::new(&p).is_absolute() { PathBuf::from(&p) } else { sc.workspace.join(&p) };
        if !sc.workspace.as_os_str().is_empty()
            && !cand.starts_with(&sc.workspace)
            && !crate::security::is_whitelisted_read(&cand.to_string_lossy())
        {
            return Err(ToolError::Denied("grep: path escapes workspace".into()));
        }
        root = cand;
    }
    if root.as_os_str().is_empty() {
        return Err(ToolError::Msg("grep: missing pattern/path".into()));
    }
    // literal:true matches exact text; otherwise pattern is a regexp.
    let literal = is_truthy(args.get("literal"));
    let mut case_sensitive = true;
    if args.contains_key("caseSensitive") {
        case_sensitive = is_truthy(args.get("caseSensitive"));
    } else if args.contains_key("case_sensitive") {
        case_sensitive = is_truthy(args.get("case_sensitive"));
    }
    let re = if !literal {
        let expr = if case_sensitive { pattern.clone() } else { format!("(?i){pattern}") };
        Some(regex::Regex::new(&expr).map_err(|e| ToolError::Msg(format!("grep: invalid pattern: {e}")))?)
    } else {
        None
    };
    let include = str_arg(args, &["include"]);
    let include_pat = if include.is_empty() { None } else { glob::Pattern::new(&include).ok() };
    let mut limit = int_arg_any(args, &["limit"], 100) as usize;
    if limit < 1 {
        limit = 100;
    }
    if limit > 500 {
        limit = 500;
    }
    let needle = if case_sensitive { pattern.clone() } else { pattern.to_lowercase() };
    let mut out: Vec<String> = Vec::new();
    // ponytail: small recursive walker, no ignore-crate until it earns its keep.
    fn walk(dir: &Path, cb: &mut dyn FnMut(&Path)) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                let s = p.to_string_lossy();
                if s.contains("/.git/") || s.contains("/node_modules/") {
                    continue;
                }
                walk(&p, cb);
            } else {
                cb(&p);
            }
        }
    }
    let ws = sc.workspace.clone();
    walk(&root, &mut |p| {
        if out.len() >= limit {
            return;
        }
        let s = p.to_string_lossy();
        if s.contains("/.git/") || s.contains("/node_modules/") {
            return;
        }
        // Workspace secrets are never indexed; external whitelist allows dotfiles.
        if crate::security::is_credential_path(&s) && p.starts_with(&ws) {
            return;
        }
        if p.metadata().map(|m| m.len() > 1 << 20).unwrap_or(true) {
            return;
        }
        let rel = if ws.as_os_str().is_empty() {
            s.to_string()
        } else {
            p.strip_prefix(&ws).map(|r| r.to_string_lossy().into_owned()).unwrap_or_else(|_| s.to_string())
        };
        if let Some(pat) = &include_pat {
            // Match against the relative path and the base name so
            // "*.go" works at any depth.
            let base = p.file_name().map(|b| b.to_string_lossy().into_owned()).unwrap_or_default();
            if !pat.matches(&rel) && !pat.matches(&base) {
                return;
            }
        }
        let Ok(b) = std::fs::read(p) else { return };
        if b.contains(&0) {
            return; // skip binaries
        }
        let text = String::from_utf8_lossy(&b);
        for (i, line) in text.split('\n').enumerate() {
            let hit = if let Some(re) = &re {
                re.is_match(line)
            } else if case_sensitive {
                line.contains(&pattern)
            } else {
                line.to_lowercase().contains(&needle)
            };
            if hit {
                out.push(format!("{rel}:{}:{line}", i + 1));
                if out.len() >= limit {
                    break;
                }
            }
        }
    });
    Ok(Outcome { output: out.join("\n"), truncated: out.len() >= limit })
}

/// Semantic code intelligence via the workspace language server
/// (`src/lsp.rs`): `op` is `symbols | definition | references |
/// diagnostics`, `path` a workspace-relative file, `line`/`character`
/// 1-based (0/0 = start of file), `query` an optional symbol-name filter.
/// Credential files are refused like `read` (servers get full file text).
fn do_lsp(reg: &Registry, sc: &SecCtx, args: &HashMap<String, serde_json::Value>) -> Result<Outcome, ToolError> {
    let f = match &reg.lsp_query {
        Some(f) => f,
        None => return Err(ToolError::Msg("lsp: no language-server manager (agent wires one per turn)".into())),
    };
    let op = str_arg(args, &["op"]);
    let path = str_arg(args, &["path", "file", "target"]);
    if path.is_empty() {
        return Err(ToolError::Msg("lsp: missing `path` (workspace-relative file)".into()));
    }
    if crate::security::is_credential_path(&path) {
        return Err(ToolError::Denied("lsp: credential file".into()));
    }
    let line = int_arg_any(args, &["line", "row"], 0);
    let character = int_arg_any(args, &["character", "col", "column"], 0);
    let filter = str_arg(args, &["query", "filter", "pattern"]);
    let text = f(&sc.workspace, &op, &path, line, character, &filter).map_err(ToolError::Msg)?;
    Ok(Outcome { output: text, truncated: false })
}

fn do_glob(sc: &SecCtx, args: &HashMap<String, serde_json::Value>) -> Result<Outcome, ToolError> {
    let pattern = str_arg(args, &["pattern", "glob"]);
    if pattern.is_empty() {
        return Err(ToolError::Msg("glob: missing pattern".into()));
    }
    // Optional path base: workspace or the external read allowlist.
    let mut root = sc.workspace.clone();
    if !str_arg(args, &["path"]).is_empty() {
        let p = str_arg(args, &["path"]);
        let cand = if std::path::Path::new(&p).is_absolute() { PathBuf::from(&p) } else { sc.workspace.join(&p) };
        if !sc.workspace.as_os_str().is_empty()
            && !cand.starts_with(&sc.workspace)
            && !crate::security::is_whitelisted_read(&cand.to_string_lossy())
        {
            return Err(ToolError::Denied("glob: path escapes workspace".into()));
        }
        root = cand;
    }
    let mut limit = int_arg_any(args, &["limit"], 100) as usize;
    if limit < 1 {
        limit = 100;
    }
    if limit > 1000 {
        limit = 1000;
    }
    let ws = sc.workspace.clone();
    let mut rel: Vec<String> = Vec::new();
    if pattern.contains("**") {
        // Walk + glob-pattern match (globToRegExp parity via the glob crate).
        let pat = glob::Pattern::new(&pattern).map_err(|e| ToolError::Msg(format!("glob: bad pattern: {e}")))?;
        fn walk2(dir: &Path, cb: &mut dyn FnMut(&Path)) {
            let Ok(rd) = std::fs::read_dir(dir) else { return };
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    let s = p.to_string_lossy();
                    if s.contains("/.git/") || s.contains("/node_modules/") {
                        continue;
                    }
                    walk2(&p, cb);
                } else {
                    cb(&p);
                }
            }
        }
        walk2(&root, &mut |p| {
            if rel.len() >= limit {
                return;
            }
            let Ok(rp) = p.strip_prefix(&root) else { return };
            let rps = rp.to_string_lossy().replace('\\', "/");
            if rps.is_empty() {
                return;
            }
            if crate::security::is_credential_path(&p.to_string_lossy()) && p.starts_with(&ws) {
                return; // workspace secrets hidden; external whitelist lists all dotfiles
            }
            if pat.matches(&rps) {
                rel.push(rps);
            }
        });
    } else {
        let mut hits: Vec<String> = glob::glob(root.join(&pattern).to_str().unwrap_or(""))
            .map_err(|e| ToolError::Msg(format!("glob: {e}")))?
            .flatten()
            .filter(|p| {
                !(crate::security::is_credential_path(&p.to_string_lossy()) && p.starts_with(&ws))
            })
            .filter_map(|p| p.strip_prefix(&root).ok().map(|r| r.display().to_string()))
            .collect();
        hits.sort();
        rel = hits;
    }
    let truncated = rel.len() > limit;
    if truncated {
        rel.truncate(limit);
    }
    Ok(Outcome { output: if rel.is_empty() { "(no matches)".into() } else { rel.join("\n") }, truncated })
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

fn do_memory_write(reg: &Registry, sc: &SecCtx, args: &HashMap<String, serde_json::Value>) -> Result<Outcome, ToolError> {
    let topic = str_arg(args, &["topic", "name"]);
    let content = str_arg(args, &["content", "body"]);
    let ov = if reg.kb_memory_dir.trim().is_empty() { None } else { Some(reg.kb_memory_dir.as_str()) };
    crate::kb::memory_write_in(&sc.workspace, &topic, &content, ov)
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
    let qs = parse_questions(args)?;
    // Headless runs guide the model to proceed with best guesses (Go parity).
    if !sc.gate.is_live() {
        return Ok(Outcome { output: unwired_question_guidance(&qs), truncated: false });
    }
    match sc.gate.ask_question(&sc.session, qs.clone()) {
        Ok(answers) => Ok(Outcome { output: format_question_answers(&qs, &answers), truncated: false }),
        Err(e) => {
            // Dismissals and transport failures reach the model as guidance
            // to continue without the answers, never as a stuck turn.
            Err(ToolError::Msg(format!("question: {e}; proceed with your best guess and note the assumption")))
        }
    }
}

/// {question, header, options: [{label, description}], multiple}).
/// Bare strings ("q1") and legacy {text, options:[string]} also parse.
fn parse_questions(args: &HashMap<String, serde_json::Value>) -> Result<Vec<crate::approve::Question>, ToolError> {
    let raw = args.get("questions").cloned().unwrap_or(serde_json::Value::Null);
    let list = raw.as_array().cloned().unwrap_or_default();
    if list.is_empty() {
        return Err(ToolError::Msg("question: missing questions".into()));
    }
    let mut qs = Vec::new();
    for (i, item) in list.iter().enumerate() {
        if let Some(s) = item.as_str() {
            if s.is_empty() {
                continue;
            }
            qs.push(crate::approve::Question {
                question: s.trim().to_string(),
                header: String::new(),
                options: vec![],
                multiple: false,
            });
            continue;
        }
        let Some(m) = item.as_object() else {
            return Err(ToolError::Msg(format!("question: questions[{i}] must be {{question, header, options}}")));
        };
        let get = |k: &str| m.get(k).and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
        let question = {
            let q = get("question");
            if q.is_empty() { get("text") } else { q }
        };
        if question.is_empty() {
            return Err(ToolError::Msg(format!("question: questions[{i}].question is required")));
        }
        let mut options = Vec::new();
        if let Some(arr) = m.get("options").and_then(|o| o.as_array()) {
            for (j, o) in arr.iter().enumerate() {
                if let Some(label) = o.as_str() {
                    // Legacy string options.
                    if label.is_empty() {
                        continue;
                    }
                    options.push(crate::approve::QuestionOption { label: label.into(), description: String::new() });
                    continue;
                }
                let Some(om) = o.as_object() else {
                    return Err(ToolError::Msg(format!(
                        "question: questions[{i}].options[{j}] must be {{label, description}}"
                    )));
                };
                let label = om.get("label").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
                if label.is_empty() {
                    return Err(ToolError::Msg(format!(
                        "question: questions[{i}].options[{j}].label is required"
                    )));
                }
                options.push(crate::approve::QuestionOption {
                    label,
                    description: om.get("description").and_then(|v| v.as_str()).unwrap_or("").trim().to_string(),
                });
            }
        }
        qs.push(crate::approve::Question {
            question,
            header: get("header"),
            options,
            multiple: m.get("multiple").and_then(|v| v.as_bool()).unwrap_or(false),
        });
    }
    if qs.is_empty() {
        return Err(ToolError::Msg("question: missing questions".into()));
    }
    Ok(qs)
}

fn format_question_answers(qs: &[crate::approve::Question], answers: &[Vec<String>]) -> String {
    let mut parts = Vec::new();
    for (i, q) in qs.iter().enumerate() {
        let a: Vec<String> = answers.get(i).cloned().unwrap_or_default();
        let a = if a.is_empty() { vec!["Unanswered".to_string()] } else { a };
        parts.push(format!("{:?}={:?}", q.question, a.join(", ")));
    }
    format!(
        "User has answered your questions: {}. You can now continue with the user's answers in mind.",
        parts.join(", ")
    )
}

fn unwired_question_guidance(qs: &[crate::approve::Question]) -> String {
    let names: Vec<&str> = qs.iter().map(|q| q.question.as_str()).collect();
    format!(
        "question UI is not wired in this session; proceed with your best guess for ({}) and note the assumption",
        names.join("; ")
    )
}

fn do_websearch(reg: &Registry, args: &HashMap<String, serde_json::Value>) -> Result<Outcome, ToolError> {
    if reg.websearch_disabled {
        return Err(ToolError::Msg("websearch: disabled by configuration (websearch = false)".into()));
    }
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
    let timeout_ms = int_arg_any(args, &["timeout", "timeout_ms", "timeoutMs"], 30_000).min(120_000);
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
    let max = subagent_max_depth();
    if sc.depth >= max {
        return Err(ToolError::Msg(format!("subagent: nesting depth {max} exceeded (subagents cannot launch subagents)")));
    }
    // Follow-ups target live child sessions, which do not persist:
    // fail loudly instead of silently starting an unrelated child.
    if !str_arg(args, &["sessionID", "sessionId", "session_id"]).is_empty() {
        return Err(ToolError::Msg("subagent: follow-up via sessionID is not supported; start a new subagent with full context instead".into()));
    }
    let task = str_arg(args, &["task", "prompt"]);
    if task.is_empty() {
        return Err(ToolError::Msg("subagent: missing task".into()));
    }
    let name = str_arg(args, &["agent", "name"]);
    let name = if name.is_empty() { "general" } else { name.as_str() };
    let req = SubagentReq {
        task: task.clone(),
        agent: name.into(),
        description: str_arg(args, &["description"]),
        model: str_arg(args, &["model"]),
    };
    let Some(f) = &reg.subagent else {
        return Err(ToolError::Msg("subagent: executor not attached".into()));
    };
    if is_truthy(args.get("background")) {
        // Detached run: the sync executor parks on a worker thread via
        // block_in_place, so drive it from a spawned async task and poll
        // with task_output.
        let id = new_task(&sc.session, "subagent", &req.task);
        let slot = task_slot(&id);
        let f = f.clone();
        let sc = sc.clone();
        let id2 = id.clone();
        tokio::spawn(async move {
            let out = match f(&sc, &req) {
                Ok(s) => s,
                Err(e) => format!("error: {e}"),
            };
            finish_task(&id2, &slot, &out);
        });
        return Ok(Outcome { output: format!("started background task {id} (poll with task_output)"), truncated: false });
    }
    f(sc, &req)
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
    let arr = raw.as_array().cloned().unwrap_or_default();
    let mut list = Vec::new();
    for (i, item) in arr.iter().enumerate() {
        let Some(m) = item.as_object() else {
            return Err(ToolError::Msg(format!("todowrite: todos[{i}] must be {{content, status}}")));
        };
        let content = m.get("content").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
        let status = m.get("status").and_then(|v| v.as_str()).unwrap_or("").trim().to_lowercase();
        if !matches!(status.as_str(), "pending" | "in_progress" | "completed" | "cancelled") {
            return Err(ToolError::Msg(format!(
                "todowrite: todos[{i}].status must be pending|in_progress|completed|cancelled"
            )));
        }
        if content.is_empty() {
            return Err(ToolError::Msg(format!("todowrite: todos[{i}].content is required")));
        }
        list.push((content, status));
    }
    if list.is_empty() {
        store.lock().unwrap().remove(&sc.session);
        return Ok(Outcome { output: "Todos cleared".into(), truncated: false });
    }
    let count = |s: &str| list.iter().filter(|(_, st)| st == s).count();
    let mut parts = vec![format!("{}/{} completed", count("completed"), list.len())];
    for s in ["in_progress", "pending", "cancelled"] {
        let n = count(s);
        if n > 0 {
            parts.push(format!("{n} {}", s.replace('_', " ")));
        }
    }
    let mut out = format!("Todos updated: {}", parts.join(", "));
    let active: Vec<&str> = list.iter().filter(|(_, st)| st == "in_progress").map(|(c, _)| c.as_str()).collect();
    if !active.is_empty() {
        out.push_str(&format!(". In progress: {}", active.join(", ")));
    }
    store.lock().unwrap().insert(sc.session.clone(), list);
    Ok(Outcome { output: out, truncated: false })
}

fn do_patch(sc: &SecCtx, args: &HashMap<String, serde_json::Value>) -> Result<Outcome, ToolError> {
    let diff = str_arg(args, &["patchText", "patch_text", "patch", "diff"]);
    if diff.trim().is_empty() {
        return Err(ToolError::Msg("patch: missing patchText (wrap operations in *** Begin Patch *** / *** End Patch ***)".into()));
    }
    let plan = sc.agent == Agent::Plan;
    // Primary envelope: *** Begin Patch *** (Add/Update/Delete/Move).
    if crate::begin_patch::is_begin_patch(&diff) {
        let out = crate::begin_patch::apply_begin_patch(&sc.workspace, plan, &diff)
            .map_err(|e| ToolError::Msg(e.to_string()))?;
        return Ok(cap(out));
    }
    let files = parse_unified(&diff)?;
    let mut edited = Vec::new();
    for (rel, hunks) in files {
        let abs = resolve(sc, &rel)?;
        if crate::security::is_credential_path(&abs.to_string_lossy()) {
            return Err(ToolError::Denied(format!("patch: credential path denied: {rel}")));
        }
        if plan && !crate::security::plan_allows_write(&sc.workspace, &abs.to_string_lossy()) {
            return Err(ToolError::Denied(format!(
                "patch: plan-mode agents may only change plan.md or .lumina/plans/*.md (got {rel})"
            )));
        }
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


fn first_patch_path(diff: &str) -> String {
    // Begin-Patch headers first, then unified +++ (Go patchFirstPath parity).
    let bp = crate::begin_patch::patch_first_path(diff);
    if bp != "patch" {
        return bp;
    }
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
        SecCtx { workspace: dir.to_path_buf(), session: "t".into(), agent, mode, headless: true, gate: ApprovalGate::default(), bash_rules: HashMap::new(), allow_tooling_write: false, depth: 0, sandbox: Default::default() }
    }

    fn args(pairs: &[(&str, &str)]) -> HashMap<String, serde_json::Value> {
        pairs.iter().map(|(k, v)| ((*k).into(), serde_json::Value::String((*v).into()))).collect()
    }

    #[test]
    fn read_write_roundtrip() {
        let d = tempfile::tempdir().unwrap();
        let reg = Registry::new();
        let sc = ctx(d.path(), Agent::Build, PermissionMode::Bypass);
        reg.call(&sc, "write", &args(&[("path", "a.txt"), ("content", "hi\nthere")])).unwrap();
        let out = reg.call(&sc, "read", &args(&[("path", "a.txt")])).unwrap();
        assert_eq!(out.output, "1: hi\n2: there");
        assert!(!out.truncated);
        // Paged slice.
        let out = reg.call(&sc, "read", &args(&[("path", "a.txt"), ("offset", "2"), ("limit", "1")])).unwrap();
        assert_eq!(out.output, "2: there");
        // Absolute write path in output (Go parity).
        let out = reg.call(&sc, "write", &args(&[("path", "b.txt"), ("content", "x")])).unwrap();
        assert!(out.output.ends_with("b.txt"), "{}", out.output);
        // Directory listing skips workspace credential files.
        std::fs::write(d.path().join(".env"), "K=x").unwrap();
        let out = reg.call(&sc, "read", &args(&[("path", ".")])).unwrap();
        assert!(out.output.contains("a.txt"), "{}", out.output);
        assert!(!out.output.contains(".env"), "{}", out.output);
        // replaceAll replaces every occurrence; ambiguous single errors.
        reg.call(&sc, "write", &args(&[("path", "c.txt"), ("content", "a a a")])).unwrap();
        let err = reg.call(&sc, "edit", &args(&[("path", "c.txt"), ("old_string", "a"), ("new_string", "b")])).unwrap_err();
        assert!(matches!(err, ToolError::Msg(_)), "{err:?}");
        let mut m = args(&[("path", "c.txt"), ("old_string", "a"), ("new_string", "b")]);
        m.insert("replaceAll".into(), serde_json::Value::Bool(true));
        reg.call(&sc, "edit", &m).unwrap();
        let out = reg.call(&sc, "read", &args(&[("path", "c.txt")])).unwrap();
        assert_eq!(out.output, "1: b b b");
    }

    #[test]
    fn grep_literal_case_and_include() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.go"), "Hello World\nfoo bar\n").unwrap();
        std::fs::write(d.path().join("b.txt"), "hello world\n").unwrap();
        let reg = Registry::new();
        let sc = ctx(d.path(), Agent::Build, PermissionMode::Bypass);
        // Case-insensitive regex.
        let mut m = args(&[("pattern", "hello")]);
        m.insert("case_sensitive".into(), serde_json::Value::Bool(false));
        let out = reg.call(&sc, "grep", &m).unwrap();
        assert!(out.output.contains("a.go:1:"), "{}", out.output);
        assert!(out.output.contains("b.txt:1:"), "{}", out.output);
        // include limits to matching basenames at any depth.
        let mut m = args(&[("pattern", "hello"), ("include", "*.go")]);
        m.insert("case_sensitive".into(), serde_json::Value::Bool(false));
        let out = reg.call(&sc, "grep", &m).unwrap();
        assert!(out.output.contains("a.go"), "{}", out.output);
        assert!(!out.output.contains("b.txt"), "{}", out.output);
        // literal matches exact text.
        let mut m = args(&[("pattern", "Hello (World")]);
        let out = reg.call(&sc, "grep", &m);
        assert!(out.is_err(), "paren is invalid regex without literal");
        m.insert("literal".into(), serde_json::Value::Bool(true));
        let out = reg.call(&sc, "grep", &m).unwrap();
        assert!(out.output.is_empty() || !out.output.contains("a.go"), "{}", out.output);
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
        assert!(!is_destructive("cargo test"));
    }

    #[test]
    fn escape_denied() {
        let d = tempfile::tempdir().unwrap();
        let reg = Registry::new();
        let sc = ctx(d.path(), Agent::Build, PermissionMode::Bypass);
        let err = reg.call(&sc, "read", &args(&[("path", "../secret")])).unwrap_err();
        assert!(matches!(err, ToolError::Denied(_)), "{err}");
    }

    #[test]
    fn sandbox_disabled_by_default_runs_direct() {
        // No [sandbox] section: plain `sh -c`, byte-for-byte old behavior.
        let d = tempfile::tempdir().unwrap();
        let sc = ctx(d.path(), Agent::Build, PermissionMode::Bypass);
        assert!(!sc.sandbox.enabled);
        let out = exec_bash("echo hi", d.path(), "t", &[], Some(Duration::from_secs(10)), true, d.path(), &sc.sandbox).unwrap();
        assert!(out.contains("hi"), "{out}");
    }

    #[test]
    fn sandbox_missing_binary_fails_closed() {
        // Enabled but no binary: hard error, never a silent direct run.
        let d = tempfile::tempdir().unwrap();
        let prof = crate::sandbox::Profile { enabled: true, network: false, command: "definitely-not-bwrap-xyz".into() };
        let err = exec_bash("echo hi", d.path(), "t", &[], Some(Duration::from_secs(10)), true, d.path(), &prof).unwrap_err();
        let s = err.to_string();
        assert!(s.contains("No such file") || s.contains("not found"), "{s}");
    }

    #[test]
    fn sandbox_live_confines_home() {
        // Where user namespaces work, `ls $HOME` must fail inside the
        // sandbox (HOME is deliberately unbound). Where they don't
        // (locked-down containers), bwrap fails loudly — that path is a
        // skip (structural fail-closed is proven by
        // missing_binary_fails_closed). This test fails ONLY on
        // demonstrated leakage: a successful home listing.
        let d = tempfile::tempdir().unwrap();
        let prof = crate::sandbox::Profile { enabled: true, network: false, command: "bwrap".into() };
        let out = exec_bash("ls \"$HOME\"", d.path(), "t-live", &[], Some(Duration::from_secs(30)), true, d.path(), &prof);
        match out {
            Ok(text) => {
                assert!(text.contains("[exit ") || text.to_lowercase().contains("no such file"), "HOME leaked into sandbox: {text}");
            }
            Err(e) => {
                eprintln!("skip: sandbox unavailable here ({e})");
            }
        }
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
            sandbox: Default::default(),
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
    fn question_schema_and_unwired_guidance() {
        let d = tempfile::tempdir().unwrap();
        let reg = Registry::new();
        let sc = ctx2(d.path());
        // Headless (no live gate): guidance output, not an error.
        let mut q = HashMap::new();
        q.insert(
            "questions".into(),
            serde_json::json!([
                {"question": "pick one?", "header": "h", "options": [{"label": "a", "description": "first"}, {"label": "b"}], "multiple": true},
                "bare shorthand",
            ]),
        );
        let out = reg.call(&sc, "question", &q).unwrap();
        assert!(out.output.contains("best guess"), "{}", out.output);
        assert!(out.output.contains("pick one?"), "{}", out.output);
        let qs = parse_questions(&q).unwrap();
        assert_eq!(qs.len(), 2);
        assert!(qs[0].multiple && qs[0].options[0].description == "first");
        assert_eq!(qs[1].question, "bare shorthand");
        let s = format_question_answers(&qs, &[vec!["a".into(), "b".into()]]);
        assert!(s.contains("\"pick one?\"=\"a, b\""), "{s}");
        assert!(s.contains("\"bare shorthand\"=\"Unanswered\""), "{s}");
        // Missing question text errors.
        let mut bad = HashMap::new();
        bad.insert("questions".into(), serde_json::json!([{"header": "h"}]));
        assert!(reg.call(&sc, "question", &bad).is_err());
    }

    #[test]
    fn task_output_unknown_and_background_bash() {
        let d = tempfile::tempdir().unwrap();
        let reg = Registry::new();
        let sc = ctx2(d.path());
        // Unknown id fails closed.
        let err = reg.call(&sc, "task_output", &args(&[("task_id", "task-999999")])).unwrap_err();
        assert!(matches!(err, ToolError::Msg(_)), "{err:?}");
        // Missing id fails closed.
        assert!(reg.call(&sc, "task_output", &HashMap::new()).is_err());
        // Background bash returns a pollable id; output arrives after finish.
        let mut bg = HashMap::new();
        bg.insert("command".into(), serde_json::Value::String("echo hello-bg".into()));
        bg.insert("background".into(), serde_json::Value::Bool(true));
        let out = reg.call(&sc, "bash", &bg).unwrap();
        assert!(out.output.contains("task-"), "{}", out.output);
        assert!(out.output.contains("task_output"), "{}", out.output);
        let id = out.output.split_whitespace().nth(3).unwrap().to_string();
        // Poll until done (fast command; bounded wait).
        let mut got = String::new();
        for _ in 0..100 {
            let r = reg.call(&sc, "task_output", &args(&[("task_id", &id)])).unwrap();
            if !r.output.contains("still running") {
                got = r.output;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(got.contains("hello-bg"), "background output lost: {got}");
    }

    #[test]
    fn background_done_hook_fires() {
        use std::sync::{Arc, Mutex};
        let d = tempfile::tempdir().unwrap();
        let reg = Registry::new();
        let sc = ctx2(d.path());
        let fired: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(vec![]));
        let f2 = fired.clone();
        set_background_hook(Arc::new(move |sess: &str, id: &str, kind: &str, label: &str| {
            f2.lock().unwrap().push(format!("{sess}|{id}|{kind}|{label}"));
        }));
        let mut bg = HashMap::new();
        bg.insert("command".into(), serde_json::Value::String("echo hooked".into()));
        bg.insert("background".into(), serde_json::Value::Bool(true));
        let out = reg.call(&sc, "bash", &bg).unwrap();
        let id = out.output.split_whitespace().nth(3).unwrap().to_string();
        for _ in 0..100 {
            let r = reg.call(&sc, "task_output", &args(&[("task_id", &id)])).unwrap();
            if !r.output.contains("still running") {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        // Unhook so other tests stay silent.
        set_background_hook(Arc::new(|_, _, _, _| {}));
        let got = fired.lock().unwrap();
        // (Other tests may complete background tasks concurrently; match ours.)
        assert!(got.iter().any(|e| e.starts_with(&format!("t|{id}|bash|"))), "{got:?}");
        // Session-scoped: another session's inbox is untouched.
        assert!(super::task_registry().lock().unwrap().get(&id).unwrap().session == "t");
    }

    #[test]
    fn todos_roundtrip_and_validate() {
        let d = tempfile::tempdir().unwrap();
        let reg = Registry::new();
        let mut a = HashMap::new();
        a.insert("todos".into(), serde_json::json!([{"content": "x", "status": "in_progress"}]));
        let out = reg.call(&ctx2(d.path()), "todowrite", &a).unwrap();
        assert!(out.output.contains("Todos updated: 0/1 completed, 1 in progress. In progress: x"), "{}", out.output);
        let mut bad = HashMap::new();
        bad.insert("todos".into(), serde_json::json!([{"content": "x", "status": "bogus"}]));
        assert!(reg.call(&ctx2(d.path()), "todowrite", &bad).is_err());
        let mut empty = HashMap::new();
        empty.insert("todos".into(), serde_json::json!([{"content": "", "status": "pending"}]));
        assert!(reg.call(&ctx2(d.path()), "todowrite", &empty).is_err());
        let out = reg.call(&ctx2(d.path()), "todowrite", &HashMap::new()).unwrap();
        assert_eq!(out.output, "Todos cleared");
    }

    #[test]
    fn todos_progress_reaches_3_of_3() {
        // The web TodoPill parses the LATEST todowrite output with
        // `(\d+)\/(\d+) completed` and shows `In progress: …` when present:
        // a full run must end in exactly "3/3 completed" with no stale tail.
        let d = tempfile::tempdir().unwrap();
        let reg = Registry::new();
        let sc = ctx2(d.path());
        let send = |items: serde_json::Value| {
            let mut a = HashMap::new();
            a.insert("todos".into(), items);
            reg.call(&sc, "todowrite", &a).unwrap().output
        };
        let out = send(serde_json::json!([
            {"content": "a", "status": "completed"},
            {"content": "b", "status": "in_progress"},
            {"content": "c", "status": "pending"},
        ]));
        assert!(out.contains("Todos updated: 1/3 completed"), "{out}");
        assert!(out.contains("In progress: b"), "{out}");
        let out = send(serde_json::json!([
            {"content": "a", "status": "completed"},
            {"content": "b", "status": "completed"},
            {"content": "c", "status": "in_progress"},
        ]));
        assert!(out.contains("Todos updated: 2/3 completed"), "{out}");
        assert!(out.contains("In progress: c"), "{out}");
        let out = send(serde_json::json!([
            {"content": "a", "status": "completed"},
            {"content": "b", "status": "completed"},
            {"content": "c", "status": "completed"},
        ]));
        assert!(out.contains("Todos updated: 3/3 completed"), "{out}");
        assert!(!out.contains("In progress"), "{out}");
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
        // lsp without a wired manager fails closed with guidance.
        let err = reg.call(&ctx2(d.path()), "lsp", &args(&[("path", "a.txt"), ("op", "symbols")])).unwrap_err();
        assert!(err.to_string().contains("manager"), "{err}");
        // tool_target names the file and op.
        assert_eq!(tool_target("shell", &args(&[("command", "echo hi")])), "echo hi");
        assert_eq!(tool_target("lsp", &args(&[("path", "a.txt"), ("op", "symbols")])), "a.txt:symbols");
    }

    #[test]
    fn lsp_forwards_args_to_manager() {
        let d = tempfile::tempdir().unwrap();
        let mut reg = Registry::new();
        reg.lsp_query = Some(Arc::new(|ws, op, path, line, ch, filter| {
            Ok(format!("{op}|{}|{path}|{line}|{ch}|{filter}", ws.display()))
        }));
        let out = reg
            .call(
                &ctx2(d.path()),
                "lsp",
                &args(&[("op", "definition"), ("path", "a.rs"), ("line", "3"), ("character", "5"), ("query", "foo")]),
            )
            .unwrap();
        let ws = d.path().display().to_string();
        assert_eq!(out.output, format!("definition|{ws}|a.rs|3|5|foo"), "{}", out.output);
        // Credential files are refused before reaching the server.
        let err = reg
            .call(&ctx2(d.path()), "lsp", &args(&[("path", ".env"), ("op", "symbols")]))
            .unwrap_err();
        assert!(err.to_string().contains("denied"), "{err}");
    }

    #[test]
    fn truncate_to_floors_at_char_boundary() {
        // ASCII fast path: untouched.
        let mut s = "hello".to_string();
        truncate_to(&mut s, 10);
        assert_eq!(s, "hello");
        // "é" is 2 bytes: capping "aé" (3 bytes) at 2 floors to "a"
        // instead of panicking mid-char.
        let mut s = "aé".to_string();
        truncate_to(&mut s, 2);
        assert_eq!(s, "a");
        // Exact boundary keeps the char.
        let mut s = "aé".to_string();
        truncate_to(&mut s, 3);
        assert_eq!(s, "aé");
    }

    #[test]
    fn cap_multibyte_output_does_not_panic() {
        // Regression: `cap` used `String::truncate`, which panics when the
        // 256KB split lands inside a multi-byte char — killing the daemon
        // turn worker (`is_char_boundary` assertion). 262143 1-byte chars
        // + "…" (3 bytes) puts the split inside the ellipsis.
        let big = "a".repeat(262143) + "…";
        let out = cap(big);
        assert!(out.truncated);
        assert!(out.output.len() <= MAX_BYTES as usize);
    }
}
