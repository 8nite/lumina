//! GitHub device flow for Copilot (`lumina auth device`, TUI /connect).
//! Mirrors `internal/llm/copilot_auth.go`: web auth via github.com device
//! codes, tokens in `~/.config/lumina/tokens/<provider>.json` (0600).

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AuthError {
    #[error("auth: {0}")]
    Msg(String),
    #[error(transparent)]
    Http(#[from] reqwest::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

pub const COPILOT_OAUTH_CLIENT_ID: &str = "Iv1.b507a08c87ecfe98";
pub const COPILOT_DEVICE_SCOPE: &str = "read:user";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceCode {
    #[serde(default)]
    pub device_code: String,
    #[serde(default)]
    pub user_code: String,
    #[serde(default)]
    pub verification_uri: String,
    #[serde(default)]
    pub expires_in: u64,
    #[serde(default)]
    pub interval: u64,
}

fn device_host(token_url: &str) -> String {
    // ponytail: string surgery, no url crate — hosts only.
    let t = token_url.trim_end_matches('/');
    let no_scheme = t.split("://").nth(1).unwrap_or(t);
    let host = no_scheme.split('/').next().unwrap_or(no_scheme);
    if host == "api.github.com" {
        return "https://github.com".into();
    }
    if let Some(rest) = host.strip_suffix(".api.github.com") {
        return format!("https://{rest}"); // data-residency style hosts
    }
    if let Some((h, _)) = no_scheme.split_once("/api/") {
        if !h.is_empty() && !h.contains('/') {
            return format!("https://{h}");
        }
    }
    format!("https://{host}")
}

fn client() -> reqwest::Client {
    reqwest::Client::builder().user_agent("lumina/0.1.0").build().expect("reqwest client")
}

/// Begin web auth: user opens `verification_uri` and enters `user_code`.
pub async fn start_device_flow(token_url: &str, scope: &str) -> Result<DeviceCode, AuthError> {
    let scope = if scope.is_empty() { COPILOT_DEVICE_SCOPE } else { scope };
    let res = client()
        .post(format!("{}/login/device/code", device_host(token_url)))
        .form(&[("client_id", COPILOT_OAUTH_CLIENT_ID), ("scope", scope)])
        .header("Accept", "application/json")
        .send()
        .await?;
    if res.status().as_u16() >= 400 {
        return Err(AuthError::Msg(format!("device start: status {}", res.status())));
    }
    let mut dc: DeviceCode = res.json().await?;
    if dc.device_code.is_empty() || dc.user_code.is_empty() {
        return Err(AuthError::Msg("device start: bad response".into()));
    }
    if dc.interval == 0 {
        dc.interval = 5;
    }
    Ok(dc)
}

#[derive(Debug)]
pub enum Poll {
    Pending,
    Token(String),
}

/// Check authorization once: Pending while the user hasn't approved.
pub async fn poll_device_flow(token_url: &str, device_code: &str) -> Result<Poll, AuthError> {
    let res = client()
        .post(format!("{}/login/oauth/access_token", device_host(token_url)))
        .form(&[
            ("client_id", COPILOT_OAUTH_CLIENT_ID),
            ("device_code", device_code),
            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
        ])
        .header("Accept", "application/json")
        .send()
        .await?;
    let v: serde_json::Value = res.json().await?;
    if let Some(t) = v.get("access_token").and_then(|t| t.as_str()) {
        if !t.is_empty() {
            return Ok(Poll::Token(t.into()));
        }
    }
    match v.get("error").and_then(|e| e.as_str()).unwrap_or("") {
        "authorization_pending" | "slow_down" | "" => Ok(Poll::Pending),
        "expired_token" => Err(AuthError::Msg("device code expired; start over".into())),
        "access_denied" => Err(AuthError::Msg("authorization denied".into())),
        other => Err(AuthError::Msg(format!(
            "device poll: {other} {}",
            v.get("error_description").and_then(|d| d.as_str()).unwrap_or("")
        ))),
    }
}

fn token_store_dir() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".into());
    std::path::Path::new(&home).join(".config/lumina/tokens")
}

fn token_store_path(provider: &str) -> std::path::PathBuf {
    let safe: String = provider.chars().map(|c| if c.is_alphanumeric() { c } else { '_' }).collect();
    token_store_dir().join(format!("{safe}.json"))
}

/// Save a device-flow token (0600, never logged).
pub fn write_token_store(provider: &str, github_token: &str) -> Result<(), AuthError> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::create_dir_all(token_store_dir())?;
    #[allow(clippy::permissions_set_readonly_false)]
    let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(token_store_path(provider))?;
    use std::io::Write;
    f.write_all(serde_json::json!({"github_token": github_token}).to_string().as_bytes())?;
    Ok(())
}

/// Stored GitHub token, or "" when absent.
pub fn read_token_store(provider: &str) -> String {
    std::fs::read_to_string(token_store_path(provider))
        .ok()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        .and_then(|v| v.get("github_token").and_then(|t| t.as_str()).map(|s| s.to_string()))
        .unwrap_or_default()
}

/// Effective GitHub token: explicit config secret first, then the 0600 store.
pub fn effective_token(provider: &str, cfg: &crate::config::Config) -> String {
    if let Some(p) = cfg.providers.get(provider) {
        if !p.api_key_env.is_empty() {
            if let Ok(v) = std::env::var(&p.api_key_env) {
                if !v.is_empty() {
                    return v;
                }
            }
        }
        if !p.api_key_cmd.is_empty() {
            if let Ok(out) = std::process::Command::new("sh").arg("-c").arg(&p.api_key_cmd).output() {
                let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if !s.is_empty() {
                    return s;
                }
            }
        }
    }
    read_token_store(provider)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_host_shapes() {
        assert_eq!(device_host("https://api.github.com"), "https://github.com");
        assert_eq!(device_host("https://ghe.example.com/api/v3/token"), "https://ghe.example.com");
        assert_eq!(device_host(""), "https://");
    }
}
