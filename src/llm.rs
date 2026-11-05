//! Vendor-neutral streaming LLM adapters + 3-tier router.
//! Protocols: openai-compatible, anthropic-compatible, ollama, copilot,
//! openai-responses.

use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use thiserror::Error;
use tokio::sync::mpsc;

/// Per-request LLM ceiling: a silent gateway stream fails the turn instead
/// of hanging it forever (the drain loop only ends on a terminal chunk).
pub const STREAM_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);

/// Probe-verified usable Copilot model ids + when they were verified.
type CopilotUsable = HashMap<String, (HashSet<String>, std::time::Instant)>;

/// Probe-verified Copilot usability cache TTL: entitlements change rarely,
/// and each refresh costs one tiny chat probe per catalog entry.
const COPILOT_USABLE_TTL: std::time::Duration = std::time::Duration::from_secs(3600);

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
    #[allow(dead_code)]
    pub id: String,
    pub name: String,
    pub args: HashMap<String, serde_json::Value>,
    #[allow(dead_code)]
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
    /// Prompt tokens served from provider prefix cache (Anthropic
    /// cache_read_input_tokens, OpenAI prompt_tokens_details.cached_tokens).
    /// Carried separately so cost math can price them at cache rates.
    pub cache_read: u32,
    /// Prompt tokens written to the provider cache on this request
    /// (Anthropic cache_creation_input_tokens). Zero on most providers.
    pub cache_write: u32,
}

impl Usage {
    pub fn add(&mut self, o: &Usage) {
        self.input = self.input.saturating_add(o.input);
        self.output = self.output.saturating_add(o.output);
        self.cache_read = self.cache_read.saturating_add(o.cache_read);
        self.cache_write = self.cache_write.saturating_add(o.cache_write);
    }
    /// Whole-percent prefix-cache hit rate for this usage slice (0 when empty).
    pub fn cache_hit_pct(&self) -> u32 {
        if self.input == 0 {
            return 0;
        }
        (self.cache_read.min(self.input) as u64 * 100 / self.input as u64) as u32
    }
}

/// Extract cached-token counts from an OpenAI-style `usage` object.
/// Providers disagree on the key (`cached_tokens` under
/// `prompt_tokens_details`, flat `prompt_cache_hit_tokens`, …) — accept all.
pub fn openai_cached_tokens(u: &serde_json::Value) -> (u32, u32) {
    let mut read = 0u32;
    let mut write = 0u32;
    if let Some(d) = u.get("prompt_tokens_details") {
        read = read.saturating_add(d.get("cached_tokens").and_then(|n| n.as_u64()).unwrap_or(0) as u32);
    }
    // Some gateways flatten the detail one level up.
    read = read.saturating_add(u.get("cached_tokens").and_then(|n| n.as_u64()).unwrap_or(0) as u32);
    read = read.saturating_add(u.get("prompt_cache_hit_tokens").and_then(|n| n.as_u64()).unwrap_or(0) as u32);
    read = read.saturating_add(u.get("prefix_cached_tokens").and_then(|n| n.as_u64()).unwrap_or(0) as u32);
    write = write.saturating_add(u.get("prompt_cache_miss_tokens").and_then(|n| n.as_u64()).unwrap_or(0) as u32);
    (read, write)
}

#[derive(Debug, Clone, Default)]
pub struct Request {
    pub model: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
    pub max_tokens: u32,
    pub reasoning_effort: String,
    /// Attached images (base64 payloads, never paths; 8MB cap enforced by callers).
    pub images: Vec<ImageAttachment>,
}

/// One attached image (vision-capable models only).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ImageAttachment {
    pub name: String,
    pub media_type: String,
    pub data: String, // raw base64 (no data: prefix)
}

/// `"providerKey/model"` split; bare ids return ("", id).
pub fn split_model(s: &str) -> (String, String) {
    match s.split_once('/') {
        Some((k, m)) if !k.is_empty() && !m.is_empty() => (k.into(), m.into()),
        _ => (String::new(), s.into()),
    }
}

/// Canonical reasoning ladder, low to high: none < minimal < low < medium <
/// high < xhigh < max. This is the union of provider API vocabularies;
/// per-model SELECTABILITY comes from
/// each model's advertised `supported_efforts` (see `efforts_or_default`),
/// never from this list alone.
pub fn normalize_effort(s: &str) -> &str {
    match s.trim().to_lowercase().as_str() {
        "low" | "medium" | "high" | "xhigh" | "max" | "minimal" | "none" => {
            // ponytail: borrow the matching static without allocating.
            match s.trim().to_lowercase().as_str() {
                "low" => "low",
                "medium" => "medium",
                "high" => "high",
                "xhigh" => "xhigh",
                "minimal" => "minimal",
                "none" => "none",
                _ => "max",
            }
        }
        _ => "",
    }
}

pub fn valid_effort(s: &str) -> bool {
    matches!(
        s.trim().to_lowercase().as_str(),
        "" | "auto" | "none" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max"
    )
}

/// Render "" as "auto" for pills and pickers (Go EffortLabel parity).
/// Used by TUI pickers + tests.
#[allow(dead_code)]
pub fn effort_label(s: &str) -> &str {
    match normalize_effort(s) {
        "" => "auto",
        "none" => "none",
        "minimal" => "minimal",
        "low" => "low",
        "medium" => "medium",
        "high" => "high",
        "xhigh" => "xhigh",
        _ => "max",
    }
}

/// Anthropic thinking budget ladder (must be >= 1024 when set).
/// Position on the reasoning ladder (auto/"" = -1, provider default).
/// `none` budgets 0 (thinking omitted); `minimal` takes the API floor.
/// Live in TUI pickers + tests (`#[cfg(test)]` only in this binary).
#[allow(dead_code)]
pub fn effort_index(effort: &str) -> i32 {
    match normalize_effort(effort) {
        "" => -1,
        "none" => 0,
        "minimal" => 1,
        "low" => 2,
        "medium" => 3,
        "high" => 4,
        "xhigh" => 5,
        "max" => 6,
        _ => -1,
    }
}

/// Advertised ladder with "auto" prepended, or the global ladder when the
/// provider advertises nothing (empty = unknown). Advertised tokens pass
/// through verbatim (deduped) — never filtered to a hardcoded subset — so
/// provider vocabularies like `minimal`/`none` survive to pickers and wire.
#[allow(dead_code)]
pub fn efforts_or_default(supported: &[String]) -> Vec<String> {
    const LADDER: [&str; 8] = ["auto", "none", "minimal", "low", "medium", "high", "xhigh", "max"];
    if supported.is_empty() {
        return LADDER.iter().map(|s| s.to_string()).collect();
    }
    let mut out = vec!["auto".to_string()];
    for s in supported {
        let v = normalize_effort(s);
        if v.is_empty() || out.iter().any(|o| o == v) {
            continue;
        }
        out.push(v.to_string());
    }
    if out.len() == 1 {
        return LADDER.iter().map(|s| s.to_string()).collect();
    }
    out
}

/// Clamp an effort into the advertised ladder: above the top clamps down,
/// below the bottom (or unknown) clamps up to the lowest supported.
#[allow(dead_code)]
pub fn clamp_effort(effort: &str, supported: &[String]) -> String {
    let ladder = efforts_or_default(supported);
    let want = normalize_effort(effort);
    if ladder.iter().any(|v| v == want) {
        return want.to_string();
    }
    let idx = effort_index(want);
    // Walk down from the top to the highest supported rung at or below want.
    for v in ladder.iter().rev() {
        if effort_index(v) <= idx && *v != "auto" {
            return v.clone();
        }
    }
    ladder.into_iter().find(|v| v != "auto").unwrap_or_default()
}

pub fn effort_budget(effort: &str) -> u32 {
    match normalize_effort(effort) {
        "low" => 1024,
        "medium" => 4096,
        "high" => 10000,
        "xhigh" => 20000,
        "max" => 32000,
        // minimal takes the API floor; none budgets 0 (thinking omitted).
        "minimal" => 1024,
        _ => 0,
    }
}

/// One rung down the reasoning ladder; None at/below the floor ("",
/// provider default). Steps down after an effort-rejection 400 instead of
/// failing the turn on a stale pick or an unadvertised ladder.
pub fn effort_step_down(effort: &str) -> Option<&'static str> {
    Some(match normalize_effort(effort) {
        "max" => "xhigh",
        "xhigh" => "high",
        "high" => "medium",
        "medium" => "low",
        "low" => "minimal",
        "minimal" => "none",
        "none" => "",
        _ => return None,
    })
}

/// True when a 400 body blames the reasoning effort:
/// "does not support reasoning_effort='max'"). Transport-level signal for
/// the step-down retry; values are never parsed out of the message.
pub fn is_effort_error(body: &str) -> bool {
    let b = body.to_lowercase();
    b.contains("reasoning")
        && (b.contains("effort") || b.contains("support") || b.contains("invalid"))
}

#[derive(Debug, Clone)]
pub struct ProviderDef {
    pub protocol: String,
    pub base_url: String,
    pub api_key: String,
    /// Copilot only: long-lived GitHub token (exchange at call time).
    pub github_token: String,
    /// Copilot/GHES only: token exchange endpoint override.
    pub token_url: String,
}

impl ProviderDef {
    fn kind(&self) -> &str {
        match self.protocol.as_str() {
            "" | "openai-compatible" => "openai-compatible",
            "openai-responses" | "responses" => "openai-responses",
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
    /// Exchanged Copilot session tokens by provider name (short-lived, ~25min).
    copilot_sessions: std::sync::RwLock<HashMap<String, CopilotSession>>,
    /// Probe-verified usable Copilot models by provider (the catalog lists
    /// entitlement-dead entries that 400 at chat time; verified 12/47 usable
    /// 2026-11-03). Stale-while-revalidate: pickers serve the cache when
    /// fresh, otherwise refresh in the background (see list_models).
    /// Arc-shared: Router::clone must share these (the refresh task writes
    /// through a clone; a deep copy would sink the write).
    copilot_usable: std::sync::Arc<std::sync::RwLock<CopilotUsable>>,
    /// In-flight usability refreshes (one per provider; no probe storms).
    copilot_refreshing: std::sync::Arc<std::sync::RwLock<HashSet<String>>>,
    /// Advertised context windows by (protocol, base_url, model).
    ctx_cache: std::sync::RwLock<HashMap<String, u32>>,
}

#[derive(Debug, Clone)]
struct CopilotSession {
    token: String,
    expires_unix: i64,
    base_url: String,
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
            copilot_sessions: std::sync::RwLock::new(self.copilot_sessions.read().unwrap().clone()),
            copilot_usable: self.copilot_usable.clone(),
            copilot_refreshing: self.copilot_refreshing.clone(),
            ctx_cache: std::sync::RwLock::new(self.ctx_cache.read().unwrap().clone()),
        }
    }
}

impl Router {
    pub fn new(cfg: &crate::config::Config) -> Result<Self, LlmError> {
        const PROTOCOLS: &[&str] = &[
            "openai-compatible",
            "openai-responses",
            "responses",
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
                    api_key: if p.protocol == "copilot" { String::new() } else { key.clone() },
                    github_token: if p.protocol == "copilot" {
                        crate::copilot::effective_token(name, cfg)
                    } else {
                        String::new()
                    },
                    token_url: if p.protocol == "copilot" {
                        if !p.token_url.is_empty() {
                            p.token_url.clone()
                        } else {
                            std::env::var(format!("{}_TOKEN_URL", name.to_uppercase())).unwrap_or_default()
                        }
                    } else {
                        String::new()
                    },
                },
            );
        }
        providers.entry("local_ollama".into()).or_insert(ProviderDef {
            protocol: "ollama".into(),
            base_url: "http://127.0.0.1:11434".into(),
            api_key: String::new(),
            github_token: String::new(),
            token_url: String::new(),
        });
        Ok(Self {
            fast: cfg.router.fast.clone(),
            executor: cfg.router.executor.clone(),
            planner: cfg.router.planner.clone(),
            improver: cfg.router.improver.clone(),
            providers,
            overrides: std::sync::RwLock::new(HashMap::new()),
            copilot_sessions: std::sync::RwLock::new(HashMap::new()),
            copilot_usable: std::sync::Arc::new(std::sync::RwLock::new(HashMap::new())),
            copilot_refreshing: std::sync::Arc::new(std::sync::RwLock::new(HashSet::new())),
            ctx_cache: std::sync::RwLock::new(HashMap::new()),
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

    pub(crate) fn provider_for(&self, model: &str) -> Result<(String, ProviderDef, String), LlmError> {
        let (key, mid) = split_model(model);
        if !key.is_empty() {
            if let Some(p) = self.providers.get(&key) {
                return Ok((key, p.clone(), mid));
            }
        }
        let exec = self.tier_model("executor");
        let (ek, emid) = split_model(&exec);
        if !ek.is_empty() {
            if let Some(p) = self.providers.get(&ek) {
                return Ok((ek, p.clone(), emid));
            }
        }
        self.providers
            .iter()
            .next()
            .map(|(k, p)| (k.clone(), p.clone(), model.to_string()))
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
        let (key, def, mid) = self.provider_for(&req.model)?;
        req.model = mid;
        match def.kind() {
            "openai-compatible" => chat_openai(&def, req, tx, &[]).await,
            "openai-responses" => chat_responses(&def, req, tx).await,
            "anthropic-compatible" => chat_anthropic(&def, req, tx).await,
            "ollama" => chat_ollama(&def, req, tx).await,
            "copilot" => {
                let sess = self.copilot_session(&key, &def).await?;
                let mut cdef = def.clone();
                cdef.base_url = sess.base_url.clone();
                cdef.api_key = sess.token.clone();
                chat_openai(&cdef, req, tx, &copilot_headers()).await
            }
            other => Err(LlmError::Msg(format!("protocol {other:?} not yet ported"))),
        }
    }

    /// Short-lived Copilot session (cached, refreshed 5min before expiry).
    async fn copilot_session(&self, key: &str, def: &ProviderDef) -> Result<CopilotSession, LlmError> {
        if let Some(s) = self.copilot_sessions.read().unwrap().get(key) {
            if chrono::Utc::now().timestamp() + 300 < s.expires_unix {
                return Ok(s.clone());
            }
        }
        let token_url = if def.token_url.is_empty() {
            "https://api.github.com/copilot_internal/v2/token".to_string()
        } else {
            def.token_url.clone()
        };
        if def.github_token.is_empty() {
            return Err(LlmError::Msg(
                "copilot: no github token (api_key_env/api_key_cmd, device auth, or token store)".into(),
            ));
        }
        match Self::exchange_copilot(&token_url, &def.base_url, &def.github_token).await {
            Ok(sess) => {
                self.copilot_sessions.write().unwrap().insert(key.into(), sess.clone());
                Ok(sess)
            }
            Err(e) => {
                // A construction-time key can be valid for the API yet
                // rejected by the exchange (classic PATs 404): fall back to
                // the device-authed store token when it differs.
                let stored = crate::copilot::read_token_store(key);
                if !stored.is_empty() && stored != def.github_token {
                    if let Ok(sess) = Self::exchange_copilot(&token_url, &def.base_url, &stored).await {
                        self.copilot_sessions.write().unwrap().insert(key.into(), sess.clone());
                        return Ok(sess);
                    }
                }
                Err(e)
            }
        }
    }

    async fn exchange_copilot(token_url: &str, base_url: &str, github_token: &str) -> Result<CopilotSession, LlmError> {
        let mut req = client().get(token_url).bearer_auth(github_token);
        for (k, v) in copilot_headers() {
            req = req.header(k, v);
        }
        let res = req.send().await?;
        if !res.status().is_success() {
            return Err(LlmError::Msg(format!("copilot token exchange: status {}", res.status())));
        }
        let v: serde_json::Value = res.json().await?;
        let token = v.get("token").and_then(|t| t.as_str()).unwrap_or("").to_string();
        if token.is_empty() {
            return Err(LlmError::Msg("copilot token exchange: empty token".into()));
        }
        let mut exp = v.get("expires_at").and_then(|e| e.as_i64()).unwrap_or(0);
        if exp == 0 {
            exp = chrono::Utc::now().timestamp()
                + v.get("expires_in").and_then(|e| e.as_i64()).unwrap_or(1500);
        }
        let base = if !base_url.is_empty() && base_url != "http://127.0.0.1:11434/v1" {
            base_url.trim_end_matches('/').to_string()
        } else if let Some(p) = v.get("proxy-ep").and_then(|p| p.as_str()) {
            format!("https://{}", p.trim_end_matches('/'))
        } else if let Some(api) = v.get("endpoints").and_then(|e| e.get("api")).and_then(|a| a.as_str()) {
            api.trim_end_matches('/').to_string()
        } else {
            "https://api.githubcopilot.com".to_string()
        };
        Ok(CopilotSession { token, expires_unix: exp, base_url: base })
    }

    /// Drop a cached session so the next call re-exchanges immediately
    /// (SetKey parity: fresh device-auth token takes effect at once).
    pub fn drop_copilot_session(&self, key: &str) {
        self.copilot_sessions.write().unwrap().remove(key);
    }

    /// Provider-advertised context window, 0 when unknown (metadata failures
    /// never propagate — callers show used-only gauges instead).
    pub async fn context_window(&self, model: &str) -> u32 {
        let Ok((_, def, mid)) = self.provider_for(model) else { return 0 };
        let cache_key = format!("{}|{}|{mid}", def.protocol, def.base_url);
        if let Some(&w) = self.ctx_cache.read().unwrap().get(&cache_key) {
            return w;
        }
        let w = match def.kind() {
            "openai-compatible" | "copilot" => self.openai_window(&def, &mid).await,
            "anthropic-compatible" | "openai-responses" | "responses" => self.openai_window(&def, &mid).await,
            "ollama" => ollama_window(&def, &mid).await,
            _ => 0,
        };
        self.ctx_cache.write().unwrap().insert(cache_key, w);
        w
    }

    async fn openai_window(&self, def: &ProviderDef, model: &str) -> u32 {
        // Copilot needs an exchanged token for /models; best-effort only.
        // Anthropic-compatible gateways list at /v1/models (Go parity).
        let (base, key) = if def.kind() == "copilot" {
            let Ok(sess) = self.copilot_session(&self.provider_key_for(def), def).await else { return 0 };
            (sess.base_url, sess.token)
        } else if def.kind() == "anthropic-compatible" {
            (format!("{}/v1", def.base_url.trim_end_matches('/')), def.api_key.clone())
        } else {
            (def.base_url.clone(), def.api_key.clone())
        };
        let mut tmp = def.clone();
        tmp.base_url = base;
        tmp.api_key = key;
        openai_models(&tmp, &[])
            .await
            .unwrap_or_default()
            .iter()
            .find(|m| m.id == model)
            .map(|m| m.context_window)
            .unwrap_or(0)
    }

    fn provider_key_for(&self, def: &ProviderDef) -> String {
        self.providers
            .iter()
            .find(|(_, p)| p.base_url == def.base_url && p.protocol == def.protocol)
            .map(|(k, _)| k.clone())
            .unwrap_or_default()
    }

    /// Live model listing (TUI /model, web picker, `lumina models`).
    pub async fn list_models(&self, provider: &str) -> Result<Vec<ModelMeta>, LlmError> {
        let Some(def) = self.providers.get(provider).cloned() else {
            return Err(LlmError::Msg(format!("unknown provider {provider:?}")));
        };
        match def.kind() {
            "openai-compatible" => openai_models(&def, &[]).await,
            "openai-responses" | "responses" => openai_models(&def, &[]).await,
            "anthropic-compatible" => {
                let mut tmp = def.clone();
                tmp.base_url = format!("{}/v1", def.base_url.trim_end_matches('/'));
                openai_models(&tmp, &[]).await
            },
            "copilot" => {
                let sess = self.copilot_session(provider, &def).await?;
                let url = format!("{}/models", sess.base_url.trim_end_matches('/'));
                let mut req = client().get(&url).bearer_auth(&sess.token);
                for (k, v) in copilot_headers() {
                    req = req.header(k, v.clone());
                }
                let res = req.send().await?;
                if !res.status().is_success() {
                    return Err(LlmError::Msg(format!("models {url}: {}", res.status())));
                }
                let v: serde_json::Value = res.json().await?;
                // Structural filter first: entries advertising endpoints
                // without /chat/completions (Responses-only) can never ride
                // our chat path. Legacy entries omit the field; the probe
                // cache decides those (fail-open until verified).
                let compat: Vec<ModelMeta> = openai_models_from_value(&v)
                    .into_iter()
                    .filter(|m| {
                        copilot_chat_compatible(
                            catalog_endpoints(&v, &m.id).as_ref(),
                        )
                    })
                    .collect();
                // Probe-verified usability, stale-while-revalidate: a fresh
                // cache hides entitlement-dead entries (400
                // model_not_supported at chat time); otherwise refresh in the
                // background and serve the structural list meanwhile.
                if let Some(set) = self.copilot_usable_fresh(provider) {
                    Ok(compat.into_iter().filter(|m| set.contains(&m.id)).collect())
                } else {
                    let ids: Vec<String> = compat.iter().map(|m| m.id.clone()).collect();
                    self.spawn_copilot_refresh(
                        provider.to_string(),
                        sess.base_url.clone(),
                        sess.token.clone(),
                        ids,
                    );
                    Ok(compat)
                }
            }
            "ollama" => ollama_models(&def).await,
            _ => Ok(Vec::new()),
        }
    }

    /// Fresh probe-verified usable set, if any (fail-open: stale/missing
    /// means the caller serves the structural list + refreshes).
    fn copilot_usable_fresh(&self, provider: &str) -> Option<HashSet<String>> {
        self.copilot_usable.read().unwrap().get(provider).and_then(|(set, at)| {
            if at.elapsed() < COPILOT_USABLE_TTL {
                Some(set.clone())
            } else {
                None
            }
        })
    }

    /// Refresh usability in the background (one in flight per provider).
    /// Never blocks pickers; a total outage skips the write so the next
    /// listing retries instead of poisoning the cache.
    fn spawn_copilot_refresh(&self, provider: String, base: String, token: String, ids: Vec<String>) {
        if !self.copilot_refreshing.write().unwrap().insert(provider.clone()) {
            return;
        }
        let this = self.clone();
        tokio::spawn(async move {
            let out = probe_copilot_usable(&base, &token, &ids).await;
            this.copilot_refreshing.write().unwrap().remove(&provider);
            if out.signalled {
                this.copilot_usable
                    .write()
                    .unwrap()
                    .insert(provider, (out.ids, std::time::Instant::now()));
            }
        });
    }
}

/// Headers identifying us to Copilot gateways (mirrors Go copilotHeaders).
fn copilot_headers() -> Vec<(&'static str, String)> {
    vec![
        ("User-Agent", "GitHubCopilotChat/0.35.0".into()),
        ("Editor-Version", "vscode/1.107.0".into()),
        ("Editor-Plugin-Version", "copilot-chat/0.35.0".into()),
        ("Copilot-Integration-Id", "vscode-chat".into()),
        // Chat-time headers (copilot.go parity): required for the
        // conversation-edits path on some Copilot gateways.
        ("Accept", "text/event-stream".into()),
        ("Openai-Intent", "conversation-edits".into()),
        ("X-Initiator", "agent".into()),
    ]
}

fn client() -> reqwest::Client {
    static ONCE: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    ONCE.get_or_init(|| {
        reqwest::Client::builder()
            .user_agent("lumina/0.1.0")
            .build()
            .expect("reqwest client")
    })
    .clone()
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

/// Drain accumulated OpenAI tool-call fragments into one done chunk.
fn flush_openai_calls(arg_bufs: &mut HashMap<u32, (String, String, String)>, tx: &mpsc::UnboundedSender<Chunk>) {
    if arg_bufs.is_empty() {
        let _ = tx.send(Chunk { done: true, ..Default::default() });
        return;
    }
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

/// OpenCode `model-retry` parity: retryable provider failures (429, 529,
/// 5xx, transport timeouts/connects) retry with capped exponential backoff
/// instead of failing the turn. Everything else surfaces immediately.
fn is_retryable_status(st: u16) -> bool {
    st == 429 || st == 529 || (500..=599).contains(&st)
}

fn retry_delay(attempt: u32) -> std::time::Duration {
    std::time::Duration::from_millis((2000u64 << attempt.min(3)).min(10_000))
}

fn is_retryable_transport(e: &reqwest::Error) -> bool {
    e.is_timeout() || e.is_connect()
}

/// POST with retry: `make` builds a fresh request per attempt (builders are
/// single-use). Returns the first non-retryable response, or the last
/// retryable response / transport error once the budget (5 retries) is spent.
async fn post_with_retry<F>(mut make: F, label: &str) -> Result<reqwest::Response, LlmError>
where
    F: FnMut() -> reqwest::RequestBuilder,
{
    let mut attempt = 0u32;
    loop {
        match make().send().await {
            Ok(res) if !is_retryable_status(res.status().as_u16()) => return Ok(res),
            Ok(res) if attempt >= 5 => {
                let st = res.status();
                let t = res.text().await.unwrap_or_default();
                return Err(LlmError::Msg(format!("{label}: {st}: {t}")));
            }
            Ok(_) => {}
            Err(e) if is_retryable_transport(&e) && attempt < 5 => {}
            Err(e) => return Err(LlmError::Http(e)),
        }
        tracing::warn!("{label}: retryable provider failure, attempt {}/5", attempt + 1);
        tokio::time::sleep(retry_delay(attempt)).await;
        attempt += 1;
    }
}

async fn chat_openai(
    def: &ProviderDef,
    req: Request,
    tx: mpsc::UnboundedSender<Chunk>,
    extra_headers: &[(&str, String)],
) -> Result<Usage, LlmError> {
    let url = format!("{}/chat/completions", def.base_url.trim_end_matches('/'));
    // Vision rides the LAST user message only (Go parity); earlier user
    // turns stay text so the model doesn't re-see stale images every round.
    let last_user_idx = req.messages.iter().rposition(|m| m.role == "user");
    let messages: Vec<serde_json::Value> = req
        .messages
        .iter()
        .enumerate()
        .map(|(i, m)| {
            if req.images.is_empty() || Some(i) != last_user_idx {
                return serde_json::json!({"role": m.role, "content": m.content});
            }
            // Vision shape: text part + one image_url part per attachment.
            let mut parts = vec![serde_json::json!({"type": "text", "text": m.content})];
            for im in &req.images {
                let mt = if im.media_type.is_empty() { "image/png" } else { &im.media_type };
                parts.push(serde_json::json!({"type": "image_url",
                    "image_url": {"url": format!("data:{mt};base64,{}", im.data)}}));
            }
            serde_json::json!({"role": m.role, "content": parts})
        })
        .collect();
    // Go parity: only send tools/max_tokens/reasoning_effort when set.
    let mut body = serde_json::json!({
        "model": req.model,
        "messages": messages,
        "stream": true,
        "stream_options": {"include_usage": true},
    });
    if req.max_tokens > 0 {
        body["max_tokens"] = serde_json::json!(req.max_tokens);
    }
    if !normalize_effort(&req.reasoning_effort).is_empty() {
        body["reasoning_effort"] = serde_json::json!(normalize_effort(&req.reasoning_effort));
    }
    if !req.tools.is_empty() {
        body["tools"] = serde_json::Value::Array(openai_tools(&req.tools));
    }
    let mut http = client().post(&url).headers(auth_headers(def));
    for (k, v) in extra_headers {
        http = http.header(*k, v.clone());
    }
    // Session routing (stable per request).
    // Effort step-down (responses parity): retry down the ladder on an
    // effort-rejection 400 instead of failing the turn.
    let sid = uuid::Uuid::new_v4().to_string();
    let mut effort = normalize_effort(&req.reasoning_effort).to_string();
    let mut retries = 0u32;
    let res = loop {
        if effort.is_empty() {
            if let Some(o) = body.as_object_mut() {
                o.remove("reasoning_effort");
            }
        } else {
            body["reasoning_effort"] = serde_json::json!(effort);
        }
        let mut http = client().post(&url).headers(auth_headers(def));
        for (k, v) in extra_headers {
            http = http.header(*k, v.clone());
        }
        http = http.header("x-lumina-session", &sid).header("x-opencode-session", &sid);
        let res = match http.json(&body).send().await {
            Ok(r) if !is_retryable_status(r.status().as_u16()) => r,
            Ok(r) if retries >= 5 => r,
            Ok(_) => {
                tracing::warn!("openai: retryable provider failure, attempt {}/5", retries + 1);
                tokio::time::sleep(retry_delay(retries)).await;
                retries += 1;
                continue;
            }
            Err(e) if is_retryable_transport(&e) && retries < 5 => {
                tracing::warn!("openai: retryable transport failure, attempt {}/5", retries + 1);
                tokio::time::sleep(retry_delay(retries)).await;
                retries += 1;
                continue;
            }
            Err(e) => return Err(LlmError::Http(e)),
        };
        if res.status().is_success() {
            break res;
        }
        let st = res.status();
        let t = res.text().await.unwrap_or_default();
        if st.as_u16() == 400 && !effort.is_empty() && is_effort_error(&t) {
            if let Some(down) = effort_step_down(&effort) {
                effort = down.to_string();
                retries = 0;
                continue;
            }
        }
        return Err(LlmError::Msg(format!("openai {url}: {st}: {t}")));
    };
    // Accumulate per-index tool-call fragments, then emit completed calls with Done.
    let mut arg_bufs: HashMap<u32, (String, String, String)> = HashMap::new(); // idx -> (id, name, args)
    let mut usage = Usage::default();
    let mut stream = res.bytes_stream();
    let mut buf = Vec::<u8>::new();
    // SSE dispatch: continued data: lines join until a blank line (readSSE).
    let mut acc = String::new();
    let mut fr_done = false;
    while let Some(item) = stream.next().await {
        let bytes = item.map_err(LlmError::Http)?;
        buf.extend_from_slice(&bytes);
        while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = buf.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&line);
            let line = line.trim();
            if fr_done {
                break;
            }
            if line.is_empty() {
                // Blank line dispatches the accumulated event below.
            } else if let Some(d) = line.strip_prefix("data:") {
                let d = d.trim();
                if d == "[DONE]" {
                    // Terminal marker without finish_reason: flush pending
                    // calls or the whole batch is dropped and the turn ends
                    // on the model's "now I'll …" text (session df1834c6).
                    flush_openai_calls(&mut arg_bufs, &tx);
                    fr_done = true;
                    break;
                }
                if !acc.is_empty() {
                    acc.push('\n');
                }
                acc.push_str(d);
                continue;
            } else {
                continue; // event:/id:/retry: lines carry no payload here
            }
            if acc.is_empty() {
                continue;
            }
            let data = std::mem::take(&mut acc);
            let v: serde_json::Value = match serde_json::from_str(&data) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if let Some(u) = v.get("usage") {
                // `stream_options.include_usage` repeats totals per frame on
                // some gateways; only the growth counts. Cached counts ride
                // inside the same object — track maxima, not sums.
                usage.input = usage.input.max(u.get("prompt_tokens").and_then(|n| n.as_u64()).unwrap_or(0) as u32);
                usage.output = usage.output.max(u.get("completion_tokens").and_then(|n| n.as_u64()).unwrap_or(0) as u32);
                // OpenAI prefix caching: cached tokens ride inside usage.
                let (cr, cw) = openai_cached_tokens(u);
                usage.cache_read = usage.cache_read.max(cr);
                usage.cache_write = usage.cache_write.max(cw);
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
                    let e = arg_bufs.entry(idx).or_default();
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
                // finish_reason can ride the same frame as the last tool-call
                // fragment; the old `continue` skipped this flush and the
                // call was lost on [DONE]/EOF.
                if v.get("choices").and_then(|c| c.get(0)).and_then(|c| c.get("finish_reason")).is_some_and(|f| !f.is_null()) {
                    flush_openai_calls(&mut arg_bufs, &tx);
                    fr_done = true;
                    break;
                }
                continue;
            }
            // finish_reason flush (openai_compat.go parity): pending tool
            // calls complete now; don't wait for EOF on a held-open stream.
            if v.get("choices").and_then(|c| c.get(0)).and_then(|c| c.get("finish_reason")).is_some_and(|f| !f.is_null()) {
                flush_openai_calls(&mut arg_bufs, &tx);
                fr_done = true;
                break;
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
        if fr_done {
            break;
        }
    }
    if !fr_done {
        flush_openai_calls(&mut arg_bufs, &tx);
    }
    Ok(usage)
}

async fn chat_anthropic(
    def: &ProviderDef,
    req: Request,
    tx: mpsc::UnboundedSender<Chunk>,
) -> Result<Usage, LlmError> {
    let url = format!("{}/v1/messages", def.base_url.trim_end_matches('/'));
    let system: String = req
        .messages
        .iter()
        .filter(|m| m.role == "system")
        .map(|m| m.content.as_str())
        .collect();
    // Vision rides EVERY user turn as content blocks (anthropic.go parity).
    let non_system: Vec<&Message> = req.messages.iter().filter(|m| m.role != "system").collect();
    let last_user_pos = non_system.iter().rposition(|m| m.role == "user");
    let messages: Vec<_> = non_system
        .iter()
        .enumerate()
        .map(|(i, m)| {
            if m.role != "user" || req.images.is_empty() {
                if Some(i) == last_user_pos {
                    // Prompt-caching breakpoint: the stable prefix
                    // (system + history) is served from cache; only the
                    // growing suffix is billed at full input rates. Mark
                    // the last user block so prefix-cache hits apply.
                    return serde_json::json!({"role": m.role, "content": [
                        {"type": "text", "text": m.content, "cache_control": {"type": "ephemeral"}}]});
                }
                return serde_json::json!({"role": m.role, "content": m.content});
            }
            let mut parts = vec![serde_json::json!({"type": "text", "text": m.content})];
            for im in &req.images {
                let mt = if im.media_type.is_empty() { "image/png" } else { im.media_type.as_str() };
                parts.push(serde_json::json!({"type": "image",
                    "source": {"type": "base64", "media_type": mt, "data": im.data}}));
            }
            if Some(i) == last_user_pos {
                parts.push(serde_json::json!({"type": "text", "text": "", "cache_control": {"type": "ephemeral"}}));
            }
            serde_json::json!({"role": "user", "content": parts})
        })
        .collect();
    // Thinking needs headroom: floor max_tokens at budget+1024 (Go parity).
    let budget = effort_budget(&req.reasoning_effort);
    let mut max_tokens = if req.max_tokens == 0 { 4096 } else { req.max_tokens };
    if budget > 0 && max_tokens < budget + 1024 {
        max_tokens = budget + 1024;
    }
    let mut body = serde_json::json!({
        "model": req.model,
        "max_tokens": max_tokens,
        "messages": messages,
        "stream": true,
    });
    if !system.is_empty() {
        // System is the most stable prefix: always a cache breakpoint.
        body["system"] = serde_json::json!([{"type": "text", "text": system,
            "cache_control": {"type": "ephemeral"}}]);
    }
    if !req.tools.is_empty() {
        let mut tools: Vec<serde_json::Value> = req.tools.iter().map(|t|
            serde_json::json!({"name": t.name, "description": t.description,
                "input_schema": if t.schema.is_null() { serde_json::json!({"type":"object"}) } else { t.schema.clone() }})
        ).collect();
        // Tools are stable across the turn: cache them too (last tool
        // carries the breakpoint — Anthropic allows up to 4).
        if let Some(last) = tools.last_mut() {
            if let Some(o) = last.as_object_mut() {
                o.insert("cache_control".into(), serde_json::json!({"type": "ephemeral"}));
            }
        }
        body["tools"] = serde_json::Value::Array(tools);
    }
    if budget > 0 {
        body["thinking"] =
            serde_json::json!({"type": "enabled", "budget_tokens": budget});
    }
    // Anthropic-native auth is x-api-key, not Bearer (anthropic.go parity).
    let mut headers = reqwest::header::HeaderMap::new();
    if let Ok(v) = def.api_key.parse() {
        headers.insert("x-api-key", v);
    }
    if let Ok(v) = "2023-06-01".parse() {
        headers.insert("anthropic-version", v);
    }
    let res = post_with_retry(
        || client().post(&url).headers(headers.clone()).json(&body),
        "anthropic",
    )
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
    let mut stopped = false;
    while let Some(item) = stream.next().await {
        let bytes = item.map_err(LlmError::Http)?;
        buf.extend_from_slice(&bytes);
        while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = buf.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&line).trim().to_string();
            if stopped {
                break;
            }
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
                        usage.output = usage.output.max(u.get("output_tokens").and_then(|n| n.as_u64()).unwrap_or(0) as u32);
                    }
                }
                "message_start" => {
                    if let Some(m) = v.get("message").and_then(|m| m.get("usage")) {
                        usage.input = usage.input.max(m.get("input_tokens").and_then(|n| n.as_u64()).unwrap_or(0) as u32);
                        // Anthropic prompt caching: creation + read counts.
                        usage.cache_write = usage.cache_write.max(
                            m.get("cache_creation_input_tokens").and_then(|n| n.as_u64()).unwrap_or(0) as u32);
                        usage.cache_read = usage.cache_read.max(
                            m.get("cache_read_input_tokens").and_then(|n| n.as_u64()).unwrap_or(0) as u32);
                    }
                }
                // Per-block tool emission (anthropic.go parity): a finished
                // block is actionable now, not at stream EOF.
                "content_block_stop" => {
                    let idx = v.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as u32;
                    if let (Some((id, name)), Some(raw)) = (names.remove(&idx), partials.remove(&idx)) {
                        if !name.is_empty() {
                            let args = parse_args(&raw);
                            let _ = tx.send(Chunk {
                                tool_calls: vec![ToolCall { id, name, args, raw_args: raw }],
                                ..Default::default()
                            });
                        }
                    }
                }
                // Terminate on message_stop: a gateway may leave the stream
                // open afterwards, and waiting for EOF would hang the turn.
                "message_stop" => {
                    let _ = tx.send(Chunk { done: true, ..Default::default() });
                    stopped = true;
                    break;
                }
                _ => {}
            }
        }
        if stopped {
            break;
        }
    }
    if stopped {
        return Ok(usage);
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
    // Vision rides the last user message as `images: [base64…]` (ollama.go).
    let last_user_idx = req.messages.iter().rposition(|m| m.role == "user");
    let messages: Vec<serde_json::Value> = req
        .messages
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let mut o = serde_json::json!({"role": m.role, "content": m.content});
            if !req.images.is_empty() && Some(i) == last_user_idx {
                o["images"] = serde_json::json!(req.images.iter().map(|im| &im.data).collect::<Vec<_>>());
            }
            o
        })
        .collect();
    let mut body = serde_json::json!({
        "model": req.model,
        "messages": messages,
        "tools": req.tools.iter().map(|t| serde_json::json!({"type": "function", "function": {"name": t.name, "description": t.description,
            "parameters": if t.schema.is_null() { serde_json::json!({"type":"object"}) } else { t.schema.clone() }}})).collect::<Vec<_>>(),
        "stream": true,
    });
    // Ollama has no effort ladder, only thinking on/off: any explicit
    // variant enables it, auto ("") leaves the model default.
    if !normalize_effort(&req.reasoning_effort).is_empty() {
        body["think"] = serde_json::json!(true);
    }
    let res = post_with_retry(|| client().post(&url).json(&body), "ollama").await?;
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


/// One live model (OpenRouter-style `context_length`; plain OpenAI has none → 0).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelMeta {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub context_window: u32,
    /// Provider-advertised reasoning ladder for this model; empty = unknown,
    /// fall back to the hardcoded ladder (Go SupportedEfforts parity).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub supported_efforts: Vec<String>,
}
fn is_zero(n: &u32) -> bool {
    *n == 0
}

/// Live model listing for pickers (`lumina models`, TUI /model, web picker).
/// Metadata failures mean "unknown" — never fail a turn over listing.
/// OpenAI-style model listing (GET /models; OpenRouter `context_length`).
/// Provider-specific schemas belong in
/// plugins via the `models.list` hook, not here.
async fn openai_models(def: &ProviderDef, extra: &[(&str, String)]) -> Result<Vec<ModelMeta>, LlmError> {
    let url = format!("{}/models", def.base_url.trim_end_matches('/'));
    let mut req = client().get(&url).headers(auth_headers(def));
    for (k, v) in extra {
        req = req.header(*k, v.clone());
    }
    let res = req.send().await?;
    if !res.status().is_success() {
        return Err(LlmError::Msg(format!("models {url}: {}", res.status())));
    }
    let v: serde_json::Value = res.json().await?;
    Ok(openai_models_from_value(&v))
}

/// Pure parse of an OpenAI-style `/models` payload (shared by the GET
/// wrapper and the Copilot branch, which needs the raw payload too for
/// its structural endpoint filter).
fn openai_models_from_value(v: &serde_json::Value) -> Vec<ModelMeta> {
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
                    .or_else(|| m.get("max_context_window"))
                    .and_then(|n| n.as_u64())
                    .unwrap_or(0) as u32,
                supported_efforts: m
                    .get("supported_efforts")
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|e| e.as_str().map(|s| s.to_string()))
                            .collect()
                    })
                    .unwrap_or_default(),
            });
        }
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

/// Advertised `supported_endpoints` for one catalog id (None when the
/// legacy entry omits the field).
fn catalog_endpoints(catalog: &serde_json::Value, id: &str) -> Option<Vec<String>> {
    catalog
        .get("data")
        .and_then(|d| d.as_array())
        .and_then(|arr| {
            arr.iter().find(|m| m.get("id").and_then(|i| i.as_str()) == Some(id))
        })
        .and_then(|m| m.get("supported_endpoints"))
        .and_then(|e| e.as_array())
        .map(|a| a.iter().filter_map(|e| e.as_str().map(|s| s.to_string())).collect())
}

/// Structural chat compatibility: a present endpoint list without
/// `/chat/completions` (Responses-only models) can never ride our chat
/// path. Absent lists (legacy entries) stay; the probe cache decides those.
fn copilot_chat_compatible(endpoints: Option<&Vec<String>>) -> bool {
    match endpoints {
        None => true,
        Some(list) => list.iter().any(|e| e == "/chat/completions"),
    }
}

/// Fail-open probe verdict: only a clear refusal hides a model.
/// Throttles, outages and auth failures keep it — never poison the picker
/// on a transient error. Returns (keep, definitive_signal).
fn classify_copilot_probe(status: u16, body: &str) -> (bool, bool) {
    if status == 200 {
        return (true, true);
    }
    if status == 404 {
        return (false, true);
    }
    if status == 400 {
        let b = body.to_lowercase();
        if b.contains("not_supported")
            || b.contains("not supported")
            || b.contains("not found")
            || b.contains("no such model")
        {
            return (false, true);
        }
        return (true, false);
    }
    (true, false)
}

/// One tiny non-streaming chat probe: does this model actually accept
/// chat completions on this account? (max_tokens 1; ~10 tokens each.)
async fn probe_copilot_model(base: &str, token: &str, id: &str) -> (bool, bool) {
    let body = serde_json::json!({
        "model": id,
        "messages": [{"role": "user", "content": "hi"}],
        "max_tokens": 1,
        "stream": false,
    });
    let mut req = client()
        .post(format!("{}/chat/completions", base.trim_end_matches('/')))
        .bearer_auth(token)
        .timeout(std::time::Duration::from_secs(20))
        .json(&body);
    for (k, v) in copilot_headers() {
        req = req.header(k, v.clone());
    }
    let res = match req.send().await {
        Ok(r) => r,
        Err(_) => return (true, false),
    };
    let status = res.status().as_u16();
    let text = res.text().await.unwrap_or_default();
    classify_copilot_probe(status, &text)
}

/// Usability probe outcome over a catalog: the verified-usable ids, plus
/// whether the run produced any definitive signal at all.
struct CopilotProbeOut {
    ids: HashSet<String>,
    signalled: bool,
}

/// Probe every id (6 at a time) for real chat usability.
async fn probe_copilot_usable(base: &str, token: &str, ids: &[String]) -> CopilotProbeOut {
    let mut out = CopilotProbeOut { ids: HashSet::new(), signalled: false };
    let mut js = tokio::task::JoinSet::new();
    for chunk in ids.chunks(6) {
        for id in chunk {
            let (b, t, id) = (base.to_string(), token.to_string(), id.clone());
            js.spawn(async move {
                let (keep, signal) = probe_copilot_model(&b, &t, &id).await;
                (id, keep, signal)
            });
        }
        while let Some(r) = js.join_next().await {
            if let Ok((id, keep, signal)) = r {
                if keep {
                    out.ids.insert(id);
                }
                out.signalled |= signal;
            }
        }
    }
    out
}

async fn ollama_models(def: &ProviderDef) -> Result<Vec<ModelMeta>, LlmError> {
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
                out.push(ModelMeta { id: name.clone(), name, context_window: 0, supported_efforts: Vec::new() });
            }
        }
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(out)
}

/// Ollama context window via /api/show (model_info `*.context_length`).
async fn ollama_window(def: &ProviderDef, model: &str) -> u32 {
    let url = format!("{}/api/show", def.base_url.trim_end_matches('/'));
    let res = match client().post(&url).json(&serde_json::json!({"name": model})).send().await {
        Ok(r) => r,
        Err(_) => return 0,
    };
    let v: serde_json::Value = match res.json().await {
        Ok(v) => v,
        Err(_) => return 0,
    };
    // model_info keys look like "llama.context_length"; details may repeat it.
    let mut best = v
        .get("details")
        .and_then(|d| d.get("context_length"))
        .and_then(|n| n.as_u64())
        .unwrap_or(0);
    if let Some(mi) = v.get("model_info").and_then(|m| m.as_object()) {
        for (k, n) in mi {
            if k.ends_with("context_length") {
                if let Some(n) = n.as_u64() {
                    best = best.max(n);
                }
            }
        }
    }
    best.min(u32::MAX as u64) as u32
}

/// OpenAI Responses API (`POST /responses`, SSE): output_text deltas stream
/// immediately; function-call args accumulate per output_index until the
/// terminal completed/incomplete/failed event.
async fn chat_responses(
    def: &ProviderDef,
    req: Request,
    tx: mpsc::UnboundedSender<Chunk>,
) -> Result<Usage, LlmError> {
    let url = format!("{}/responses", def.base_url.trim_end_matches('/'));
    let mut instructions = String::new();
    let mut input = Vec::new();
    // Vision rides the LAST user message only (Go parity).
    let last_user_idx = req.messages.iter().rposition(|m| m.role == "user" && m.role != "system" && m.role != "tool");
    for (i, m) in req.messages.iter().enumerate() {
        if m.role == "system" {
            if !instructions.is_empty() {
                instructions.push('\n');
            }
            instructions.push_str(&m.content);
            continue;
        }
        if m.role == "tool" {
            continue; // Responses replays via encrypted reasoning, not tool role
        }
        // Vision shape on the last user message when images are attached.
        if !req.images.is_empty() && Some(i) == last_user_idx {
            let mut parts = vec![serde_json::json!({"type": "input_text", "text": m.content})];
            for im in &req.images {
                let mt = if im.media_type.is_empty() { "image/png" } else { &im.media_type };
                parts.push(serde_json::json!({"type": "input_image",
                    "image_url": format!("data:{mt};base64,{}", im.data)}));
            }
            input.push(serde_json::json!({"role": "user", "content": parts}));
        } else {
            input.push(serde_json::json!({"role": m.role, "content": m.content}));
        }
    }
    let mut body = serde_json::json!({"model": req.model, "stream": true, "input": input});
    if std::env::var("LUMINA_DEBUG_SSE").is_ok() {
        if let Ok(b) = serde_json::to_string(&body) {
            eprintln!("[responses-req] model={} msgs={} tools={} bytes={} head={:.300}", req.model, req.messages.len(), req.tools.len(), b.len(), b);
        }
    }
    if !instructions.is_empty() {
        body["instructions"] = serde_json::json!(instructions);
    }
    if !normalize_effort(&req.reasoning_effort).is_empty() {
        body["reasoning"] = serde_json::json!({"effort": normalize_effort(&req.reasoning_effort)});
    }
    if !req.tools.is_empty() {
        body["tools"] = serde_json::json!(req.tools.iter().map(|t|
            serde_json::json!({"type": "function", "name": t.name, "description": t.description,
                "parameters": if t.schema.is_null() { serde_json::json!({"type":"object"}) } else { t.schema.clone() }})
        ).collect::<Vec<_>>());
    }
    // One routing session for both gateway headers (Go parity: single sid).
    let sid = uuid::Uuid::new_v4().to_string();
    // Effort step-down: a stale pick or unadvertised ladder 400s on
    // unsupported rungs (e.g. iceman rejects max) — walk down to the
    // provider default instead of failing the turn.
    let mut effort = normalize_effort(&req.reasoning_effort).to_string();
    let mut retries = 0u32;
    let res = loop {
        if effort.is_empty() {
            if let Some(o) = body.as_object_mut() {
                o.remove("reasoning");
            }
        } else {
            body["reasoning"] = serde_json::json!({"effort": effort});
        }
        let res = match client()
            .post(&url)
            .headers(auth_headers(def))
            .header("x-lumina-session", &sid)
            .header("x-opencode-session", &sid)
            .json(&body)
            .send()
            .await
        {
            Ok(r) if !is_retryable_status(r.status().as_u16()) => r,
            Ok(r) if retries >= 5 => r,
            Ok(_) => {
                tracing::warn!("responses: retryable provider failure, attempt {}/5", retries + 1);
                tokio::time::sleep(retry_delay(retries)).await;
                retries += 1;
                continue;
            }
            Err(e) if is_retryable_transport(&e) && retries < 5 => {
                tracing::warn!("responses: retryable transport failure, attempt {}/5", retries + 1);
                tokio::time::sleep(retry_delay(retries)).await;
                retries += 1;
                continue;
            }
            Err(e) => return Err(LlmError::Http(e)),
        };
        if res.status().is_success() {
            break res;
        }
        let st = res.status();
        let t = res.text().await.unwrap_or_default();
        if st.as_u16() == 400 && !effort.is_empty() && is_effort_error(&t) {
            if let Some(down) = effort_step_down(&effort) {
                effort = down.to_string();
                retries = 0;
                continue;
            }
        }
        return Err(LlmError::Msg(format!("responses {url}: {st}: {t}")));
    };
    let mut pending: HashMap<u32, (String, String, String)> = HashMap::new(); // idx -> (id, name, args)
    let mut saw_text = false;
    let mut stream = res.bytes_stream();
    let mut buf = Vec::<u8>::new();
    let debug_sse = std::env::var("LUMINA_DEBUG_SSE").is_ok();
    while let Some(item) = stream.next().await {
        let bytes = item.map_err(LlmError::Http)?;
        buf.extend_from_slice(&bytes);
        while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = buf.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&line).trim().to_string();
            let Some(data) = line.strip_prefix("data:") else { continue };
            let data = data.trim();
            if data == "[DONE]" {
                continue;
            }
            if debug_sse {
                eprintln!("[sse] {data:.300}");
            }
            let Ok(v) = serde_json::from_str::<serde_json::Value>(data) else { continue };
            let typ = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
            let idx = v.get("output_index").and_then(|i| i.as_u64()).unwrap_or(0) as u32;
            match typ {
                "response.output_text.delta" => {
                    let t = v.get("delta").and_then(|d| d.as_str()).unwrap_or("");
                    if !t.is_empty() {
                        saw_text = true;
                        let _ = tx.send(Chunk { delta: t.into(), ..Default::default() });
                    }
                }
                // Full-text fallback: some gateways batch text into .done with
                // no preceding deltas. Only use it when nothing streamed (the
                // .done text otherwise duplicates the deltas).
                "response.output_text.done" => {
                    let t = v.get("text").and_then(|x| x.as_str()).unwrap_or("");
                    if !t.is_empty() && !saw_text {
                        saw_text = true;
                        let _ = tx.send(Chunk { delta: t.into(), ..Default::default() });
                    }
                }
                "response.output_item.added" => {
                    if let Some(item) = v.get("item") {
                        if item.get("type").and_then(|t| t.as_str()) == Some("function_call") {
                            let id = item
                                .get("call_id")
                                .or_else(|| item.get("id"))
                                .and_then(|i| i.as_str())
                                .unwrap_or("")
                                .to_string();
                            let name = item.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string();
                            pending.insert(idx, (id, name, String::new()));
                        }
                    }
                }
                "response.function_call_arguments.delta" => {
                    let d = v.get("delta").and_then(|x| x.as_str()).unwrap_or("");
                    pending.entry(idx).or_default().2.push_str(d);
                }
                // Full-args fallback for the same batching case.
                "response.function_call_arguments.done" => {
                    let full = v.get("arguments").and_then(|x| x.as_str()).unwrap_or("");
                    let e = pending.entry(idx).or_default();
                    if e.2.is_empty() {
                        e.2 = full.to_string();
                    }
                }
                // Item-done carries call identity (backup when .added was missed).
                "response.output_item.done" => {
                    if let Some(item) = v.get("item") {
                        if item.get("type").and_then(|t| t.as_str()) == Some("function_call") {
                            let e = pending.entry(idx).or_default();
                            if e.0.is_empty() {
                                e.0 = item
                                    .get("call_id")
                                    .or_else(|| item.get("id"))
                                    .and_then(|i| i.as_str())
                                    .unwrap_or("")
                                    .to_string();
                            }
                            if e.1.is_empty() {
                                e.1 = item.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string();
                            }
                        }
                    }
                }
                "response.completed" | "response.incomplete" | "response.failed" => {
                    // Responses API terminal usage (input/output + cached).
                    if let Some(u) = v.get("response").and_then(|r| r.get("usage")) {
                        let (cr, cw) = openai_cached_tokens(u);
                        let cached_detail = u.get("input_tokens_details")
                            .and_then(|d| d.get("cached_tokens"))
                            .and_then(|n| n.as_u64()).unwrap_or(0) as u32;
                        let ru = Usage {
                            input: u.get("input_tokens").and_then(|n| n.as_u64()).unwrap_or(0) as u32,
                            output: u.get("output_tokens").and_then(|n| n.as_u64()).unwrap_or(0) as u32,
                            cache_read: cr.max(cached_detail),
                            cache_write: cw,
                        };
                        flush_openai_calls(&mut pending, &tx);
                        return Ok(ru);
                    }
                    flush_openai_calls(&mut pending, &tx);
                    return Ok(Usage::default());
                }
                _ => {}
            }
        }
    }
    // Gateway closed (or sent [DONE]) without a terminal event: still flush
    // calls it already streamed. Dropping them made the harness end the turn
    // on the model's "now I'll …" text with no tool call (session df1834c6).
    flush_openai_calls(&mut pending, &tx);
    Ok(Usage::default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_budget_covers_transient_failures() {
        assert!(is_retryable_status(429));
        assert!(is_retryable_status(503));
        assert!(is_retryable_status(529));
        assert!(!is_retryable_status(400));
        assert!(!is_retryable_status(401));
        assert!(!is_retryable_status(404));
        assert_eq!(retry_delay(0), std::time::Duration::from_millis(2000));
        assert_eq!(retry_delay(1), std::time::Duration::from_millis(4000));
        assert_eq!(retry_delay(9), std::time::Duration::from_millis(10_000));
    }

    #[test]
    fn split_model_shapes() {
        assert_eq!(split_model("openrouter/deepseek-r1"), ("openrouter".into(), "deepseek-r1".into()));
        assert_eq!(split_model("qwen"), ("".into(), "qwen".into()));
    }

    #[test]
    fn usage_add_saturates_cache_counters() {
        let mut a = Usage { input: 10, output: 5, cache_read: u32::MAX, cache_write: 1 };
        a.add(&Usage { input: 1, output: 2, cache_read: 7, cache_write: 3 });
        assert_eq!((a.input, a.output), (11, 7));
        assert_eq!(a.cache_read, u32::MAX, "saturating, never wraps");
        assert_eq!(a.cache_write, 4);
    }

    #[test]
    fn cache_hit_pct_clamps_and_handles_empty() {
        assert_eq!(Usage::default().cache_hit_pct(), 0);
        assert_eq!(Usage { input: 1000, cache_read: 710, ..Default::default() }.cache_hit_pct(), 71);
        // Over-reporting provider data never exceeds 100.
        assert_eq!(Usage { input: 100, cache_read: 500, ..Default::default() }.cache_hit_pct(), 100);
    }

    #[test]
    fn openai_cached_tokens_accepts_gateway_variants() {
        let v = serde_json::json!({"prompt_tokens": 100,
            "prompt_tokens_details": {"cached_tokens": 60}});
        assert_eq!(openai_cached_tokens(&v), (60, 0));
        let v = serde_json::json!({"prompt_cache_hit_tokens": 25});
        assert_eq!(openai_cached_tokens(&v), (25, 0));
        let v = serde_json::json!({"prompt_tokens": 100});
        assert_eq!(openai_cached_tokens(&v), (0, 0));
    }

    #[test]
    fn effort_helpers() {
        assert_eq!(effort_index("auto"), -1);
        assert_eq!(effort_index(""), -1);
        assert_eq!(effort_index("none"), 0);
        assert_eq!(effort_index("minimal"), 1);
        assert_eq!(effort_index("low"), 2);
        assert_eq!(effort_index("xhigh"), 5);
        assert_eq!(effort_index("max"), 6);
        assert_eq!(effort_index("bogus"), -1);
        assert_eq!(efforts_or_default(&[]), vec!["auto", "none", "minimal", "low", "medium", "high", "xhigh", "max"]);
        assert_eq!(
            efforts_or_default(&["high".into(), "low".into()]),
            vec!["auto", "high", "low"]
        );
        assert_eq!(clamp_effort("max", &["low".into(), "high".into()]), "high");
        assert_eq!(clamp_effort("bogus", &["low".into()]), "low");
        assert_eq!(clamp_effort("low", &["low".into()]), "low");
        // Advertised provider vocabularies pass through verbatim.
        assert_eq!(
            efforts_or_default(&["minimal".into(), "low".into(), "medium".into(), "high".into(), "xhigh".into()]),
            vec!["auto", "minimal", "low", "medium", "high", "xhigh"]
        );
        assert_eq!(clamp_effort("max", &["minimal".into(), "low".into(), "medium".into(), "high".into(), "xhigh".into()]), "xhigh");
    }

    #[test]
    fn effort_step_down_walks_to_default() {
        assert_eq!(effort_step_down("max"), Some("xhigh"));
        assert_eq!(effort_step_down("xhigh"), Some("high"));
        assert_eq!(effort_step_down("high"), Some("medium"));
        assert_eq!(effort_step_down("medium"), Some("low"));
        assert_eq!(effort_step_down("low"), Some("minimal"));
        assert_eq!(effort_step_down("minimal"), Some("none"));
        assert_eq!(effort_step_down("none"), Some(""));
        assert_eq!(effort_step_down(""), None);
        assert_eq!(effort_step_down("bogus"), None);
    }

    #[test]
    fn effort_error_detects_rejection() {
        assert!(is_effort_error("Upstream request failed: [invalid_request_error] Model 'iceman-contributor-free' does not support reasoning_effort='max'."));
        assert!(is_effort_error("reasoning effort unsupported"));
        assert!(!is_effort_error("Upstream request failed: Model is unavailable."));
        assert!(!is_effort_error(""));
    }

    #[test]
    fn effort_ladder() {
        assert_eq!(normalize_effort(" XHigh "), "xhigh");
        assert_eq!(normalize_effort("Minimal"), "minimal");
        assert_eq!(normalize_effort("NONE"), "none");
        assert_eq!(normalize_effort("bogus"), "");
        assert!(valid_effort("auto") && valid_effort("minimal") && valid_effort("none") && !valid_effort("ultra"));
        assert_eq!(effort_label("minimal"), "minimal");
        assert_eq!(effort_label("none"), "none");
        assert_eq!(effort_budget("low"), 1024);
        assert_eq!(effort_budget("minimal"), 1024);
        assert_eq!(effort_budget("none"), 0);
        assert_eq!(effort_budget(""), 0);
    }

    #[test]
    fn effort_label_renders_auto() {
        assert_eq!(effort_label(""), "auto");
        assert_eq!(effort_label("auto"), "auto");
        assert_eq!(effort_label(" XHIGH "), "xhigh");
        assert_eq!(effort_label("MAX"), "max");
        assert_eq!(effort_label("bogus"), "auto");
    }

    #[test]
    fn empty_protocol_maps_to_openai_compatible() {
        let d = ProviderDef {
            protocol: String::new(),
            base_url: "http://x".into(),
            api_key: String::new(),
            github_token: String::new(),
            token_url: String::new(),
        };
        assert_eq!(d.kind(), "openai-compatible");
        let d2 = ProviderDef { protocol: "responses".into(), ..d.clone() };
        assert_eq!(d2.kind(), "openai-responses");
    }

    #[test]
    fn openai_fragment_assembly() {
        // Simulate two SSE tool_call fragments reassembled by index.
        let mut bufs: HashMap<u32, (String, String, String)> = HashMap::new();
        for (idx, args) in [(0u32, "{\"path\": \"a"), (0u32, ".rs\"}")] {
            bufs.entry(idx).or_default().2.push_str(args);
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
                github_token: String::new(),
                token_url: String::new(),
            })]
            .into_iter()
            .collect(),
            overrides: std::sync::RwLock::new(HashMap::new()),
            copilot_sessions: std::sync::RwLock::new(HashMap::new()),
            copilot_usable: std::sync::Arc::new(std::sync::RwLock::new(HashMap::new())),
            copilot_refreshing: std::sync::Arc::new(std::sync::RwLock::new(HashSet::new())),
            ctx_cache: std::sync::RwLock::new(HashMap::new()),
        };
        assert_eq!(r.model_for("executor", "plan"), "o/m");
        assert_eq!(r.model_for("fast", "build"), "o/m");
    }

    /// Core parses only generic OpenAI/OpenRouter metadata (`context_length`,
    /// `supported_efforts`). Provider-specific schemas stay unknown here —
    /// a plugin enriches them via the `models.list` hook.
    #[tokio::test]
    async fn openai_models_parses_generic_window_and_efforts() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            let Ok((mut s, _)) = listener.accept() else { return };
            let mut head = Vec::new();
            while !head.ends_with(b"\r\n\r\n") && head.len() < 65536 {
                let mut b = [0u8; 1];
                if s.read_exact(&mut b).is_err() {
                    return;
                }
                head.push(b[0]);
            }
            let body = r#"{"data":[
                {"id":"muse-spark-1.3-contributor","name":"Muse Spark 1.3 Contributor","context_length":1048576,"supported_efforts":["low","high"],"limit":{"context":2097152},"capabilities":{"reasoning":{"effort":["minimal","low","medium","high","xhigh"]}}},
                {"id":"beam-free","name":"Beam Free"}
            ]}"#;
            let _ = s.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            );
        });
        let def = ProviderDef {
            protocol: "openai-responses".into(),
            base_url: format!("http://{addr}"),
            api_key: String::new(),
            github_token: String::new(),
            token_url: String::new(),
        };
        let models = openai_models(&def, &[]).await.expect("models");
        assert_eq!(models.len(), 2, "{models:?}");
        // Sorted by id: beam-free first, generic fields only.
        assert_eq!(models[0].id, "beam-free");
        assert_eq!(models[0].context_window, 0);
        assert!(models[0].supported_efforts.is_empty());
        assert_eq!(models[1].id, "muse-spark-1.3-contributor");
        assert_eq!(models[1].context_window, 1048576, "{models:?}");
        assert_eq!(models[1].supported_efforts, vec!["low", "high"]);
    }

    #[test]
    fn copilot_structural_filter_drops_responses_only() {
        // Legacy entries omit supported_endpoints (probe decides);
        // a present list without /chat/completions is structural.
        assert!(copilot_chat_compatible(None));
        assert!(copilot_chat_compatible(Some(&vec!["/chat/completions".into()])));
        assert!(copilot_chat_compatible(Some(&vec!["/chat/completions".into(), "/responses".into()])));
        assert!(!copilot_chat_compatible(Some(&vec!["/responses".into(), "ws:/responses".into()])));
        assert!(!copilot_chat_compatible(Some(&vec![])));
    }

    #[test]
    fn copilot_probe_verdict_fails_open() {
        // Ground truth only: 200 keeps, clear refusals drop.
        assert_eq!(classify_copilot_probe(200, ""), (true, true));
        assert_eq!(classify_copilot_probe(404, ""), (false, true));
        assert_eq!(
            classify_copilot_probe(400, r#"{"error":{"code":"model_not_supported"}}"#),
            (false, true)
        );
        assert_eq!(
            classify_copilot_probe(400, "The requested model is not supported"),
            (false, true)
        );
        // Anything else keeps without a signal (throttle/outage/auth:
        // never poison the picker on a transient error).
        assert_eq!(classify_copilot_probe(400, r#"{"error":"bad request"}"#), (true, false));
        assert_eq!(classify_copilot_probe(429, "slow down"), (true, false));
        assert_eq!(classify_copilot_probe(500, ""), (true, false));
        assert_eq!(classify_copilot_probe(401, ""), (true, false));
    }

    /// Deadline-bounded mock Copilot gateway: serves GET /models with the
    /// given catalog and answers POST /chat/completions 200 only for the
    /// model id containing "good" (400 model_not_supported otherwise).
    fn serve_copilot_mock(
        listener: std::net::TcpListener,
        catalog: &'static str,
        max_conns: usize,
    ) {
        use std::io::{Read, Write};
        listener.set_nonblocking(true).unwrap();
        let end = std::time::Instant::now() + std::time::Duration::from_secs(8);
        let mut served = 0;
        while served < max_conns && std::time::Instant::now() < end {
            let (s, _) = match listener.accept() {
                Ok(x) => x,
                Err(_) => {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                    continue;
                }
            };
            let mut s = s;
            s.set_nonblocking(false).ok();
            s.set_read_timeout(Some(std::time::Duration::from_secs(5))).ok();
            let mut head = Vec::new();
            loop {
                let mut b = [0u8; 1];
                if s.read_exact(&mut b).is_err() {
                    break;
                }
                head.push(b[0]);
                if head.ends_with(b"\r\n\r\n") || head.len() > 65536 {
                    break;
                }
            }
            let head_s = String::from_utf8_lossy(&head).into_owned();
            let mut body = Vec::new();
            if head_s.starts_with("POST") {
                let len = head_s
                    .lines()
                    .find_map(|l| {
                        l.trim().to_lowercase().strip_prefix("content-length:").and_then(|n| {
                            n.trim().parse::<usize>().ok()
                        })
                    })
                    .unwrap_or(0);
                let mut buf = vec![0u8; len.min(65536)];
                let mut read = 0;
                while read < buf.len() {
                    match s.read(&mut buf[read..]) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => read += n,
                    }
                }
                body = buf[..read].to_vec();
            }
            let (status, resp_body) = if head_s.starts_with("GET /models") {
                ("200 OK", catalog.to_string())
            } else if head_s.starts_with("POST") {
                if String::from_utf8_lossy(&body).contains("\"good\"") {
                    ("200 OK", r#"{"id":"good","choices":[],"usage":{}}"#.to_string())
                } else {
                    (
                        "400 Bad Request",
                        r#"{"error":{"code":"model_not_supported","message":"The requested model is not supported"}}"#
                            .to_string(),
                    )
                }
            } else {
                ("404 Not Found", "{}".to_string())
            };
            let _ = s.write_all(
                format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{resp_body}",
                    resp_body.len()
                )
                .as_bytes(),
            );
            served += 1;
        }
    }

    const MOCK_CATALOG: &str = r#"{"data":[
        {"id":"dead-chat","supported_endpoints":["/chat/completions"]},
        {"id":"good"},
        {"id":"legacy-dead"},
        {"id":"responses-only","supported_endpoints":["/responses","ws:/responses"]}
    ]}"#;

    fn copilot_test_router(addr: &str) -> Router {
        Router {
            fast: String::new(),
            executor: "cop/good".into(),
            planner: String::new(),
            improver: String::new(),
            providers: [("cop".into(), ProviderDef {
                protocol: "copilot".into(),
                base_url: format!("http://{addr}"),
                api_key: String::new(),
                github_token: "g".into(),
                token_url: String::new(),
            })]
            .into_iter()
            .collect(),
            overrides: std::sync::RwLock::new(HashMap::new()),
            copilot_sessions: std::sync::RwLock::new(
                [("cop".into(), CopilotSession {
                    token: "sess".into(),
                    expires_unix: chrono::Utc::now().timestamp() + 99999,
                    base_url: format!("http://{addr}"),
                })]
                .into_iter()
                .collect(),
            ),
            copilot_usable: std::sync::Arc::new(std::sync::RwLock::new(HashMap::new())),
            copilot_refreshing: std::sync::Arc::new(std::sync::RwLock::new(HashSet::new())),
            ctx_cache: std::sync::RwLock::new(HashMap::new()),
        }
    }

    /// First listing (no cache): Responses-only is hidden structurally;
    /// the rest serve meanwhile while the background probe refreshes.
    #[tokio::test]
    async fn copilot_list_models_structural_first() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        std::thread::spawn(move || serve_copilot_mock(listener, MOCK_CATALOG, 12));
        let r = copilot_test_router(&addr);
        let models = r.list_models("cop").await.expect("models");
        let ids: Vec<_> = models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["dead-chat", "good", "legacy-dead"], "{ids:?}");
    }

    /// The probe itself: only the actually-usable id survives.
    #[tokio::test]
    async fn copilot_probe_finds_usable() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        std::thread::spawn(move || serve_copilot_mock(listener, MOCK_CATALOG, 8));
        let out = probe_copilot_usable(
            &format!("http://{addr}"),
            "sess",
            &["good".into(), "dead-chat".into(), "legacy-dead".into()],
        )
        .await;
        assert!(out.signalled);
        assert_eq!(out.ids, HashSet::from(["good".to_string()]));
    }

    /// Fresh cache: the listing hides entitlement-dead entries outright.
    #[tokio::test]
    async fn copilot_list_models_cached_hides_dead() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        // Fresh cache: no background refresh fires, exactly one connection.
        std::thread::spawn(move || serve_copilot_mock(listener, MOCK_CATALOG, 2));
        let r = copilot_test_router(&addr);
        r.copilot_usable.write().unwrap().insert(
            "cop".into(),
            (HashSet::from(["good".to_string()]), std::time::Instant::now()),
        );
        let models = r.list_models("cop").await.expect("models");
        let ids: Vec<_> = models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["good"], "{ids:?}");
    }
}

#[cfg(test)]
mod responses_tests {
    use super::*;
    use std::io::{Read, Write};

    /// Fake SSE server: emits a .done-only text event (no deltas) to prove
    /// the fallback path delivers text that would otherwise be dropped.
    #[tokio::test]
    async fn done_only_text_is_delivered() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let Ok((mut s, _)) = listener.accept() else { return };
            // Read just the headers; the client closes after the stream ends.
            let mut head = Vec::new();
            while !head.ends_with(b"\r\n\r\n") && head.len() < 8192 {
                let mut b = [0u8; 1];
                if s.read(&mut b).is_err() || b[0] == 0 && head.is_empty() {
                    break;
                }
                head.push(b[0]);
                if head.len() > 8192 {
                    break;
                }
            }
            let _ = head;
            let body = concat!(
                "event: response.output_text.done\n",
                "data: {\"type\":\"response.output_text.done\",\"output_index\":0,\"text\":\"DONE FALLBACK\"}\n\n",
                "event: response.completed\n",
                "data: {\"type\":\"response.completed\"}\n\n",
            );
            let _ = s.write_all(format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            ).as_bytes());
        });
        let def = ProviderDef {
            protocol: "openai-responses".into(),
            base_url: format!("http://{addr}"),
            api_key: String::new(),
            github_token: String::new(),
            token_url: String::new(),
        };
        let (tx, mut rx) = mpsc::unbounded_channel();
        let req = Request {
            model: "m".into(),
            messages: vec![llm_message("user", "hi")],
            tools: vec![],
            max_tokens: 0,
            reasoning_effort: String::new(),
            images: vec![],
        };
        let usage = chat_responses(&def, req, tx).await.expect("stream");
        let _ = usage;
        let mut text = String::new();
        let mut done = false;
        while let Some(c) = rx.recv().await {
            text.push_str(&c.delta);
            if c.done {
                done = true;
                break;
            }
        }
        assert!(done);
        assert_eq!(text, "DONE FALLBACK", "fallback .done text must be delivered");
    }

    /// Effort step-down: a 400 blaming reasoning_effort retries one rung
    /// lower instead of failing the turn (iceman rejects max).
    #[tokio::test]
    async fn effort_rejection_steps_down_and_recovers() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            // Two attempts: max 400s, xhigh succeeds.
            for _ in 0..2 {
                let Ok((mut s, _)) = listener.accept() else { return };
                let mut head = Vec::new();
                while !head.ends_with(b"\r\n\r\n") && head.len() < 65536 {
                    let mut b = [0u8; 1];
                    if s.read_exact(&mut b).is_err() {
                        return;
                    }
                    head.push(b[0]);
                }
                // Read the JSON body (Content-Length) to see the effort.
                let len = String::from_utf8_lossy(&head)
                    .lines()
                    .find_map(|l| {
                        l.strip_prefix("Content-Length:")
                            .or_else(|| l.strip_prefix("content-length:"))
                    })
                    .and_then(|v| v.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                let mut body = vec![0u8; len];
                if len > 0 && s.read_exact(&mut body).is_err() {
                    return;
                }
                let asked_max = String::from_utf8_lossy(&body).contains("\"effort\":\"max\"");
                let (status, payload) = if asked_max {
                    ("400 Bad Request", "{\"error\":{\"type\":\"server_error\",\"message\":\"Upstream request failed: [invalid_request_error] Model 'm' does not support reasoning_effort='max'.\"}}".to_string())
                } else {
                    ("200 OK", "data: {\"type\":\"response.completed\"}\n\n".to_string())
                };
                let _ = s.write_all(
                    format!(
                        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                        payload.len()
                    )
                    .as_bytes(),
                );
            }
        });
        let def = ProviderDef {
            protocol: "openai-responses".into(),
            base_url: format!("http://{addr}"),
            api_key: String::new(),
            github_token: String::new(),
            token_url: String::new(),
        };
        let (tx, mut rx) = mpsc::unbounded_channel();
        let req = Request {
            model: "m".into(),
            messages: vec![llm_message("user", "hi")],
            tools: vec![],
            max_tokens: 0,
            reasoning_effort: "max".into(),
            images: vec![],
        };
        chat_responses(&def, req, tx).await.expect("retry must recover");
        let mut done = false;
        while let Some(c) = rx.recv().await {
            if c.done {
                done = true;
                break;
            }
        }
        assert!(done, "stream must terminate after the stepped-down retry");
    }

    fn llm_message(role: &str, content: &str) -> Message {
        Message { role: role.into(), content: content.into(), thinking: String::new() }
    }
}

#[cfg(test)]
mod mixed_tests {
    use super::*;
    use std::io::{Read, Write};

    /// Fake SSE server replaying a fixed script, fed line-by-line.
    fn serve_script(body: &'static str) -> (String, std::thread::JoinHandle<()>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let h = std::thread::spawn(move || {
            let Ok((mut s, _)) = listener.accept() else { return };
            let mut head = Vec::new();
            while !head.ends_with(b"\r\n\r\n") && head.len() < 65536 {
                let mut b = [0u8; 1];
                if s.read(&mut b).is_err() {
                    return;
                }
                head.push(b[0]);
            }
            let _ = s.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            );
        });
        (format!("http://{addr}"), h)
    }

    #[tokio::test]
    async fn delta_text_plus_tool_call_survive() {
        let body = concat!(
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"working\"}\n\n",
            "event: response.output_item.added\n",
            "data: {\"type\":\"response.output_item.added\",\"output_index\":1,\"item\":{\"type\":\"function_call\",\"id\":\"x\",\"call_id\":\"c1\",\"name\":\"read\"}}\n\n",
            "event: response.function_call_arguments.delta\n",
            "data: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":1,\"delta\":\"{\\\"path\\\":\\\"a\"}\"}\n\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\"}\n\n",
        );
        let (base, h) = serve_script(body);
        let def = ProviderDef {
            protocol: "openai-responses".into(),
            base_url: base,
            api_key: String::new(),
            github_token: String::new(),
            token_url: String::new(),
        };
        let (tx, mut rx) = mpsc::unbounded_channel();
        let req = Request {
            model: "m".into(),
            messages: vec![Message { role: "user".into(), content: "hi".into(), thinking: String::new() }],
            tools: vec![],
            max_tokens: 0,
            reasoning_effort: String::new(),
            images: vec![],
        };
        chat_responses(&def, req, tx).await.expect("stream");
        let mut text = String::new();
        let mut calls = 0;
        let mut done = false;
        while let Some(c) = rx.recv().await {
            text.push_str(&c.delta);
            calls += c.tool_calls.len();
            if c.done {
                done = true;
                break;
            }
        }
        assert!(done);
        assert_eq!(text, "working", "delta text must survive alongside tool calls");
        assert_eq!(calls, 1, "tool call must survive alongside delta text");
        h.join().unwrap();
    }

    /// A gateway that closes without `response.completed` must still deliver
    /// the function calls it streamed. Dropping them ends the turn on the
    /// model's "now I'll …" text with no tool call (session df1834c6).
    #[tokio::test]
    async fn responses_eof_flushes_pending_calls() {
        let body = concat!(
            "event: response.output_item.added\n",
            "data: {\"type\":\"response.output_item.added\",\"output_index\":1,\"item\":{\"type\":\"function_call\",\"id\":\"x\",\"call_id\":\"c1\",\"name\":\"read\"}}\n\n",
            "event: response.function_call_arguments.delta\n",
            "data: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":1,\"delta\":\"{\\\"path\\\":\\\"a.txt\\\"}\"}\n\n",
        );
        let (base, h) = serve_script(body);
        let def = ProviderDef {
            protocol: "openai-responses".into(),
            base_url: base,
            api_key: String::new(),
            github_token: String::new(),
            token_url: String::new(),
        };
        let (tx, mut rx) = mpsc::unbounded_channel();
        let req = Request {
            model: "m".into(),
            messages: vec![Message { role: "user".into(), content: "hi".into(), thinking: String::new() }],
            tools: vec![],
            max_tokens: 0,
            reasoning_effort: String::new(),
            images: vec![],
        };
        chat_responses(&def, req, tx).await.expect("stream");
        let mut calls = Vec::new();
        let mut done = false;
        while let Some(c) = rx.recv().await {
            calls.extend(c.tool_calls);
            if c.done {
                done = true;
                break;
            }
        }
        assert!(done, "stream must terminate at EOF");
        assert_eq!(calls.len(), 1, "pending call must survive a stream cut");
        assert_eq!(calls[0].name, "read");
        assert_eq!(calls[0].args.get("path").and_then(|v| v.as_str()), Some("a.txt"));
        h.join().unwrap();
    }

    /// OpenAI-compat `[DONE]` without a finish_reason frame: the last delta's
    /// tool call must be flushed instead of dropped (same silent-stop class).
    #[tokio::test]
    async fn openai_done_flushes_pending_calls() {
        let body = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"Switching to native file tools\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"1\",\"function\":{\"name\":\"read\",\"arguments\":\"{\\\"path\\\":\\\"a.txt\\\"}\"}}]}}]}\n\n",
            "data: [DONE]\n\n",
        );
        let (base, h) = serve_script(body);
        let def = ProviderDef {
            protocol: "openai-compatible".into(),
            base_url: base,
            api_key: String::new(),
            github_token: String::new(),
            token_url: String::new(),
        };
        let (tx, mut rx) = mpsc::unbounded_channel();
        let req = Request {
            model: "m".into(),
            messages: vec![Message { role: "user".into(), content: "hi".into(), thinking: String::new() }],
            tools: vec![],
            max_tokens: 0,
            reasoning_effort: String::new(),
            images: vec![],
        };
        chat_openai(&def, req, tx, &[]).await.expect("stream");
        let mut calls = Vec::new();
        let mut done = false;
        while let Some(c) = rx.recv().await {
            calls.extend(c.tool_calls);
            if c.done {
                done = true;
                break;
            }
        }
        assert!(done);
        assert_eq!(calls.len(), 1, "call streamed before [DONE] must survive");
        assert_eq!(calls[0].name, "read");
        h.join().unwrap();
    }

    /// finish_reason riding the same frame as the last tool-call fragment:
    /// the pre-fix `continue` skipped the flush and the call vanished.
    #[tokio::test]
    async fn openai_same_frame_finish_flushes_pending_calls() {
        let body = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"1\",\"function\":{\"name\":\"read\",\"arguments\":\"{\\\"path\\\":\\\"a.txt\\\"}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n";
        let (base, h) = serve_script(body);
        let def = ProviderDef {
            protocol: "openai-compatible".into(),
            base_url: base,
            api_key: String::new(),
            github_token: String::new(),
            token_url: String::new(),
        };
        let (tx, mut rx) = mpsc::unbounded_channel();
        let req = Request {
            model: "m".into(),
            messages: vec![Message { role: "user".into(), content: "hi".into(), thinking: String::new() }],
            tools: vec![],
            max_tokens: 0,
            reasoning_effort: String::new(),
            images: vec![],
        };
        chat_openai(&def, req, tx, &[]).await.expect("stream");
        let mut calls = Vec::new();
        let mut done = false;
        while let Some(c) = rx.recv().await {
            calls.extend(c.tool_calls);
            if c.done {
                done = true;
                break;
            }
        }
        assert!(done);
        assert_eq!(calls.len(), 1, "same-frame finish must flush the call");
        assert_eq!(calls[0].name, "read");
        h.join().unwrap();
    }
}
