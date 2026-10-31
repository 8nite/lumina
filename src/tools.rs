//! Core tool registry: read/edit/write/bash/grep/glob.
//! Mirrors `internal/tools` tool surface (M1: no subagents, no MCP — those
//! claim their own milestones). Every call is gated by `security::decide`.

use crate::security::{self, Agent, Decision, PermissionMode};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
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
    pub agent: Agent,
    pub mode: PermissionMode,
    /// Fail-closed for unknown tools when true (headless `run`).
    pub headless: bool,
}

#[derive(Debug, Clone)]
pub struct Outcome {
    pub output: String,
    pub truncated: bool,
}

pub struct Registry;

impl Registry {
    pub fn names() -> &'static [&'static str] {
        &["read", "edit", "write", "bash", "grep", "glob"]
    }

    pub fn call(
        &self,
        sc: &SecCtx,
        name: &str,
        args: &HashMap<String, serde_json::Value>,
    ) -> Result<Outcome, ToolError> {
        let target = tool_target(name, args);
        let destructive = name == "bash" && is_destructive(&target);
        let eval = crate::security::Eval {
            agent: sc.agent,
            mode: sc.mode,
            tool: name.into(),
            target: target.clone(),
            workspace: sc.workspace.clone(),
            destructive,
        };
        match security::decide(&eval) {
            Decision::Deny => return Err(ToolError::Denied(format!("{name} {target}"))),
            Decision::Ask if sc.headless => {
                return Err(ToolError::ApprovalRequired(format!("{name} {target}")));
            }
            // Interactive approval (TUI/Web modal) arrives in M3/M4; M1 treats
            // Ask as deny outside headless-run errors so servers never hang.
            Decision::Ask => return Err(ToolError::Denied(format!("{name} {target} (ask)"))),
            Decision::Allow => {}
        }
        match name {
            "read" => do_read(sc, args),
            "edit" => do_edit(sc, args),
            "write" => do_write(sc, args),
            "bash" => do_bash(sc, args),
            "grep" => do_grep(sc, args),
            "glob" => do_glob(sc, args),
            _ => Err(ToolError::Msg(format!("unknown tool {name}"))),
        }
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
        "bash" => s(&["command", "cmd"]),
        "grep" => s(&["pattern", "query"]),
        "glob" => s(&["pattern", "glob"]),
        _ => String::new(),
    }
}

fn is_destructive(cmd: &str) -> bool {
    let c = cmd.to_lowercase();
    c.contains("rm -rf")
        || c.contains("git push --force")
        || c.contains("git push -f")
        || c.contains("git reset --hard")
        || c.contains("mkfs")
        || c.contains(":(){")
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

fn do_bash(sc: &SecCtx, args: &HashMap<String, serde_json::Value>) -> Result<Outcome, ToolError> {
    let cmd = str_arg(args, &["command", "cmd"]);
    if cmd.trim().is_empty() {
        return Err(ToolError::Msg("bash: empty command".into()));
    }
    let timeout = args
        .get("timeout_ms")
        .and_then(|v| v.as_u64())
        .map(Duration::from_millis)
        .unwrap_or(Duration::from_secs(120));
    // ponytail: std Command + scoped kill via wait_timeout emulation (poll loop).
    let mut child = std::process::Command::new("sh")
        .arg("-c")
        .arg(&cmd)
        .current_dir(&sc.workspace)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(dir: &Path, agent: Agent, mode: PermissionMode) -> SecCtx {
        SecCtx { workspace: dir.to_path_buf(), agent, mode, headless: true }
    }

    fn args(pairs: &[(&str, &str)]) -> HashMap<String, serde_json::Value> {
        pairs.iter().map(|(k, v)| ((*k).into(), serde_json::Value::String((*v).into()))).collect()
    }

    #[test]
    fn read_write_roundtrip() {
        let d = tempfile::tempdir().unwrap();
        let reg = Registry;
        let sc = ctx(d.path(), Agent::Build, PermissionMode::Bypass);
        reg.call(&sc, "write", &args(&[("path", "a.txt"), ("content", "hi")])).unwrap();
        let out = reg.call(&sc, "read", &args(&[("path", "a.txt")])).unwrap();
        assert_eq!(out.output, "hi");
    }

    #[test]
    fn plan_cannot_edit_source() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("x.rs"), "old").unwrap();
        let reg = Registry;
        let sc = ctx(d.path(), Agent::Plan, PermissionMode::Bypass);
        let err = reg.call(&sc, "edit", &args(&[("path", "x.rs"), ("old_string", "old"), ("new_string", "new")])).unwrap_err();
        assert!(matches!(err, ToolError::Denied(_)), "{err}");
    }

    #[test]
    fn headless_ask_becomes_approval_error() {
        let d = tempfile::tempdir().unwrap();
        let reg = Registry;
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
        let reg = Registry;
        let sc = ctx(d.path(), Agent::Build, PermissionMode::Bypass);
        let err = reg.call(&sc, "read", &args(&[("path", "../secret")])).unwrap_err();
        assert!(matches!(err, ToolError::Denied(_)), "{err}");
    }
}
