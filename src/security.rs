//! Immutable security floor: plan/build structural fence + 4-mode broker.
//! Credential hard-denies are evaluated first in every decision and win over
//! everything, including `bypass`. `plan` agent is read-only except scoped
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

/// Home directory (`os.UserHomeDir` then `$HOME`).
pub fn home_dir() -> String {
    std::env::var("HOME").unwrap_or_default()
}

/// `~` and `~/…` expand to the home directory (ExpandHome parity).
pub fn expand_home(p: &str) -> String {
    if p == "~" {
        let h = home_dir();
        if h.is_empty() { p.into() } else { h }
    } else if let Some(rest) = p.strip_prefix("~/") {
        let h = home_dir();
        if h.is_empty() { p.into() } else { format!("{h}/{rest}") }
    } else {
        p.into()
    }
}

/// Temp locations writable outside the workspace.
pub fn temp_prefixes() -> Vec<String> {
    let mut out = Vec::new();
    for s in [std::env::temp_dir().to_string_lossy().into_owned(),
        "/tmp".into(), "/var/tmp".into(), "/temp".into(), "/private/tmp".into()] {
        if !s.is_empty() && !out.contains(&s) {
            out.push(s);
        }
    }
    out
}

/// System locations readable outside the workspace (Linux/macOS set).
pub fn system_read_prefixes() -> Vec<String> {
    ["/etc", "/usr", "/bin", "/sbin", "/lib", "/lib64", "/opt", "/System", "/Library"]
        .iter().map(|s| s.to_string()).collect()
}

fn under(dir: &str, p: &str) -> bool {
    !dir.is_empty() && (p == dir || p.starts_with(&format!("{dir}/")))
}

fn clean_abs(p: &str) -> Option<String> {
    let e = expand_home(p);
    let path = Path::new(&e);
    if !path.is_absolute() {
        return None;
    }
    // Lexical clean (no symlink chase).
    let mut norm = PathBuf::new();
    for comp in path.components() {
        use std::path::Component::*;
        match comp {
            RootDir => norm.push("/"),
            Prefix(pr) => norm.push(pr.as_os_str()),
            CurDir => {}
            ParentDir => { norm.pop(); }
            Normal(c) => norm.push(c),
        }
    }
    Some(norm.to_string_lossy().into_owned())
}

/// Read allowlist: home, temp, or system (IsWhitelistedRead parity).
/// Relative paths return false (they resolve inside the workspace).
pub fn is_whitelisted_read(p: &str) -> bool {
    let Some(e) = clean_abs(p) else { return false };
    let h = home_dir();
    if under(&h, &e) {
        return true;
    }
    temp_prefixes().iter().any(|t| under(t, &e)) || system_read_prefixes().iter().any(|t| under(t, &e))
}

/// Write allowlist: temp locations only (IsWhitelistedWrite parity).
pub fn is_whitelisted_write(p: &str) -> bool {
    let Some(e) = clean_abs(p) else { return false };
    temp_prefixes().iter().any(|t| under(t, &e))
}

/// Union for generic fence checks.
pub fn is_whitelisted_external(p: &str) -> bool {
    is_whitelisted_read(p) || is_whitelisted_write(p)
}

fn in_workspace(workspace: &Path, target: &str) -> bool {
    if workspace.as_os_str().is_empty() {
        return false;
    }
    let e = expand_home(target);
    let cand = if Path::new(&e).is_absolute() { PathBuf::from(&e) } else { workspace.join(&e) };
    let mut norm = PathBuf::new();
    for comp in cand.components() {
        use std::path::Component::*;
        match comp {
            RootDir => norm.push("/"),
            Prefix(pr) => norm.push(pr.as_os_str()),
            CurDir => {}
            ParentDir => { norm.pop(); }
            Normal(c) => norm.push(c),
        }
    }
    norm.starts_with(workspace)
}

/// Inside the workspace or whitelisted for reads.
pub fn is_read_allowed(workspace: &Path, target: &str) -> bool {
    if workspace.as_os_str().is_empty() {
        return true;
    }
    if in_workspace(workspace, target) {
        return true;
    }
    is_whitelisted_read(target)
}

/// Inside the workspace or whitelisted for writes (temp only).
pub fn is_write_allowed(workspace: &Path, target: &str) -> bool {
    if workspace.as_os_str().is_empty() {
        return true;
    }
    if in_workspace(workspace, target) {
        return true;
    }
    is_whitelisted_write(target)
}

/// External-whitelist secret override (credentialOverrideAllowed parity):
/// credential-shaped paths outside the workspace in a whitelisted location
/// skip the hard-deny. Workspace-internal secrets stay denied. Bash commands
/// are scanned for path-like tokens.
pub fn credential_override_allowed(workspace: &Path, tool: &str, target: &str) -> bool {
    let low = tool.to_lowercase();
    if low == "bash" || low == "shell" {
        let mut found_external = false;
        for tok in target.split([' ', '\t', '\n', '"', '\'', ';', '|', '&', '`']) {
            let t = tok.trim_matches(|c| matches!(c, '(' | ')' | '{' | '}' | '<' | '>'));
            if t.is_empty() {
                continue;
            }
            if !(t.starts_with('/') || t.starts_with('~') || t.starts_with("$HOME")) {
                continue;
            }
            let t = expand_home(&t.replace("$HOME", &home_dir()));
            let Some(abs) = clean_abs(&t) else { continue };
            if in_workspace(workspace, &abs) {
                // Workspace-internal secret ref wins: stay denied.
                if is_credential_path(&abs) {
                    return false;
                }
                continue;
            }
            if is_whitelisted_read(&abs) || is_whitelisted_write(&abs) {
                found_external = true;
            }
        }
        if !found_external {
            let low_t = target.to_lowercase();
            for frag in temp_prefixes().into_iter().chain(system_read_prefixes()) {
                if low_t.contains(&frag.to_lowercase()) {
                    found_external = true;
                    break;
                }
            }
            let h = home_dir().to_lowercase();
            if !h.is_empty() && low_t.contains(&h) {
                found_external = true;
            }
            if target.contains("~/") || target.contains("$HOME") {
                found_external = true;
            }
        }
        return found_external;
    }
    // File tools: single path target.
    if !is_credential_path(target) {
        return false;
    }
    if in_workspace(workspace, target) {
        return false; // workspace secrets stay denied
    }
    is_whitelisted_external(target)
}

/// `plan` may only write `plan.md` or `.lumina/plans/*.md` inside the workspace.
pub fn plan_allows_write(workspace: &Path, target: &str) -> bool {
    let expanded = expand_home(target);
    let abs = if Path::new(&expanded).is_absolute() {
        PathBuf::from(&expanded)
    } else {
        workspace.join(&expanded)
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
    /// Writes to skills//rules docs need explicit opt-in (default false).
    pub allow_tooling_write: bool,
}

/// OpenCode `shell/parse.ts` parity (portable subset): split a command
/// into top-level segments on `&&`, `||`, `;`, `|` and newlines outside
/// quotes, so each subcommand is permission-checked on its own
/// (`a && b | c` = 3 checks, denied individually).
pub fn split_shell_segments(cmd: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut chars = cmd.chars().peekable();
    let push = |out: &mut Vec<String>, cur: &mut String| {
        let t = cur.trim().to_string();
        if !t.is_empty() {
            out.push(t);
        }
        cur.clear();
    };
    while let Some(c) = chars.next() {
        if let Some(q) = quote {
            cur.push(c);
            if c == q {
                quote = None;
            }
            continue;
        }
        match c {
            '\'' | '"' => {
                quote = Some(c);
                cur.push(c);
            }
            ';' | '\n' => push(&mut out, &mut cur),
            '|' | '&' => {
                // `||` and `&&` are one separator, like single `|` / `&`.
                if chars.peek() == Some(&c) {
                    chars.next();
                }
                push(&mut out, &mut cur);
            }
            _ => cur.push(c),
        }
    }
    push(&mut out, &mut cur);
    out
}

/// A pure directory-change prefix (`cd /x`, `pushd ..`) only selects the
/// working directory; like opencode it is not a permission resource alone.
fn is_cd_only(seg: &str) -> bool {
    let t = seg.trim_start();
    t == "cd" || t.starts_with("cd ") || t == "pushd" || t.starts_with("pushd ") || t == "popd"
}

/// Combine per-segment verdicts the opencode way: any deny denies,
/// otherwise any ask asks, otherwise allow. Segments matching no rule
/// count as ask (fail-safe: opencode defaults unmatched to ask).
/// Returns None only when nothing checkable ran (empty / cd-only).
pub fn decide_bash_segments(
    rules: &std::collections::HashMap<String, String>,
    cmd: &str,
) -> Option<Decision> {
    // No rules: fall through to the mode default (preserves --bypass/-y).
    if rules.is_empty() {
        return None;
    }
    let mut saw_allow = false;
    let mut saw_ask = false;
    let mut checked = 0;
    for seg in split_shell_segments(cmd) {
        let t = seg.trim();
        if t.is_empty() || is_cd_only(t) {
            continue;
        }
        checked += 1;
        match match_bash_rules(rules, t) {
            Some(Decision::Deny) => return Some(Decision::Deny),
            Some(Decision::Ask) => saw_ask = true,
            Some(Decision::Allow) => saw_allow = true,
            None => saw_ask = true,
        }
    }
    if checked == 0 {
        return None;
    }
    if saw_ask {
        Some(Decision::Ask)
    } else if saw_allow {
        Some(Decision::Allow)
    } else {
        None
    }
}

/// Glob-prefix bash rules: `"cargo test *" = "allow"`, `"*" = "ask"`.
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
    t.contains("skills/") || t.ends_with("agents.md") || t.contains("rules/")
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
        "head ", "tail ", "wc ", "find ", "cargo test", "cargo check", "cargo clippy", "pnpm test",
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
    // Layer 0 hard denies first (credential paths honor the explicit
    // external-whitelist override).
    if is_credential_path(&e.target) && !credential_override_allowed(&e.workspace, &e.tool, &e.target) {
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
        if let Some(d) = decide_bash_segments(&e.bash_rules, &e.target) {
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
    fn shell_segments_split_and_combine() {
        assert_eq!(split_shell_segments("cargo test * && cargo clippy"), vec!["cargo test *", "cargo clippy"]);
        assert_eq!(split_shell_segments("a; b | c"), vec!["a", "b", "c"]);
        assert_eq!(split_shell_segments("echo \"a && b\""), vec!["echo \"a && b\""]);
        assert_eq!(split_shell_segments("cd /tmp && ls"), vec!["cd /tmp", "ls"]);
        let mut rules = std::collections::HashMap::new();
        rules.insert("cargo test *".to_string(), "allow".to_string());
        rules.insert("rm *".to_string(), "deny".to_string());
        rules.insert("*".to_string(), "ask".to_string());
        // All-allow batch allows; any deny denies the whole command.
        assert!(matches!(decide_bash_segments(&rules, "cargo test * && ls"), Some(Decision::Ask)));
        assert!(matches!(decide_bash_segments(&rules, "cargo test *"), Some(Decision::Allow)));
        assert!(matches!(decide_bash_segments(&rules, "cargo test * && rm -rf /"), Some(Decision::Deny)));
        // cd-only prefix is not a resource; empty rules fall through.
        assert!(matches!(decide_bash_segments(&rules, "cd /tmp && cargo test *"), Some(Decision::Allow)));
        assert!(decide_bash_segments(&std::collections::HashMap::new(), "cargo test *").is_none());
        assert!(decide_bash_segments(&rules, "   ").is_none());
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
    fn tooling_docs_cover_rules_dir() {
        for t in [
            ".lumina/rules/coding.md",
            "/home/u/.config/lumina/rules/style.md",
            ".lumina/skills/foo/SKILL.md",
            "AGENTS.md",
        ] {
            assert!(is_tooling_doc(t), "{t}");
        }
        assert!(!is_tooling_doc("src/main.rs"));
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
    fn bypass_still_denies_credentials() {
        for t in [".env", "auth.json"] {
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

#[cfg(test)]
mod parity_tests {
    use super::*;

    fn ws() -> PathBuf {
        PathBuf::from("/repo")
    }

    #[test]
    fn credential_widened_lists() {
        // Go CredentialFileNames parity: .env.local, .npmrc, id_rsa, vault, secrets.
        for t in [
            ".env.local",
            ".env.production",
            ".npmrc",
            ".pypirc",
            "id_rsa",
            "id_ed25519",
            "vault",
            "secrets",
            "token.json",
            "vault_exec",
        ] {
            assert!(is_credential_path(t), "{t}");
        }
        // Cluster / identity subtrees and shadow files.
        assert!(is_credential_path("/home/u/.kube/config"));
        assert!(is_credential_path("/home/u/.ssh/id_rsa"));
        assert!(is_credential_path("/etc/shadow"));
        assert!(is_credential_path("gshadow"));
        // Innocent paths stay allowed.
        assert!(!is_credential_path("main.go"));
        assert!(!is_credential_path("README.md"));
    }

    #[test]
    fn destructive_bash_detector() {
        for c in [
            "rm -rf /tmp/x",
            "git push --force origin main",
            "git reset --hard HEAD",
            "mkfs /dev/sda",
            ":(){ :|:& };:",
            "dd if=/dev/zero of=/dev/sda",
        ] {
            assert!(is_destructive_bash(c), "{c}");
        }
        assert!(!is_destructive_bash("cargo test"));
        assert!(!is_destructive_bash("ls -la"));
    }

    #[test]
    fn plan_rejects_shell_metachar() {
        // Go IsReadOnlyBash parity: metachar disqualifies even "read-only" prefixes.
        let mk = |target: &str| Eval {
            agent: Agent::Plan,
            mode: PermissionMode::Bypass,
            tool: "bash".into(),
            target: target.into(),
            workspace: ws(),
            destructive: false,
            bash_rules: Default::default(),
            allow_tooling_write: false,
        };
        assert_eq!(decide(&mk("git status --porcelain")), Decision::Allow);
        for bad in [
            "echo hi > plan.md",
            "git status; rm x",
            "ls | grep foo",
            "git log && touch p",
        ] {
            assert_eq!(decide(&mk(bad)), Decision::Deny, "{bad}");
        }
    }
}
