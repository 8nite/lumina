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
    "auth.json",
    "lumina.json",
    "credentials.json",
    ".env",
    ".pem",
    ".key",
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
];

pub fn is_credential_path(target: &str) -> bool {
    let lower = target.to_lowercase();
    let base = Path::new(&lower)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(&lower);
    if CREDENTIAL_FILES.contains(&base) {
        return true;
    }
    CREDENTIAL_SUBSTRINGS.iter().any(|s| lower.contains(s))
}

pub fn is_google_target(target: &str) -> bool {
    let u = target.to_lowercase();
    u.contains("googleapis.com") || u.contains("google.com") || u.contains("gemini")
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
}

pub fn decide(e: &Eval) -> Decision {
    // Layer 0 hard denies first.
    if is_credential_path(&e.target) || is_google_target(&e.target) {
        return Decision::Deny;
    }
    let tool = e.tool.as_str();
    let is_read = matches!(tool, "read" | "glob" | "grep" | "kb_query" | "skill" | "lsp");
    let is_edit = matches!(tool, "edit" | "write" | "patch");
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
            return Decision::Deny; // M0: plan bash denied; read-only inspection allowlist lands in M1 tools.
        }
        return Decision::Allow;
    }

    // Build envelope governed by permission mode.
    if is_read {
        return Decision::Allow;
    }
    if is_edit {
        return match e.mode {
            PermissionMode::Ask => Decision::Ask,
            _ => Decision::Allow,
        };
    }
    if is_bash {
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
    // Unknown tools default to prompt (fail closed).
    Decision::Ask
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
            };
            assert_eq!(decide(&e), Decision::Deny, "{t}");
        }
    }
}
