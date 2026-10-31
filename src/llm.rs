//! Vendor-neutral streaming LLM adapters + 3-tier router.
//! Mirrors `internal/llm` (M1: openai-compatible, anthropic-compatible, ollama).
//! Copilot/Responses protocols stay 501 until claimed by a later milestone.

use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use thiserror::Error;
use tokio::sync::mpsc;

#[derive(Debug, Error)]
pub enum LlmError {
    #[error("llm: {0}")]
    Msg(String),
    #[error(transparent)]
    Http(#[from] reqwest::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Message {
    pub role: String,
    pub content: String,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub thinking: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub schema: serde_json::Value,
}

#[derive(Debug, Clone, Default)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub args: HashMap<String, serde_json::Value>,
    pub raw_args: String,
}

#[derive(Debug, Clone, Default)]
pub struct Chunk {
    pub delta: String,
    pub thinking: String,
    pub tool_calls: Vec<ToolCall>,
    pub done: bool,
}

#[derive(Debug, Clone, Default)]
pub struct Usage {
    pub input: u32,
    pub output: u32,
}

#[derive(Debug, Clone, Default)]
pub struct Request {
    pub model: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
    pub max_tokens: u32,
    pub reasoning_effort: String,
}

/// `"providerKey/model"` split; bare ids return ("", id).
pub fn split_model(s: &str) -> (String, String) {
    match s.split_once('/') {
        Some((k, m)) if !k.is_empty() && !m.is_empty() => (k.into(), m.into()),
        _ => (String::new(), s.into()),
    }
}

pub fn normalize_effort(s: &str) -> &str {
    match s.trim().to_lowercase().as_str() {
        "low" | "medium" | "high" | "xhigh" | "max" => {
            // ponytail: borrow the matching static without allocating.
            match s.trim().to_lowercase().as_str() {
                "low" => "low",
                "medium" => "medium",
                "high" => "high",
                "xhigh" => "xhigh",
                _ => "max",
            }
        }
        _ => "",
    }
}

pub fn valid_effort(s: &str) -> bool {
    matches!(
        s.trim().to_lowercase().as_str(),
        "" | "auto" | "low" | "medium" | "high" | "xhigh" | "max"
    )
}

/// Anthropic thinking budget ladder (must be >= 1024 when set).
pub fn effort_budget(effort: &str) -> u32 {
    match normalize_effort(effort) {
        "low" => 1024,
        "medium" => 4096,
        "high" => 10000,
        "xhigh" => 20000,
        "max" => 32000,
        _ => 0,
    }
}

#[derive(Debug, Clone)]
pub struct ProviderDef {
    pub protocol: String,
    pub base_url: String,
    pub api_key: String,
}

impl ProviderDef {
    fn kind(&self) -> &str {
        match self.protocol.as_str() {
            "" | "openai-compatible" => "openai-compatible",
            "anthropic-compatible" => "anthropic-compatible",
            "ollama" => "ollama",
            other => other,
        }
    }
}

/// Router maps tiers (fast | executor | planner | improver) to providers.
/// Built from config with no network at construction (mirrors Go `NewRouter`).
pub struct Router {
    pub fast: String,
    pub executor: String,
    pub planner: String,
    pub improver: String,
    pub providers: HashMap<String, ProviderDef>,
    /// In-memory model-persist overrides (file persist lands with config write support).
    overrides: std::sync::RwLock<HashMap<String, String>>,
}

impl Clone for Router {
    fn clone(&self) -> Self {
        Self {
            fast: self.fast.clone(),
            executor: self.executor.clone(),
            planner: self.planner.clone(),
            improver: self.improver.clone(),
            providers: self.providers.clone(),
            overrides: std::sync::RwLock::new(self.overrides.read().unwrap().clone()),
        }
    }
}

impl Router {
    pub fn new(cfg: &crate::config::Config) -> Result<Self, LlmError> {
        const PROTOCOLS: &[&str] = &[
            "openai-compatible",
            "openai-responses",
            "anthropic-compatible",
            "ollama",
            "copilot",
            "",
        ];
        let mut providers = HashMap::new();
        for (name, p) in &cfg.providers {
            if !PROTOCOLS.contains(&p.protocol.as_str()) {
                return Err(LlmError::Msg(format!(
                    "provider {name:?}: unknown protocol {:?}",
                    p.protocol
                )));
            }
            if crate::config::is_google_url(&p.base_url) {
                return Err(LlmError::Msg(format!("provider {name:?}: Google endpoints blocked")));
            }
            let key = cfg
                .resolve_secret(name)
                .map_err(|e| LlmError::Msg(e.to_string()))?
                .unwrap_or_default();
            let base = if p.base_url.is_empty() {
                match p.protocol.as_str() {
                    "anthropic-compatible" => "https://api.anthropic.com".into(),
                    "ollama" => "http://127.0.0.1:11434".into(),
                    _ => "http://127.0.0.1:11434/v1".into(),
                }
            } else {
                p.base_url.clone()
            };
            providers.insert(
                name.clone(),
                ProviderDef {
                    protocol: p.protocol.clone(),
                    base_url: base,
                    api_key: key,
                },
            );
        }
        providers.entry("local_ollama".into()).or_insert(ProviderDef {
            protocol: "ollama".into(),
            base_url: "http://127.0.0.1:11434".into(),
            api_key: String::new(),
        });
        Ok(Self {
            fast: cfg.router.fast.clone(),
            executor: cfg.router.executor.clone(),
            planner: cfg.router.planner.clone(),
            improver: cfg.router.improver.clone(),
            providers,
            overrides: std::sync::RwLock::new(HashMap::new()),
        })
    }

    fn tier_model(&self, role: &str) -> String {
        if let Some(o) = self.overrides.read().unwrap().get(role) {
            return o.clone();
        }
        match role {
            "fast" => &self.fast,
            "planner" => &self.planner,
            "improver" => &self.improver,
            _ => &self.executor,
        }
        .clone()
    }

    /// Tier resolution; planner falls back to executor (plan.md §8.2),
    /// and `plan` agent mode forces the planner tier for executor-role calls.
    pub fn model_for(&self, role: &str, agent_mode: &str) -> String {
        if agent_mode == "plan" && role == "executor" {
            let m = self.tier_model("planner");
            if !m.is_empty() {
                return m;
            }
        }
        let m = self.tier_model(role);
        if !m.is_empty() {
            return m;
        }
        self.tier_model("executor")
    }

    pub fn set_model(&self, role: &str, model: &str) {
        self.overrides.write().unwrap().insert(role.into(), model.into());
    }

    fn provider_for(&self, model: &str) -> Result<(ProviderDef, String), LlmError> {
        let (key, mid) = split_model(model);
        if !key.is_empty() {
            if let Some(p) = self.providers.get(&key) {
                return Ok((p.clone(), mid));
            }
        }
        let exec = self.tier_model("executor");
        let (ek, emid) = split_model(&exec);
        if !ek.is_empty() {
            if let Some(p) = self.providers.get(&ek) {
                return Ok((p.clone(), emid));
            }
        }
        self.providers
            .values()
            .next()
            .map(|p| (p.clone(), model.to_string()))
            .ok_or_else(|| LlmError::Msg(format!("llm: no provider for model {model:?}")))
    }

    /// Stream one completion through the tier's provider.
    pub async fn chat_stream(
        &self,
        role: &str,
        agent_mode: &str,
        mut req: Request,
        tx: mpsc::UnboundedSender<Chunk>,
    ) -> Result<Usage, LlmError> {
        if req.model.is_empty() {
            req.model = self.model_for(role, agent_mode);
        }
        let (def, mid) = self.provider_for(&req.model)?;
        req.model = mid;
        match def.kind() {
            "openai-compatible" => chat_openai(&def, req, tx).await,
            "anthropic-compatible" => chat_anthropic(&def, req, tx).await,
            "ollama" => chat_ollama(&def, req, tx).await,
            other => Err(LlmError::Msg(format!("protocol {other:?} not yet ported"))),
        }
    }
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent("lumina/0.1.0")
        .build()
        .expect("reqwest client")
}

fn auth_headers(def: &ProviderDef) -> reqwest::header::HeaderMap {
    let mut h = reqwest::header::HeaderMap::new();
    if !def.api_key.is_empty() {
        if let Ok(v) = format!("Bearer {}", def.api_key).parse() {
            h.insert(reqwest::header::AUTHORIZATION, v);
        }
    }
    h
}

fn openai_tools(tools: &[ToolSpec]) -> Vec<serde_json::Value> {
    tools
        .iter()
        .map(|t| {
            serde_json::json!({
                "type": "function",
                "function": {
                    "name": t.name,
                    "description": t.description,
                    "parameters": if t.schema.is_null() {
                        serde_json::json!({"type": "object"})
                    } else { t.schema.clone() }
                }
            })
        })
        .collect()
}

fn parse_args(raw: &str) -> HashMap<String, serde_json::Value> {
    serde_json::from_str::<serde_json::Value>(raw)
        .ok()
        .and_then(|v| {
            if let serde_json::Value::Object(m) = v {
                Some(m.into_iter().collect())
            } else {
                None
            }
        })
        .unwrap_or_default()
}

async fn chat_openai(
    def: &ProviderDef,
    req: Request,
    tx: mpsc::UnboundedSender<Chunk>,
) -> Result<Usage, LlmError> {
    let url = format!("{}/chat/completions", def.base_url.trim_end_matches('/'));
    let body = serde_json::json!({
        "model": req.model,
        "messages": req.messages.iter().map(|m| serde_json::json!({"role": m.role, "content": m.content})).collect::<Vec<_>>(),
        "tools": openai_tools(&req.tools),
        "stream": true,
        "stream_options": {"include_usage": true},
    });
    let res = client()
        .post(&url)
        .headers(auth_headers(def))
        .json(&body)
        .send()
        .await?;
    if !res.status().is_success() {
        let st = res.status();
        let t = res.text().await.unwrap_or_default();
        return Err(LlmError::Msg(format!("openai {url}: {st}: {t}")));
    }
    // Accumulate per-index tool-call fragments, then emit completed calls with Done.
    let mut arg_bufs: HashMap<u32, (String, String, String)> = HashMap::new(); // idx -> (id, name, args)
    let mut usage = Usage::default();
    let mut stream = res.bytes_stream();
    let mut buf = Vec::<u8>::new();
    while let Some(item) = stream.next().await {
        let bytes = item.map_err(LlmError::Http)?;
        buf.extend_from_slice(&bytes);
        while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = buf.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&line);
            let line = line.trim();
            let data = match line.strip_prefix("data:") {
                Some(d) => d.trim(),
                None => continue,
            };
            if data == "[DONE]" {
                continue;
            }
            let v: serde_json::Value = match serde_json::from_str(data) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if let Some(u) = v.get("usage") {
                usage.input = u.get("prompt_tokens").and_then(|n| n.as_u64()).unwrap_or(0) as u32;
                usage.output = u.get("completion_tokens").and_then(|n| n.as_u64()).unwrap_or(0) as u32;
            }
            let Some(choice) = v.get("choices").and_then(|c| c.get(0)) else {
                continue;
            };
            let delta = choice.get("delta").or_else(|| choice.get("message"));
            let Some(d) = delta else { continue };
            let mut text = d.get("content").and_then(|c| c.as_str()).unwrap_or("").to_string();
            let thinking = d
                .get("reasoning_content")
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .to_string();
            if let Some(tcs) = d.get("tool_calls").and_then(|t| t.as_array()) {
                for tc in tcs {
                    let idx = tc.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as u32;
                    let e = arg_bufs.entry(idx).or_insert_with(Default::default);
                    if let Some(id) = tc.get("id").and_then(|i| i.as_str()) {
                        e.0 = id.into();
                    }
                    if let Some(f) = tc.get("function") {
                        if let Some(n) = f.get("name").and_then(|n| n.as_str()) {
                            if !n.is_empty() {
                                e.1 = n.into();
                            }
                        }
                        if let Some(a) = f.get("arguments").and_then(|a| a.as_str()) {
                            e.2.push_str(a);
                        }
                    }
                    // Streaming partial names arrive before args; surface text immediately.
                    if !text.is_empty() || !thinking.is_empty() {
                        let _ = tx.send(Chunk {
                            delta: std::mem::take(&mut text),
                            thinking: thinking.clone(),
                            ..Default::default()
                        });
                    }
                }
                continue;
            }
            if text.is_empty() && thinking.is_empty() {
                continue;
            }
            let _ = tx.send(Chunk {
                delta: text,
                thinking,
                ..Default::default()
            });
        }
    }
    if !arg_bufs.is_empty() {
        let mut idxs: Vec<u32> = arg_bufs.keys().copied().collect();
        idxs.sort();
        let calls = idxs
            .into_iter()
            .map(|i| {
                let (id, name, raw) = arg_bufs.remove(&i).unwrap();
                let args = parse_args(&raw);
                ToolCall { id, name, args, raw_args: raw }
            })
            .collect();
        let _ = tx.send(Chunk { tool_calls: calls, done: true, ..Default::default() });
    } else {
        let _ = tx.send(Chunk { done: true, ..Default::default() });
    }
    Ok(usage)
}

async fn chat_anthropic(
    def: &ProviderDef,
    req: Request,
    tx: mpsc::UnboundedSender<Chunk>,
) -> Result<Usage, LlmError> {
    let url = format!("{}/v1/messages", def.base_url.trim_end_matches('/'));
    let system: Vec<_> = req
        .messages
        .iter()
        .filter(|m| m.role == "system")
        .map(|m| m.content.clone())
        .collect();
    let messages: Vec<_> = req
        .messages
        .iter()
        .filter(|m| m.role != "system")
        .map(|m| {
            serde_json::json!({"role": if m.role == "assistant" { "assistant" } else { "user" }, "content": m.content})
        })
        .collect();
    let mut body = serde_json::json!({
        "model": req.model,
        "max_tokens": if req.max_tokens == 0 { 4096 } else { req.max_tokens },
        "messages": messages,
        "stream": true,
    });
    if !system.is_empty() {
        body["system"] = serde_json::json!(system.join("\n"));
    }
    if !req.tools.is_empty() {
        body["tools"] = serde_json::json!(req.tools.iter().map(|t|
            serde_json::json!({"name": t.name, "description": t.description,
                "input_schema": if t.schema.is_null() { serde_json::json!({"type":"object"}) } else { t.schema.clone() }})
        ).collect::<Vec<_>>());
    }
    if effort_budget(&req.reasoning_effort) > 0 {
        body["thinking"] =
            serde_json::json!({"type": "enabled", "budget_tokens": effort_budget(&req.reasoning_effort)});
    }
    let res = client()
        .post(&url)
        .headers(auth_headers(def))
        .header("anthropic-version", "2023-06-01")
        .json(&body)
        .send()
        .await?;
    if !res.status().is_success() {
        let st = res.status();
        let t = res.text().await.unwrap_or_default();
        return Err(LlmError::Msg(format!("anthropic {url}: {st}: {t}")));
    }
    // Track input-JSON fragments per content index for tool_use blocks.
    let mut names: HashMap<u32, (String, String)> = HashMap::new(); // idx -> (id, name)
    let mut partials: HashMap<u32, String> = HashMap::new();
    let mut usage = Usage::default();
    let mut stream = res.bytes_stream();
    let mut buf = Vec::<u8>::new();
    let mut event = String::new();
    while let Some(item) = stream.next().await {
        let bytes = item.map_err(LlmError::Http)?;
        buf.extend_from_slice(&bytes);
        while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = buf.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&line).trim().to_string();
            if let Some(e) = line.strip_prefix("event:") {
                event = e.trim().to_string();
                continue;
            }
            let Some(data) = line.strip_prefix("data:") else { continue };
            let data = data.trim();
            let Ok(v) = serde_json::from_str::<serde_json::Value>(data) else { continue };
            match event.as_str() {
                "content_block_start" => {
                    let idx = v.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as u32;
                    if let Some(b) = v.get("content_block") {
                        if b.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                            names.insert(
                                idx,
                                (
                                    b.get("id").and_then(|i| i.as_str()).unwrap_or("").into(),
                                    b.get("name").and_then(|n| n.as_str()).unwrap_or("").into(),
                                ),
                            );
                            partials.entry(idx).or_default();
                        }
                    }
                }
                "content_block_delta" => {
                    let idx = v.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as u32;
                    let Some(d) = v.get("delta") else { continue };
                    match d.get("type").and_then(|t| t.as_str()) {
                        Some("text_delta") => {
                            let t = d.get("text").and_then(|t| t.as_str()).unwrap_or("");
                            if !t.is_empty() {
                                let _ = tx.send(Chunk { delta: t.into(), ..Default::default() });
                            }
                        }
                        Some("thinking_delta") => {
                            let t = d.get("thinking").and_then(|t| t.as_str()).unwrap_or("");
                            if !t.is_empty() {
                                let _ = tx.send(Chunk { thinking: t.into(), ..Default::default() });
                            }
                        }
                        Some("input_json_delta") => {
                            let p = d.get("partial_json").and_then(|p| p.as_str()).unwrap_or("");
                            partials.entry(idx).or_default().push_str(p);
                        }
                        _ => {}
                    }
                }
                "message_delta" => {
                    if let Some(u) = v.get("usage") {
                        usage.output = u.get("output_tokens").and_then(|n| n.as_u64()).unwrap_or(0) as u32;
                    }
                }
                "message_start" => {
                    if let Some(m) = v.get("message").and_then(|m| m.get("usage")) {
                        usage.input = m.get("input_tokens").and_then(|n| n.as_u64()).unwrap_or(0) as u32;
                    }
                }
                _ => {}
            }
        }
    }
    if !names.is_empty() {
        let mut idxs: Vec<u32> = names.keys().copied().collect();
        idxs.sort();
        let calls = idxs
            .into_iter()
            .map(|i| {
                let (id, name) = names.remove(&i).unwrap();
                let raw = partials.remove(&i).unwrap_or_default();
                let args = parse_args(&raw);
                ToolCall { id, name, args, raw_args: raw }
            })
            .collect();
        let _ = tx.send(Chunk { tool_calls: calls, done: true, ..Default::default() });
    } else {
        let _ = tx.send(Chunk { done: true, ..Default::default() });
    }
    Ok(usage)
}

async fn chat_ollama(
    def: &ProviderDef,
    req: Request,
    tx: mpsc::UnboundedSender<Chunk>,
) -> Result<Usage, LlmError> {
    let url = format!("{}/api/chat", def.base_url.trim_end_matches('/'));
    let body = serde_json::json!({
        "model": req.model,
        "messages": req.messages.iter().map(|m| serde_json::json!({"role": m.role, "content": m.content})).collect::<Vec<_>>(),
        "tools": req.tools.iter().map(|t| serde_json::json!({"type": "function", "function": {"name": t.name, "description": t.description,
            "parameters": if t.schema.is_null() { serde_json::json!({"type":"object"}) } else { t.schema.clone() }}})).collect::<Vec<_>>(),
        "stream": true,
    });
    let res = client().post(&url).json(&body).send().await?;
    if !res.status().is_success() {
        let st = res.status();
        let t = res.text().await.unwrap_or_default();
        return Err(LlmError::Msg(format!("ollama {url}: {st}: {t}")));
    }
    let mut calls = Vec::new();
    let mut stream = res.bytes_stream();
    let mut buf = Vec::<u8>::new();
    while let Some(item) = stream.next().await {
        let bytes = item.map_err(LlmError::Http)?;
        buf.extend_from_slice(&bytes);
        while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = buf.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&line).trim().to_string();
            if line.is_empty() {
                continue;
            }
            let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else { continue };
            let msg = v.get("message").unwrap_or(&v);
            let text = msg.get("content").and_then(|c| c.as_str()).unwrap_or("");
            let thinking = msg.get("thinking").and_then(|c| c.as_str()).unwrap_or("");
            if !text.is_empty() || !thinking.is_empty() {
                let _ = tx.send(Chunk { delta: text.into(), thinking: thinking.into(), ..Default::default() });
            }
            if let Some(tcs) = msg.get("tool_calls").and_then(|t| t.as_array()) {
                for tc in tcs {
                    let f = tc.get("function").unwrap_or(tc);
                    let name = f.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string();
                    let args = f.get("arguments").cloned().unwrap_or(serde_json::Value::Null);
                    let raw = args.to_string();
                    let map = if let serde_json::Value::Object(m) = args {
                        m.into_iter().collect()
                    } else {
                        HashMap::new()
                    };
                    calls.push(ToolCall { id: uuid::Uuid::new_v4().to_string(), name, args: map, raw_args: raw });
                }
            }
            if v.get("done").and_then(|d| d.as_bool()).unwrap_or(false) {
                if !calls.is_empty() {
                    let _ = tx.send(Chunk { tool_calls: std::mem::take(&mut calls), done: true, ..Default::default() });
                } else {
                    let _ = tx.send(Chunk { done: true, ..Default::default() });
                }
            }
        }
    }
    Ok(Usage::default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_model_shapes() {
        assert_eq!(split_model("openrouter/deepseek-r1"), ("openrouter".into(), "deepseek-r1".into()));
        assert_eq!(split_model("qwen"), ("".into(), "qwen".into()));
    }

    #[test]
    fn effort_ladder() {
        assert_eq!(normalize_effort(" XHigh "), "xhigh");
        assert_eq!(normalize_effort("bogus"), "");
        assert!(valid_effort("auto") && !valid_effort("ultra"));
        assert_eq!(effort_budget("low"), 1024);
        assert_eq!(effort_budget(""), 0);
    }

    #[test]
    fn openai_fragment_assembly() {
        // Simulate two SSE tool_call fragments reassembled by index.
        let mut bufs: HashMap<u32, (String, String, String)> = HashMap::new();
        for (idx, args) in [(0u32, "{\"path\": \"a"), (0u32, ".rs\"}")] {
            bufs.entry(idx).or_insert_with(Default::default).2.push_str(args);
        }
        bufs.get_mut(&0).unwrap().1 = "read".into();
        let (.., raw) = bufs.remove(&0).unwrap();
        let args = parse_args(&raw);
        assert_eq!(args.get("path").unwrap(), "a.rs");
    }

    #[test]
    fn planner_falls_back_to_executor() {
        let r = Router {
            fast: String::new(),
            executor: "o/m".into(),
            planner: String::new(),
            improver: String::new(),
            providers: [("o".into(), ProviderDef {
                protocol: "ollama".into(),
                base_url: "http://127.0.0.1:11434".into(),
                api_key: String::new(),
            })]
            .into_iter()
            .collect(),
            overrides: std::sync::RwLock::new(HashMap::new()),
        };
        assert_eq!(r.model_for("executor", "plan"), "o/m");
        assert_eq!(r.model_for("fast", "build"), "o/m");
    }
}

/// One live model (OpenRouter-style `context_length`; plain OpenAI has none → 0).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelMeta {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub context_window: u32,
}
fn is_zero(n: &u32) -> bool {
    *n == 0
}

/// Live model listing for pickers (`lumina models`, TUI /model, web picker).
/// Metadata failures mean "unknown" — never fail a turn over listing.
pub async fn list_models(def: &ProviderDef) -> Result<Vec<ModelMeta>, LlmError> {
    match def.kind() {
        "openai-compatible" => {
            let url = format!("{}/models", def.base_url.trim_end_matches('/'));
            let res = client().get(&url).headers(auth_headers(def)).send().await?;
            if !res.status().is_success() {
                return Err(LlmError::Msg(format!("models {url}: {}", res.status())));
            }
            let v: serde_json::Value = res.json().await?;
            let mut out = Vec::new();
            if let Some(arr) = v.get("data").and_then(|d| d.as_array()) {
                for m in arr {
                    let id = m.get("id").and_then(|i| i.as_str()).unwrap_or("").to_string();
                    if id.is_empty() {
                        continue;
                    }
                    out.push(ModelMeta {
                        name: id.clone(),
                        id,
                        context_window: m
                            .get("context_length")
                            .or_else(|| m.get("context_window"))
                            .and_then(|n| n.as_u64())
                            .unwrap_or(0) as u32,
                    });
                }
            }
            out.sort_by(|a, b| a.id.cmp(&b.id));
            Ok(out)
        }
        "ollama" => {
            let url = format!("{}/api/tags", def.base_url.trim_end_matches('/'));
            let res = client().get(&url).send().await?;
            if !res.status().is_success() {
                return Err(LlmError::Msg(format!("models {url}: {}", res.status())));
            }
            let v: serde_json::Value = res.json().await?;
            let mut out = Vec::new();
            if let Some(arr) = v.get("models").and_then(|d| d.as_array()) {
                for m in arr {
                    let name = m.get("name").and_then(|i| i.as_str()).unwrap_or("").to_string();
                    if !name.is_empty() {
                        out.push(ModelMeta { id: name.clone(), name, context_window: 0 });
                    }
                }
            }
            out.sort_by(|a, b| a.id.cmp(&b.id));
            Ok(out)
        }
        _ => Ok(Vec::new()),
    }
}
