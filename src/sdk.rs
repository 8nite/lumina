//! Typed blocking client for the daemon HTTP + SSE API.
//! The TUI runs synchronously on its own thread, so
//! this uses `reqwest::blocking`; the daemon side stays fully async.

use serde::{Deserialize, Serialize};
use std::time::Duration;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum SdkError {
    #[error("sdk: {0}")]
    Msg(String),
    #[error(transparent)]
    Http(#[from] reqwest::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

/// Daemon session view (wire schema; fields populate pickers and headers).
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct SubmitOut {
    pub text: String,
    pub queued: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionView {
    pub id: String,
    #[serde(default)]
    pub parent_id: String,
    #[serde(default)]
    pub workspace_dir: String,
    #[serde(default)]
    pub agent: String,
    #[serde(default)]
    pub permission_mode: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub title: String,
}

#[derive(Debug, Clone)]
pub enum Event {
    TurnStarted { session: String },
    TurnDelta { session: String, delta: String, thinking: String },
    TurnProgress { session: String, stage: String },
    TurnTool {
        session: String,
        phase: String,
        tool: String,
        target: String,
        preview: String,
        error: bool,
        duration_ms: u64,
    },
    TurnCompleted { session: String, ms: u64, chars: usize, text: String, model: String },
    TurnFailed { session: String, error: String },
    PermissionAsked { req_id: String, session: String, tool: String, target: String },
    PermissionResolved { req_id: String, decision: String },
    QuestionAsked { req_id: String, session: String, questions: Vec<QuestionView> },
    QuestionResolved { req_id: String },
    ConfigUpdated { role: String, model: String },
    Other { kind: String, raw: String },
}

#[derive(Debug, Clone, Deserialize)]
pub struct QuestionOptionView {
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub description: String,
}

/// Unknown option shapes degrade to plain text (decodeQuestionItem parity).
fn decode_options(v: &serde_json::Value) -> Vec<QuestionOptionView> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|o| {
                    if let Some(label) = o.get("label").and_then(|l| l.as_str()) {
                        if label.is_empty() {
                            return None;
                        }
                        Some(QuestionOptionView {
                            label: label.into(),
                            description: o
                                .get("description")
                                .and_then(|x| x.as_str())
                                .unwrap_or("")
                                .into(),
                        })
                    } else {
                        o.as_str().map(|label| QuestionOptionView { label: label.into(), description: String::new() })
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}

#[derive(Debug, Clone, Deserialize)]
pub struct QuestionView {
    #[serde(default)]
    pub question: String,
    #[serde(default)]
    pub header: String,
    #[serde(default)]
    pub options: Vec<QuestionOptionView>,
    #[serde(default)]
    pub multiple: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CommandView {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub body: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AgentView {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub mode: String,
    #[serde(default)]
    pub description: String,
}

fn classify(v: &serde_json::Value) -> Event {
    let kind = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
    // Go envelope: {type, session_id?, data?}. Flat legacy payloads
    // (pre-envelope daemons) are tolerated field-by-field below.
    let sid = v
        .get("session_id")
        .or_else(|| v.get("session"))
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    let s = |k: &str| {
        v.get("data")
            .and_then(|d| d.get(k))
            .or_else(|| v.get(k))
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string()
    };
    let n = |k: &str| {
        v.get("data")
            .and_then(|d| d.get(k))
            .or_else(|| v.get(k))
            .and_then(|m| m.as_u64())
            .unwrap_or(0)
    };
    match kind {
        "turn.started" => Event::TurnStarted { session: sid },
        "turn.delta" => Event::TurnDelta {
            session: sid,
            delta: s("delta"),
            thinking: s("thinking"),
        },
        "turn.progress" => Event::TurnProgress { session: sid, stage: s("stage") },
        "turn.tool" => {
            let phase = s("phase");
            let tool = s("tool");
            let target = s("target");
            let preview = s("preview");
            let error = v
                .get("data")
                .and_then(|d| d.get("error"))
                .or_else(|| v.get("error"))
                .and_then(|e| e.as_bool())
                .unwrap_or(false);
            let duration_ms = n("duration_ms");
            Event::TurnTool { session: sid, phase, tool, target, preview, error, duration_ms }
        }
        "turn.completed" => {
            let text = s("text");
            let model = s("model");
            Event::TurnCompleted {
                session: sid,
                ms: {
                    let d = n("duration_ms");
                    if d > 0 { d } else { n("ms") }
                },
                chars: {
                    let c = n("chars");
                    if c > 0 { c as usize } else { text.len() }
                },
                text,
                model,
            }
        }
        "turn.failed" => Event::TurnFailed { session: sid, error: s("error") },
        "turn.context" => {
            let (used, max) = {
                let u = n("context_tokens");
                let u = if u > 0 { u } else { n("used") };
                let m = n("max_tokens");
                let m = if m > 0 { m } else { n("max") };
                (u, m)
            };
            Event::Other { kind: "turn.context".into(), raw: serde_json::json!({"used": used, "max": max}).to_string() }
        }
        "permission.asked" => Event::PermissionAsked {
            req_id: s("request_id").if_empty(s("req_id")),
            session: sid,
            tool: s("tool"),
            target: s("target"),
        },
        "permission.resolved" => Event::PermissionResolved {
            req_id: s("request_id").if_empty(s("req_id")),
            decision: s("decision"),
        },
        "question.asked" => Event::QuestionAsked {
            req_id: s("request_id").if_empty(s("req_id")),
            session: sid,
            questions: v
                .get("data")
                .and_then(|d| d.get("questions"))
                .or_else(|| v.get("questions"))
                .and_then(|q| q.as_array())
                .map(|a| {
                    a.iter()
                        .map(|q| QuestionView {
                            question: q
                                .get("question")
                                .or_else(|| q.get("text"))
                                .and_then(|x| x.as_str())
                                .unwrap_or("")
                                .into(),
                            header: q.get("header").and_then(|x| x.as_str()).unwrap_or("").into(),
                            options: q.get("options").map(decode_options).unwrap_or_default(),
                            multiple: q.get("multiple").and_then(|x| x.as_bool()).unwrap_or(false),
                        })
                        .collect()
                })
                .unwrap_or_default(),
        },
        "question.resolved" => Event::QuestionResolved { req_id: s("request_id").if_empty(s("req_id")) },
        "config.updated" => Event::ConfigUpdated { role: s("role"), model: s("model") },
        _ => Event::Other { kind: kind.into(), raw: v.to_string() },
    }
}

trait IfEmpty {
    fn if_empty(self, alt: String) -> String;
}

impl IfEmpty for String {
    fn if_empty(self, alt: String) -> String {
        if self.is_empty() {
            alt
        } else {
            self
        }
    }
}

#[derive(Clone)]
pub struct Sdk {
    base: String,
    token: String,
    http: reqwest::blocking::Client,
}

impl Sdk {
    pub fn new(base: &str, token: &str) -> Self {
        Self {
            base: base.trim_end_matches('/').to_string(),
            token: token.to_string(),
            http: reqwest::blocking::Client::builder()
                .user_agent("lumina-tui/0.1.0")
                .timeout(Duration::from_secs(600))
                .build()
                .expect("blocking client"),
        }
    }

    fn req(&self, m: reqwest::Method, path: &str) -> reqwest::blocking::RequestBuilder {
        let mut r = self.http.request(m, format!("{}{}", self.base, path));
        if !self.token.is_empty() {
            r = r.bearer_auth(&self.token);
        }
        r
    }

    fn check(res: reqwest::blocking::Response) -> Result<reqwest::blocking::Response, SdkError> {
        if !res.status().is_success() {
            let st = res.status();
            let t = res.text().unwrap_or_default();
            return Err(SdkError::Msg(format!("{st}: {t}")));
        }
        Ok(res)
    }

    pub fn list_sessions(&self, dir: Option<&str>, all: bool) -> Result<Vec<SessionView>, SdkError> {
        let mut r = self.req(reqwest::Method::GET, "/api/v1/sessions");
        if !all {
            if let Some(d) = dir {
                r = r.query(&[("directory", d)]);
            }
        }
        Ok(Self::check(r.send()?)?.json()?)
    }

    pub fn latest(&self, dir: &str) -> Result<SessionView, SdkError> {
        let res = Self::check(
            self.req(reqwest::Method::GET, "/api/v1/sessions")
                .query(&[("directory", dir), ("latest", "1")])
                .send()?,
        )?;
        Ok(res.json()?)
    }

    pub fn create(&self, ws: &str, agent: &str, mode: &str, model: &str) -> Result<SessionView, SdkError> {
        let res = Self::check(
            self.req(reqwest::Method::POST, "/api/v1/sessions").json(&serde_json::json!({
                "workspace_dir": ws, "agent": agent, "permission_mode": mode, "model": model,
            })).send()?,
        )?;
        Ok(res.json()?)
    }

    pub fn get(&self, id: &str) -> Result<SessionView, SdkError> {
        Ok(Self::check(self.req(reqwest::Method::GET, &format!("/api/v1/sessions/{id}")).send()?)?.json()?)
    }

    pub fn patch(&self, id: &str, body: &serde_json::Value) -> Result<SessionView, SdkError> {
        Ok(Self::check(self.req(reqwest::Method::PATCH, &format!("/api/v1/sessions/{id}")).json(body).send()?)?.json()?)
    }

    pub fn submit(&self, id: &str, text: &str, effort: &str) -> Result<SubmitOut, SdkError> {
        self.submit_delivery(id, text, effort, "steer")
    }

    /// Submit with explicit inbox behavior: steer (default) joins the running
    /// turn; queue chains behind it (Go PromptInput.Delivery parity).
    pub fn submit_delivery(&self, id: &str, text: &str, effort: &str, delivery: &str) -> Result<SubmitOut, SdkError> {
        let res = Self::check(
            self.req(reqwest::Method::POST, &format!("/api/v1/sessions/{id}/prompt"))
                .json(&serde_json::json!({"text": text, "effort": effort, "delivery": delivery}))
                .send()?,
        )?;
        let v: serde_json::Value = res.json()?;
        if v.get("status").and_then(|x| x.as_str()) == Some("queued")
            || v.get("queued").and_then(|q| q.as_bool()).unwrap_or(false)
        {
            return Ok(SubmitOut { text: String::new(), queued: true });
        }
        Ok(SubmitOut {
            text: v.get("text").and_then(|t| t.as_str()).unwrap_or("").to_string(),
            queued: false,
        })
    }

    pub fn submit_text(&self, id: &str, text: &str, effort: &str) -> Result<String, SdkError> {
        Ok(self.submit(id, text, effort)?.text)
    }

    pub fn fork(&self, id: &str, turn: Option<&str>) -> Result<SessionView, SdkError> {
        let res = Self::check(
            self.req(reqwest::Method::POST, &format!("/api/v1/sessions/{id}/fork"))
                .json(&serde_json::json!({"turn": turn}))
                .send()?,
        )?;
        Ok(res.json()?)
    }

    pub fn state(&self, id: &str) -> Result<serde_json::Value, SdkError> {
        Ok(Self::check(self.req(reqwest::Method::GET, &format!("/api/v1/sessions/{id}/state")).send()?)?.json()?)
    }

    pub fn answer_permission(&self, req_id: &str, decision: &str) -> Result<(), SdkError> {
        Self::check(
            self.req(reqwest::Method::POST, &format!("/api/v1/permissions/{req_id}"))
                .json(&serde_json::json!({"decision": decision}))
                .send()?,
        )?;
        Ok(())
    }

    pub fn answer_question(&self, req_id: &str, answers: &[Vec<String>]) -> Result<(), SdkError> {
        Self::check(
            self.req(reqwest::Method::POST, &format!("/api/v1/questions/{req_id}"))
                .json(&serde_json::json!({"answers": answers}))
                .send()?,
        )?;
        Ok(())
    }

    /// Close a question prompt without answers (Go DismissQuestion parity).
    pub fn dismiss_question(&self, req_id: &str) -> Result<(), SdkError> {
        Self::check(
            self.req(reqwest::Method::POST, &format!("/api/v1/questions/{req_id}"))
                .json(&serde_json::json!({"dismissed": true}))
                .send()?,
        )?;
        Ok(())
    }

    pub fn set_router(&self, role: &str, model: &str) -> Result<(), SdkError> {
        Self::check(
            self.req(reqwest::Method::PUT, "/api/v1/router")
                .json(&serde_json::json!({"role": role, "model": model}))
                .send()?,
        )?;
        Ok(())
    }

    pub fn models(&self, provider: &str) -> Result<Vec<serde_json::Value>, SdkError> {
        let v: serde_json::Value = Self::check(
            self.req(reqwest::Method::GET, "/api/v1/models").query(&[("provider", provider)]).send()?,
        )?
        .json()?;
        Ok(v.get("models").and_then(|m| m.as_array()).cloned().unwrap_or_default())
    }

    pub fn providers(&self) -> Result<Vec<serde_json::Value>, SdkError> {
        let v: serde_json::Value = Self::check(self.req(reqwest::Method::GET, "/api/v1/providers").send()?)?.json()?;
        Ok(v.get("providers").and_then(|m| m.as_array()).cloned().unwrap_or_default())
    }

    pub fn kb_query(&self, q: &str) -> Result<String, SdkError> {
        let v: serde_json::Value = Self::check(
            self.req(reqwest::Method::POST, "/api/v1/kb/query").json(&serde_json::json!({"query": q})).send()?,
        )?
        .json()?;
        Ok(v.get("context").and_then(|c| c.as_str()).unwrap_or("").to_string())
    }

    pub fn commands(&self, dir: &str) -> Result<Vec<CommandView>, SdkError> {
        let v: serde_json::Value =
            Self::check(self.req(reqwest::Method::GET, "/api/v1/commands").query(&[("directory", dir)]).send()?)?.json()?;
        Ok(v.get("commands").and_then(|c| serde_json::from_value(c.clone()).ok()).unwrap_or_default())
    }

    pub fn agents(&self, dir: &str) -> Result<Vec<AgentView>, SdkError> {
        let v: serde_json::Value =
            Self::check(self.req(reqwest::Method::GET, "/api/v1/agents").query(&[("directory", dir)]).send()?)?.json()?;
        Ok(v.get("agents").and_then(|a| serde_json::from_value(a.clone()).ok()).unwrap_or_default())
    }

    pub fn auth_start(&self, provider: &str, github_url: &str) -> Result<serde_json::Value, SdkError> {
        Ok(Self::check(
            self.req(reqwest::Method::GET, "/api/v1/auth/device/start")
                .query(&[("provider", provider), ("github_url", github_url)])
                .send()?,
        )?
        .json()?)
    }

    pub fn auth_poll(&self, provider: &str, github_url: &str, device_code: &str) -> Result<serde_json::Value, SdkError> {
        Ok(Self::check(
            self.req(reqwest::Method::GET, "/api/v1/auth/device/poll")
                .query(&[("provider", provider), ("github_url", github_url), ("device_code", device_code)])
                .send()?,
        )?
        .json()?)
    }

    pub fn evolve(&self) -> Result<String, SdkError> {
        let v: serde_json::Value =
            Self::check(self.req(reqwest::Method::POST, "/api/v1/harness/evolve").send()?)?.json()?;
        Ok(v.get("result").and_then(|r| r.as_str()).unwrap_or("accepted").to_string())
    }

    pub fn revert(&self, id: &str, turn: Option<&str>) -> Result<String, SdkError> {
        let v: serde_json::Value = Self::check(
            self.req(reqwest::Method::POST, &format!("/api/v1/sessions/{id}/revert"))
                .json(&serde_json::json!({"turn": turn}))
                .send()?,
        )?
        .json()?;
        Ok(v.get("commit").and_then(|c| c.as_str()).unwrap_or("").to_string())
    }

    pub fn unrevert(&self, id: &str) -> Result<String, SdkError> {
        let v: serde_json::Value =
            Self::check(self.req(reqwest::Method::POST, &format!("/api/v1/sessions/{id}/unrevert")).send()?)?.json()?;
        Ok(v.get("commit").and_then(|c| c.as_str()).unwrap_or("").to_string())
    }

    pub fn compact(&self, id: &str) -> Result<String, SdkError> {
        let v: serde_json::Value =
            Self::check(self.req(reqwest::Method::POST, &format!("/api/v1/sessions/{id}/compact")).send()?)?.json()?;
        Ok(v.get("result").and_then(|r| r.as_str()).unwrap_or("").to_string())
    }

    pub fn export(&self, id: &str) -> Result<String, SdkError> {
        let res = Self::check(self.req(reqwest::Method::GET, &format!("/api/v1/sessions/{id}/export")).send()?)?;
        Ok(res.text()?)
    }

    /// Subscribe to SSE; returns a receiver fed by a background thread.
    /// The thread ends when the receiver is dropped.
    pub fn subscribe(&self) -> std::sync::mpsc::Receiver<Event> {
        let (tx, rx) = std::sync::mpsc::channel();
        let base = self.base.clone();
        let token = self.token.clone();
        std::thread::spawn(move || {
            let mut backoff = Duration::from_secs(1);
            let mut reconnected = false;
            loop {
                let mut req = reqwest::blocking::Client::builder()
                    .timeout(None)
                    .build()
                    .unwrap()
                    .get(format!("{base}/api/v1/events"))
                    .header("Accept", "text/event-stream");
                if !token.is_empty() {
                    req = req.bearer_auth(&token);
                }
                let resp = match req.send() {
                    Ok(r) => r,
                    Err(_) => {
                        std::thread::sleep(backoff);
                        backoff = (backoff * 2).min(Duration::from_secs(15));
                        continue;
                    }
                };
                backoff = Duration::from_secs(1);
                if reconnected {
                    // Signal the UI to resync (running/modals) and repair the
                    // transcript after a dropped stream (Go RequestResyncMsg).
                    let _ = tx.send(Event::Other { kind: "sse.reconnected".into(), raw: String::new() });
                }
                reconnected = true;
                use std::io::BufRead;
                let reader = std::io::BufReader::new(resp);
                let (mut kind, mut data) = (String::from("message"), String::new());
                for line in reader.lines() {
                    let Ok(line) = line else { break };
                    if line.is_empty() {
                        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&data) {
                            let mut ev = classify(&v);
                            // Preserve the SSE event name when the payload lacks a type.
                            if matches!(ev, Event::Other { .. }) && kind != "message" {
                                ev = Event::Other { kind: kind.clone(), raw: data.clone() };
                            }
                            if tx.send(ev).is_err() {
                                return; // UI gone
                            }
                        }
                        kind.clear();
                        kind.push_str("message");
                        data.clear();
                        continue;
                    }
                    if let Some(k) = line.strip_prefix("event:") {
                        kind = k.trim().to_string();
                    } else if let Some(d) = line.strip_prefix("data:") {
                        data.push_str(d.trim());
                    }
                }
                // Stream broke; reconnect with backoff unless the UI is gone.
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(Duration::from_secs(15));
            }
        });
        rx
    }
}
