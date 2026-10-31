//! Immutable security floor: plan/build structural fence + 4-mode broker.
//! Mirrors `internal/security/security.go` (M0 scope).
//!
//! Hard denies (credential files, Google targets) win over everything,
//! including `bypass`. `plan` agent is read-only except scoped
//! `plan.md` / `.lumina/plans/*.md` writes inside the workspace.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Agent {
    Plan,
    Build,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionMode {
    Ask,
    AutoEdit,
    AutoApprove,
    Bypass,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Ask,
    Deny,
}

const CREDENTIAL_FILES: &[&str] = &[
    ".env",
    ".env.local",
    ".env.production",
    "auth.json",
    "lumina.json",
    "credentials.json",
    ".npmrc",
    ".pypirc",
    "id_rsa",
    "id_ed25519",
    ".pem",
    ".key",
    "vault",
    "secrets",
];
const CREDENTIAL_SUBSTRINGS: &[&str] = &[
    ".env",
    "auth.json",
    "lumina.json",
    "credentials.json",
    "secret",
    "credential",
    "private_key",
    ".pem",
    ".key",
    "vault_exec",
    "token.json",
    "id_rsa",
    "id_ed25519",
];

pub fn is_credential_path(target: &str) -> bool {
    let lower = target.to_lowercase();
    let base = Path::new(&lower)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(&lower);
    // Cluster / identity trees: the whole subtree is sensitive (Go parity).
    for frag in ["/.kube/", "/.ssh/", "/.aws/", "/.gnupg/", "/etc/ssh/", "/etc/pki/"] {
        if lower.contains(frag) {
            return true;
        }
    }
    if lower.ends_with("/.kube")
        || lower.ends_with("/.ssh")
        || lower.ends_with("/.aws")
        || lower.ends_with("/.gnupg")
    {
        return true;
    }
    if base == "shadow" || base == "gshadow" || base == "master.passwd" {
        return true;
    }
    if CREDENTIAL_FILES.contains(&base) {
        return true;
    }
    CREDENTIAL_SUBSTRINGS
        .iter()
        .any(|s| base.contains(s) || lower.contains(s))
}

pub fn is_google_target(target: &str) -> bool {
    let u = target.to_lowercase();
    for b in [
        "googleapis.com",
        "generativelanguage",
        "gemini",
        "google-workspace",
        "fonts.googleapis",
        "vertexai",
        "aiplatform",
    ] {
        if u.contains(b) {
            return true;
        }
    }
    // Literal secret injection flags are always denied (Go parity).
    if target.contains("--from-literal=") {
        return true;
    }
    false
}

/// `plan` may only write `plan.md` or `.lumina/plans/*.md` inside the workspace.
pub fn plan_allows_write(workspace: &Path, target: &str) -> bool {
    let abs = if Path::new(target).is_absolute() {
        PathBuf::from(target)
    } else {
        workspace.join(target)
    };
    // Containment: normalize `..` lexically, require workspace prefix.
    let mut norm = PathBuf::new();
    for comp in abs.components() {
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
    if !norm.starts_with(workspace) {
        return false;
    }
    let rel = norm.strip_prefix(workspace).unwrap_or(norm.as_path());
    if rel == Path::new("plan.md") {
        return true;
    }
    // `.lumina/plans/*.md` (single level, must end .md)
    let mut it = rel.components();
    use std::path::Component::Normal;
    match (it.next(), it.next(), it.next(), it.next()) {
        (Some(Normal(a)), Some(Normal(b)), Some(Normal(c)), None)
            if a == ".lumina" && b == "plans" && c.to_string_lossy().ends_with(".md") => {}
        _ => return false,
    }
    // Reject sneaky `plan.md` lookalikes outside workspace already handled by prefix check.
    !is_credential_path(target)
}

#[derive(Debug, Clone)]
pub struct Eval {
    pub agent: Agent,
    pub mode: PermissionMode,
    pub tool: String,   // e.g. "read", "edit", "write", "bash", "kb_query"
    pub target: String, // path, command, or URL
    pub workspace: PathBuf,
    pub destructive: bool, // rm -rf, push --force, reset --hard, etc.
    /// Glob/command rules from `[permissions.bash]` (first match wins, `*` last).
    pub bash_rules: std::collections::HashMap<String, String>,
    /// Writes to skills//instructions docs need explicit opt-in (default false).
    pub allow_tooling_write: bool,
}

/// Glob-prefix bash rules: `"go test *" = "allow"`, `"*" = "ask"`.
pub fn match_bash_rules(rules: &std::collections::HashMap<String, String>, cmd: &str) -> Option<Decision> {
    for (pattern, verdict) in rules {
        if pattern == "*" {
            continue;
        }
        let prefix = pattern.trim_end_matches('*');
        if cmd.starts_with(prefix) {
            return Some(match verdict.to_lowercase().as_str() {
                "allow" => Decision::Allow,
                "deny" => Decision::Deny,
                _ => Decision::Ask,
            });
        }
    }
    rules.get("*").map(|v| match v.to_lowercase().as_str() {
        "allow" => Decision::Allow,
        "deny" => Decision::Deny,
        _ => Decision::Ask,
    })
}

pub fn is_tooling_doc(target: &str) -> bool {
    let t = target.to_lowercase().replace('\\', "/");
    t.contains("skills/") || t.ends_with("agents.md") || t.contains("instructions/")
}

/// Read-only shell allowlist for plan mode (inspection only, never mutating).
/// Any shell metacharacter (redirection, pipes, chaining, substitution)
/// disqualifies the command (Go parity).
fn is_readonly_bash(cmd: &str) -> bool {
    let t = cmd.trim();
    if t.is_empty() {
        return false;
    }
    if t.contains([';', '|', '&', '$', '`', '>', '<', '\n', '\r']) {
        return false;
    }
    let c = cmd.trim_start();
    for p in [
        "git status", "git diff", "git log", "git show", "git branch", "ls", "pwd", "echo ", "cat ",
        "head ", "tail ", "wc ", "find ", "go test", "pnpm test",
    ] {
        if c.starts_with(p) || c == p.trim_end() {
            // Destructive operators disqualify even "read-only" prefixes.
            if t.contains("rm ") || t.contains("--force") || t.contains("--hard") {
                return false;
            }
            return true;
        }
    }
    false
}

/// rm -rf / force-push / hard-reset style commands (Go parity: caller sets
/// Eval.destructive, but plan fence + tests use this directly).
pub fn is_destructive_bash(cmd: &str) -> bool {
    let lc = cmd.to_lowercase();
    for p in [
        "rm -rf",
        "rm --recursive --force",
        "git push --force",
        "git reset --hard",
        "mkfs",
        ":(){",
        "dd if=",
    ] {
        if lc.contains(p) {
            return true;
        }
    }
    false
}

pub fn decide(e: &Eval) -> Decision {
    // Layer 0 hard denies first.
    if is_credential_path(&e.target) || is_google_target(&e.target) {
        return Decision::Deny;
    }
    let tool = e.tool.as_str();
    let is_read = matches!(
        tool,
        "read" | "read_file" | "glob" | "grep" | "kb_query" | "skill" | "lsp" | "question"
            | "ask_question" | "webfetch" | "websearch" | "task_output"
    );
    let is_edit = matches!(
        tool,
        "edit" | "edit_file" | "write" | "write_file" | "patch" | "memory_write" | "todowrite"
    );
    let is_bash = tool == "bash";

    // Plan structural fence (even under bypass).
    if e.agent == Agent::Plan {
        if is_edit {
            return if plan_allows_write(&e.workspace, &e.target) {
                Decision::Allow
            } else {
                Decision::Deny
            };
        }
        if is_bash {
            // Plan research may run read-only inspection, never mutations.
            return if !e.destructive && is_readonly_bash(&e.target) {
                Decision::Allow
            } else {
                Decision::Deny
            };
        }
        // Delegation could bypass the read-only fence (a build child edits
        // while the parent stays in plan), so plan denies it.
        if tool == "subagent" || tool == "mcp" {
            return Decision::Deny;
        }
        if is_read || tool == "patch" {
            return Decision::Allow;
        }
        return Decision::Deny;
    }

    // Build envelope governed by permission mode.
    if is_read {
        return Decision::Allow;
    }
    if is_edit {
        // Writes to shared tooling docs need an explicit opt-in.
        if !e.allow_tooling_write && is_tooling_doc(&e.target) {
            return Decision::Deny;
        }
        return match e.mode {
            PermissionMode::Ask => Decision::Ask,
            _ => Decision::Allow,
        };
    }
    if is_bash {
        if let Some(d) = match_bash_rules(&e.bash_rules, &e.target) {
            return d;
        }
        if e.destructive {
            return match e.mode {
                PermissionMode::Bypass => Decision::Allow,
                _ => Decision::Ask,
            };
        }
        return match e.mode {
            PermissionMode::Ask | PermissionMode::AutoEdit => Decision::Ask,
            PermissionMode::AutoApprove | PermissionMode::Bypass => Decision::Allow,
        };
    }
    // MCP / subagents / external tools: ask unless bypass (fail closed).
    match e.mode {
        PermissionMode::Bypass => Decision::Allow,
        _ => Decision::Ask,
    }
}



#[cfg(test)]
mod tests {
    use super::*;

    fn ws() -> PathBuf {
        PathBuf::from("/repo")
    }

    #[test]
    fn plan_blocks_source_edits_even_under_bypass() {
        let e = Eval {
            agent: Agent::Plan,
            mode: PermissionMode::Bypass,
            tool: "edit".into(),
            target: "src/main.go".into(),
            workspace: ws(),
            destructive: false,
            bash_rules: Default::default(),
            allow_tooling_write: false,
        };
        assert_eq!(decide(&e), Decision::Deny);
    }

    #[test]
    fn plan_allows_scoped_plan_write() {
        for t in ["plan.md", ".lumina/plans/s1.md"] {
            let e = Eval {
                agent: Agent::Plan,
                mode: PermissionMode::Ask,
                tool: "write".into(),
                target: t.into(),
                workspace: ws(),
                destructive: false,
                bash_rules: Default::default(),
                allow_tooling_write: false,
            };
            assert_eq!(decide(&e), Decision::Allow, "{t}");
        }
    }

    #[test]
    fn plan_rejects_escape_lookalike() {
        let e = Eval {
            agent: Agent::Plan,
            mode: PermissionMode::Bypass,
            tool: "write".into(),
            target: "/tmp/other/plan.md".into(),
            workspace: ws(),
            destructive: false,
            bash_rules: Default::default(),
            allow_tooling_write: false,
        };
        assert_eq!(decide(&e), Decision::Deny);
    }

    #[test]
    fn bypass_still_denies_credentials_and_google() {
        for t in [".env", "auth.json", "https://x.googleapis.com/v1"] {
            let e = Eval {
                agent: Agent::Build,
                mode: PermissionMode::Bypass,
                tool: "bash".into(),
                target: t.into(),
                workspace: ws(),
                destructive: false,
                bash_rules: Default::default(),
                allow_tooling_write: false,
            };
            assert_eq!(decide(&e), Decision::Deny, "{t}");
        }
    }
}

#[cfg(test)]
mod fence_tests {
    use super::*;

    fn ws() -> PathBuf {
        PathBuf::from("/repo")
    }

    #[test]
    fn bash_rules_precedence() {
        use std::collections::HashMap;
        let rules: HashMap<String, String> =
            [("git status*".into(), "allow".into()), ("*".into(), "ask".into())].into_iter().collect();
        assert_eq!(match_bash_rules(&rules, "git status --porcelain"), Some(Decision::Allow));
        assert_eq!(match_bash_rules(&rules, "rm -rf /"), Some(Decision::Ask));
        assert_eq!(match_bash_rules(&HashMap::new(), "ls"), None);
    }

    #[test]
    fn tooling_doc_writes_denied_without_opt_in() {
        let e = Eval {
            agent: Agent::Build,
            mode: PermissionMode::Bypass,
            tool: "edit".into(),
            target: "skills/foo/SKILL.md".into(),
            workspace: ws(),
            destructive: false,
            bash_rules: Default::default(),
            allow_tooling_write: false,
        };
        assert_eq!(decide(&e), Decision::Deny);
    }

    #[test]
    fn plan_allows_readonly_bash() {
        let e = Eval {
            agent: Agent::Plan,
            mode: PermissionMode::Bypass,
            tool: "bash".into(),
            target: "git status --porcelain".into(),
            workspace: ws(),
            destructive: false,
            bash_rules: Default::default(),
            allow_tooling_write: false,
        };
        assert_eq!(decide(&e), Decision::Allow);
        let e2 = Eval { target: "rm -rf /tmp/x".into(), ..e.clone() };
        assert_eq!(decide(&e2), Decision::Deny);
    }

    #[test]
    fn mcp_asks_unless_bypass() {
        let mk = |mode| Eval {
            agent: Agent::Build,
            mode,
            tool: "mcp".into(),
            target: "local:tool".into(),
            workspace: ws(),
            destructive: false,
            bash_rules: Default::default(),
            allow_tooling_write: false,
        };
        assert_eq!(decide(&mk(PermissionMode::Ask)), Decision::Ask);
        assert_eq!(decide(&mk(PermissionMode::Bypass)), Decision::Allow);
        let mut p = mk(PermissionMode::Bypass);
        p.agent = Agent::Plan;
        p.tool = "subagent".into();
        assert_eq!(decide(&p), Decision::Deny);
    }
}
