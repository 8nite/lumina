//! Lumina config: TOML loader + secret indirection + Google-domain block.
//! Mirrors `internal/config/config.go` (M0 scope).

use serde::Deserialize;
use std::{collections::HashMap, path::PathBuf};
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
    7432
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

#[derive(Debug, Clone, Deserialize, Default)]
pub struct KbConfig {
    #[serde(default = "default_idle")]
    pub idle_debounce_minutes: u64,
    #[serde(default)]
    pub standing_ops: bool,
}
fn default_idle() -> u64 {
    15
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
            "openai-compatible",
            "openai-responses",
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
            if is_google_url(&p.base_url) {
                return Err(ConfigError::Msg(format!(
                    "config: provider {name:?}: Google endpoints are blocked (zero-Google rule)"
                )));
            }
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

pub fn is_google_url(url: &str) -> bool {
    let u = url.to_lowercase();
    u.contains("googleapis.com") || u.contains("google.com") || u.contains("gemini")
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
    fn rejects_google_endpoint() {
        let f = write_tmp(
            "[providers.g]\nprotocol=\"openai-compatible\"\nbase_url=\"https://generativelanguage.googleapis.com\"\n",
        );
        let err = Config::load(Some(f.path().into())).unwrap_err();
        assert!(err.to_string().contains("Google"), "{err}");
    }

    #[test]
    fn bash_table_parses() {
        let f = write_tmp("[permissions.bash]\n\"git status*\" = \"allow\"\n");
        let cfg = Config::load(Some(f.path().into())).unwrap();
        assert_eq!(cfg.permissions_bash.get("git status*").unwrap(), "allow");
    }
}
