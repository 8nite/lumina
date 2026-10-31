//! Local MCP client (stdio + SSE JSON-RPC): initialize → tools/list → tools/call.
//! Mirrors `internal/mcp`. Google URLs are rejected at dial time.

use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    io::{BufRead, Write},
    path::Path,
};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum McpError {
    #[error("mcp: {0}")]
    Msg(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Http(#[from] reqwest::Error),
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ServerConf {
    #[serde(default)]
    pub transport: String, // "stdio" | "sse"
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub env: HashMap<String, String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ConfigFile {
    #[serde(default)]
    pub servers: HashMap<String, ServerConf>,
}

impl ConfigFile {
    /// Workspace `.lumina/mcp.json` wins over `~/.config/lumina/mcp.json`.
    pub fn load(workspace: &Path) -> Self {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/root".into());
        let global = Path::new(&home).join(".config/lumina/mcp.json");
        let local = workspace.join(".lumina/mcp.json");
        for p in [local, global] {
            if let Ok(raw) = std::fs::read_to_string(&p) {
                if let Ok(cfg) = serde_json::from_str::<ConfigFile>(&raw) {
                    return cfg;
                }
            }
        }
        Self::default()
    }
}

#[derive(Debug, Serialize)]
struct RpcReq<'a> {
    jsonrpc: &'static str,
    id: u64,
    method: &'a str,
    params: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct RpcResp {
    #[serde(default)]
    result: serde_json::Value,
    #[serde(default)]
    error: Option<RpcErr>,
}

#[derive(Debug, Deserialize)]
struct RpcErr {
    #[serde(default)]
    message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tool {
    pub name: String,
    #[serde(default)]
    pub description: String,
}

enum Transport {
    Stdio { child: std::process::Child, reader: std::io::BufReader<std::process::ChildStdout> },
    Sse { url: String },
}

pub struct Client {
    transport: Transport,
    next_id: u64,
    http: Option<reqwest::blocking::Client>,
}

impl Client {
    /// Dial a server (Google URLs rejected here, before any network).
    pub fn dial(mut conf: ServerConf) -> Result<Self, McpError> {
        if conf.transport.is_empty() {
            conf.transport = "stdio".into();
        }
        if !conf.url.is_empty() && crate::config::is_google_url(&conf.url) {
            return Err(McpError::Msg("mcp: Google URLs are blocked".into()));
        }
        match conf.transport.as_str() {
            "stdio" => {
                if conf.command.is_empty() {
                    return Err(McpError::Msg("mcp: stdio server missing command".into()));
                }
                let mut cmd = std::process::Command::new(&conf.command);
                cmd.args(&conf.args).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped());
                for (k, v) in &conf.env {
                    cmd.env(k, v);
                }
                let mut child = cmd.spawn()?;
                let stdout = child.stdout.take().ok_or_else(|| McpError::Msg("mcp: no child stdout".into()))?;
                let mut c = Self {
                    transport: Transport::Stdio {
                        reader: std::io::BufReader::new(stdout),
                        child,
                    },
                    next_id: 0,
                    http: None,
                };
                c.call_raw(
                    "initialize",
                    serde_json::json!({
                        "protocolVersion": "2024-11-05",
                        "clientInfo": {"name": "lumina", "version": "0.1.0"},
                    }),
                )?;
                Ok(c)
            }
            "sse" => {
                if conf.url.is_empty() {
                    return Err(McpError::Msg("mcp: sse server missing url".into()));
                }
                Ok(Self {
                    transport: Transport::Sse { url: conf.url },
                    next_id: 0,
                    http: Some(
                        reqwest::blocking::Client::builder()
                            .user_agent("lumina/0.1.0")
                            .timeout(std::time::Duration::from_secs(30))
                            .build()?,
                    ),
                })
            }
            t => Err(McpError::Msg(format!("mcp: unknown transport {t:?}"))),
        }
    }

    fn call_raw(&mut self, method: &str, params: serde_json::Value) -> Result<serde_json::Value, McpError> {
        self.next_id += 1;
        let req = RpcReq { jsonrpc: "2.0", id: self.next_id, method, params };
        match &mut self.transport {
            Transport::Stdio { child, reader } => {
                let line = serde_json::to_string(&req)?;
                let stdin = child.stdin.as_mut().ok_or_else(|| McpError::Msg("mcp: child stdin closed".into()))?;
                stdin.write_all(line.as_bytes())?;
                stdin.write_all(b"\n")?;
                stdin.flush()?;
                let mut out = String::new();
                reader.read_line(&mut out)?;
                let resp: RpcResp = serde_json::from_str(&out)?;
                if let Some(e) = resp.error {
                    return Err(McpError::Msg(format!("mcp: {}", e.message)));
                }
                Ok(resp.result)
            }
            Transport::Sse { url } => {
                let http = self.http.as_ref().unwrap();
                let res: RpcResp = http.post(url.as_str()).json(&req).send()?.json()?;
                if let Some(e) = res.error {
                    return Err(McpError::Msg(format!("mcp: {}", e.message)));
                }
                Ok(res.result)
            }
        }
    }

    pub fn list_tools(&mut self) -> Result<Vec<Tool>, McpError> {
        let v = self.call_raw("tools/list", serde_json::json!({}))?;
        let tools = v
            .get("tools")
            .and_then(|t| serde_json::from_value(t.clone()).ok())
            .unwrap_or_default();
        Ok(tools)
    }

    pub fn call_tool(&mut self, name: &str, args: serde_json::Value) -> Result<String, McpError> {
        let v = self.call_raw("tools/call", serde_json::json!({"name": name, "arguments": args}))?;
        let mut sb = String::new();
        let mut is_error = false;
        if let Some(arr) = v.get("content").and_then(|c| c.as_array()) {
            for item in arr {
                if let Some(t) = item.get("text").and_then(|t| t.as_str()) {
                    sb.push_str(t);
                }
            }
        }
        if v.get("isError").and_then(|b| b.as_bool()).unwrap_or(false) {
            is_error = true;
        }
        if is_error {
            return Err(McpError::Msg(format!("mcp tool {name:?} failed: {sb}")));
        }
        Ok(sb)
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        if let Transport::Stdio { child, .. } = &mut self.transport {
            let _ = child.kill();
        }
    }
}
