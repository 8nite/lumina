//! Lumina config: TOML loader + secret indirection.
//! Secrets only via `api_key_env` / `api_key_cmd`; plaintext keys are rejected.

use serde::Deserialize;
use std::{collections::HashMap, path::{Path, PathBuf}};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("config: {0}")]
    Msg(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Toml(#[from] toml::de::Error),
}

#[derive(Debug, Clone, Deserialize, Default)]
#[allow(dead_code)]
pub struct ServerConfig {
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
}
fn default_host() -> String {
    "127.0.0.1".into()
}
fn default_port() -> u16 {
    4089
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct DefaultsConfig {
    #[serde(default = "default_agent")]
    pub agent: String,
    #[serde(default = "default_perm")]
    pub permission_mode: String,
}
fn default_agent() -> String {
    "build".into()
}
fn default_perm() -> String {
    "bypass".into()
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct CompatConfig {
    #[serde(default = "default_true")]
    pub import_global_rules: bool,
    #[serde(default = "default_true")]
    pub import_global_skills: bool,
}
fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ProviderConfig {
    #[serde(default)]
    pub protocol: String,
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub api_key_env: String,
    #[serde(default)]
    pub api_key_cmd: String,
    /// Copilot token-exchange endpoint override (GHES). Go parity.
    #[serde(default)]
    pub token_url: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct RouterConfig {
    #[serde(default)]
    pub fast: String,
    #[serde(default)]
    pub executor: String,
    #[serde(default)]
    pub planner: String,
    #[serde(default)]
    pub improver: String,
}

impl RouterConfig {
    /// Tier model lookup; prefer `llm::Router::model_for` (planner fallback).
    #[allow(dead_code)]
    pub fn model_for_role(&self, role: &str) -> &str {
        match role {
            "fast" => &self.fast,
            "planner" => &self.planner,
            "improver" => &self.improver,
            _ => &self.executor,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct WebSearchConfig {
    #[serde(default)]
    pub provider: String,
    /// Kill-switch: when true the websearch tool is removed (Go parity).
    #[serde(default)]
    pub disabled: bool,
}

/// Auto-continue until green: an unfinished turn (unverified edits,
/// zero-edit work request, budget ceiling) loops back in server-side
/// instead of asking the user to type `continue`. Bounded by
/// `max_auto_continues` (each turn up to 120 rounds).
#[derive(Debug, Clone, Deserialize)]
pub struct AgentConfig {
    /// Master switch. Default ON (Always mode).
    #[serde(default = "default_true")]
    pub auto_continue: bool,
    /// Extra turns after the first (default 3 → up to 4×120 rounds).
    #[serde(default = "default_max_auto")]
    pub max_auto_continues: u8,
}
fn default_max_auto() -> u8 {
    3
}
impl Default for AgentConfig {
    fn default() -> Self {
        Self { auto_continue: true, max_auto_continues: default_max_auto() }
    }
}

/// Bubblewrap confinement for the `bash` tool (`src/sandbox.rs`).
/// Default OFF: sandboxing only engages with explicit opt-in.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct SandboxConfig {
    /// Master switch. Off = current behavior, byte for byte.
    #[serde(default)]
    pub enabled: bool,
    /// When true the sandbox keeps host networking (cargo fetch, npm…).
    #[serde(default)]
    pub network: bool,
    /// Sandbox binary (default `bwrap`). Must be a bare path.
    #[serde(default)]
    pub command: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct KbConfig {
    #[serde(default = "default_idle")]
    pub idle_debounce_minutes: u64,
    #[serde(default)]
    pub standing_ops: bool,
    /// Rebuild workspace AST graph on git commit (Go parity; honored by hooks).
    #[serde(default)]
    #[allow(dead_code)]
    pub auto_commit_ast_hook: bool,
    /// Explicit KB dir override (workspace-relative or absolute,
    /// colon-separated for several). Wins over rule-md mentions and
    /// `LUMINA_MEMORY_DIR`. Empty = rule-md resolution (see `kb`).
    #[serde(default)]
    pub memory_dir: String,
}
fn default_idle() -> u64 {
    15
}

/// Language-server wiring for the `lsp` tool (`src/lsp.rs`). Servers are
/// spawned lazily per workspace root and language; a missing binary fails
/// the single tool call with an install hint, never the turn.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct LspConfig {
    /// Kill-switch: when true the `lsp` tool is removed from the tool list.
    #[serde(default)]
    pub disabled: bool,
    /// Per-language server command line, e.g. `rust = "rust-analyzer"`,
    /// `typescript = "typescript-language-server --stdio"`. Unset languages
    /// use the built-in defaults.
    #[serde(default)]
    pub commands: HashMap<String, String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct RawConfig {
    #[serde(default)]
    server: ServerConfig,
    #[serde(default)]
    defaults: DefaultsConfig,
    #[serde(default)]
    compat: CompatConfig,
    #[serde(default)]
    providers: HashMap<String, ProviderConfig>,
    #[serde(default)]
    router: RouterConfig,
    #[serde(default)]
    kb: KbConfig,
    #[serde(default)]
    agent: AgentConfig,
    #[serde(default)]
    lsp: LspConfig,
    #[serde(default)]
    sandbox: SandboxConfig,
    #[serde(default)]
    websearch: WebSearchConfig,
    /// `[permissions] edit="allow"` plus nested `[permissions.bash]` table.
    /// `toml::Value` tolerates the mixed string/table shape (Go uses `toml.Primitive`).
    #[serde(default)]
    permissions: HashMap<String, toml::Value>,
}

#[derive(Debug, Clone, Default)]
pub struct Config {
    pub server: ServerConfig,
    pub defaults: DefaultsConfig,
    pub compat: CompatConfig,
    pub providers: HashMap<String, ProviderConfig>,
    pub router: RouterConfig,
    pub kb: KbConfig,
    pub agent: AgentConfig,
    pub lsp: LspConfig,
    pub sandbox: SandboxConfig,
    pub websearch: WebSearchConfig,
    pub permissions: HashMap<String, String>,
    pub permissions_bash: HashMap<String, String>,
}

impl Config {
    /// Load from `path` (explicit) or `LUMINA_CONFIG` / `~/.config/lumina/config.toml`.
    pub fn load(path: Option<PathBuf>) -> Result<Self, ConfigError> {
        let p = path.or_else(default_path).filter(|p| p.exists());
        let mut cfg = Config::default();
        if let Some(p) = p {
            let text = std::fs::read_to_string(&p)?;
            // Reject plaintext secrets at load: `api_key = "..."` is forbidden.
            for (i, line) in text.lines().enumerate() {
                let t = line.trim_start();
                if t.starts_with('#') || t.starts_with("//") {
                    continue;
                }
                // Match `api_key =` but not `api_key_env` / `api_key_cmd`.
                if let Some(rest) = t.strip_prefix("api_key") {
                    let rest = rest.trim_start();
                    if rest.starts_with('=') {
                        return Err(ConfigError::Msg(format!(
                            "config: {}:{}: plaintext `api_key` is forbidden; use `api_key_env` or `api_key_cmd`",
                            p.display(),
                            i + 1
                        )));
                    }
                }
            }
            let raw: RawConfig = toml::from_str(&text)?;
            cfg.server = raw.server;
            cfg.defaults = raw.defaults;
            cfg.compat = raw.compat;
            cfg.providers = raw.providers;
            cfg.router = raw.router;
            cfg.kb = raw.kb;
            cfg.agent = raw.agent;
            cfg.lsp = raw.lsp;
            cfg.sandbox = raw.sandbox;
            cfg.websearch = raw.websearch;
            for (k, v) in raw.permissions {
                if k == "bash" {
                    if let toml::Value::Table(t) = v {
                        for (gk, gv) in t {
                            if let toml::Value::String(s) = gv {
                                cfg.permissions_bash.insert(gk, s);
                            }
                        }
                    }
                    continue;
                }
                if let toml::Value::String(s) = v {
                    cfg.permissions.insert(k, s);
                }
            }
        }
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        const PROTOCOLS: &[&str] = &[
            "",
            "openai-compatible",
            "openai-responses",
            "responses",
            "anthropic-compatible",
            "ollama",
            "copilot",
        ];
        for (name, p) in &self.providers {
            if !PROTOCOLS.contains(&p.protocol.as_str()) {
                return Err(ConfigError::Msg(format!(
                    "config: provider {name:?} has unknown protocol {:?}",
                    p.protocol
                )));
            }
        }
        let ws = self.websearch.provider.trim().to_lowercase();
        if !matches!(
            ws.as_str(),
            "" | "exa" | "firecrawl" | "tavily" | "parallel" | "tinyfish" | "random" | "auto"
        ) {
            return Err(ConfigError::Msg(format!(
                "config: unknown websearch provider {ws:?} (want exa|firecrawl|parallel|tavily|tinyfish|random)"
            )));
        }
        Ok(())
    }

    /// Resolve a provider secret via `api_key_env` or `api_key_cmd`.
    /// Never logs or returns the value to callers that print it; errors carry no secret.
    pub fn resolve_secret(&self, provider: &str) -> Result<Option<String>, ConfigError> {
        let Some(p) = self.providers.get(provider) else {
            return Ok(None);
        };
        if !p.api_key_env.is_empty() {
            return Ok(std::env::var(&p.api_key_env).ok());
        }
        if !p.api_key_cmd.is_empty() {
            // ponytail: shell out is the minimal correct indirection; no caching here.
            let out = std::process::Command::new("sh")
                .arg("-c")
                .arg(&p.api_key_cmd)
                .output()
                .map_err(|e| ConfigError::Msg(format!("config: api_key_cmd failed: {e}")))?;
            if !out.status.success() {
                return Err(ConfigError::Msg("config: api_key_cmd exited non-zero".into()));
            }
            let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
            return Ok(if s.is_empty() { None } else { Some(s) });
        }
        Ok(None)
    }
}

/// Hot-swap a tier model and persist it to the config file when one is
/// configured (model-persist: survives daemon restarts).
pub fn persist_router_model(path: &Path, role: &str, model: &str) -> Result<(), ConfigError> {
    if !matches!(role, "fast" | "executor" | "planner" | "improver") {
        return Err(ConfigError::Msg(format!("bad router role {role:?}")));
    }
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir)?;
        }
    }
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let mut out = String::new();
    let mut in_router = false;
    let mut done = false;
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            if in_router && !done {
                out.push_str(&format!("{role} = \"{model}\"\n"));
                done = true;
            }
            in_router = t == "[router]";
            out.push_str(line);
            out.push('\n');
            continue;
        }
        if in_router && !done {
            let key = t.split(['=', ' ']).next().unwrap_or("");
            if key == role {
                out.push_str(&format!("{role} = \"{model}\"\n"));
                done = true;
                continue;
            }
        }
        out.push_str(line);
        out.push('\n');
    }
    if !done {
        if !in_router {
            if !out.is_empty() && !out.ends_with("\n\n") {
                out.push('\n');
            }
            out.push_str("[router]\n");
        }
        out.push_str(&format!("{role} = \"{model}\"\n"));
    }
    std::fs::write(path, out)?;
    Ok(())
}

pub fn default_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("LUMINA_CONFIG") {
        return Some(PathBuf::from(p));
    }
    std::env::var("HOME")
        .ok()
        .map(|h| PathBuf::from(h).join(".config/lumina/config.toml"))
}


#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_tmp(body: &str) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(body.as_bytes()).unwrap();
        f
    }

    #[test]
    fn rejects_plaintext_api_key() {
        let f = write_tmp("[providers.x]\nprotocol=\"ollama\"\napi_key=\"sk-live\"\n");
        let err = Config::load(Some(f.path().into())).unwrap_err();
        assert!(err.to_string().contains("plaintext"), "{err}");
    }

    #[test]
    fn bash_table_parses() {
        let f = write_tmp("[permissions.bash]\n\"git status*\" = \"allow\"\n");
        let cfg = Config::load(Some(f.path().into())).unwrap();
        assert_eq!(cfg.permissions_bash.get("git status*").unwrap(), "allow");
    }

    #[test]
    fn empty_and_responses_protocols_accepted() {
        for proto in ["", "responses", "openai-responses", "anthropic-compatible"] {
            let body = format!("[providers.g]\nprotocol=\"{proto}\"\n");
            let f = write_tmp(&body);
            assert!(Config::load(Some(f.path().into())).is_ok(), "{proto}");
        }
    }

    #[test]
    fn websearch_disabled_and_ast_hook_parse() {
        let f = write_tmp("[websearch]\ndisabled = true\n[kb]\nauto_commit_ast_hook = true\nmemory_dir = \"docs/notes\"\n");
        let cfg = Config::load(Some(f.path().into())).unwrap();
        assert!(cfg.websearch.disabled);
        assert!(cfg.kb.auto_commit_ast_hook);
        assert_eq!(cfg.kb.memory_dir, "docs/notes");
        let f2 = write_tmp("");
        let cfg2 = Config::load(Some(f2.path().into())).unwrap();
        assert!(!cfg2.websearch.disabled);
        assert!(!cfg2.kb.auto_commit_ast_hook);
        assert!(cfg2.kb.memory_dir.is_empty());
    }

    #[test]
    fn lsp_section_parses() {
        let f = write_tmp("[lsp]\ndisabled = true\n[lsp.commands]\nrust = \"rust-analyzer\"\n");
        let cfg = Config::load(Some(f.path().into())).unwrap();
        assert!(cfg.lsp.disabled);
        assert_eq!(cfg.lsp.commands.get("rust").map(|s| s.as_str()), Some("rust-analyzer"));
        let cfg2 = Config::load(Some(write_tmp("").path().into())).unwrap();
        assert!(!cfg2.lsp.disabled);
        assert!(cfg2.lsp.commands.is_empty());
    }

    #[test]
    fn sandbox_section_parses() {
        let f = write_tmp("[sandbox]\nenabled = true\nnetwork = true\ncommand = \"/usr/bin/bwrap\"\n");
        let cfg = Config::load(Some(f.path().into())).unwrap();
        assert!(cfg.sandbox.enabled);
        assert!(cfg.sandbox.network);
        assert_eq!(cfg.sandbox.command, "/usr/bin/bwrap");
        let cfg2 = Config::load(Some(write_tmp("").path().into())).unwrap();
        assert!(!cfg2.sandbox.enabled);
        assert!(!cfg2.sandbox.network);
        assert!(cfg2.sandbox.command.is_empty());
    }
}

#[cfg(test)]
mod persist_tests {
    use super::*;

    #[test]
    fn persist_updates_and_appends() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("config.toml");
        std::fs::write(&p, "[router]\nexecutor = \"a/x\"\n").unwrap();
        persist_router_model(&p, "executor", "b/y").unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.contains("executor = \"b/y\""), "{text}");
        assert_eq!(text.matches("executor").count(), 1);
        persist_router_model(&p, "fast", "c/z").unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.contains("fast = \"c/z\""), "{text}");
        // Missing file: creates a minimal [router] section.
        let p2 = d.path().join("fresh.toml");
        persist_router_model(&p2, "improver", "d/w").unwrap();
        assert!(std::fs::read_to_string(&p2).unwrap().contains("[router]"));
        assert!(persist_router_model(&p, "bogus", "x").is_err());
    }
}
