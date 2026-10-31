//! Typed blocking client for the daemon HTTP + SSE API.
//! Mirrors `pkg/sdk` (Go). The TUI runs synchronously on its own thread, so
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
    TurnCompleted { session: String },
    TurnFailed { session: String, error: String },
    PermissionAsked { req_id: String, session: String, tool: String, target: String },
    PermissionResolved { req_id: String, decision: String },
    QuestionAsked { req_id: String, session: String, questions: Vec<QuestionView> },
    ConfigUpdated { role: String, model: String },
    Other { kind: String, raw: String },
}

#[derive(Debug, Clone, Deserialize)]
pub struct QuestionView {
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub options: Vec<String>,
}

fn classify(v: &serde_json::Value) -> Event {
    let kind = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
    let s = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
    match kind {
        "turn.started" => Event::TurnStarted { session: s("session") },
        "turn.delta" => Event::TurnDelta { session: s("session"), delta: s("delta"), thinking: s("thinking") },
        "turn.completed" => Event::TurnCompleted { session: s("session") },
        "turn.failed" => Event::TurnFailed { session: s("session"), error: s("error") },
        "permission.asked" => Event::PermissionAsked {
            req_id: s("req_id"), session: s("session"), tool: s("tool"), target: s("target"),
        },
        "permission.resolved" => Event::PermissionResolved { req_id: s("req_id"), decision: s("decision") },
        "question.asked" => Event::QuestionAsked {
            req_id: s("req_id"),
            session: s("session"),
            questions: v.get("questions").and_then(|q| serde_json::from_value(q.clone()).ok()).unwrap_or_default(),
        },
        "config.updated" => Event::ConfigUpdated { role: s("role"), model: s("model") },
        _ => Event::Other { kind: kind.into(), raw: v.to_string() },
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

    pub fn submit(&self, id: &str, text: &str, effort: &str) -> Result<String, SdkError> {
        let res = Self::check(
            self.req(reqwest::Method::POST, &format!("/api/v1/sessions/{id}/prompt"))
                .json(&serde_json::json!({"text": text, "effort": effort}))
                .send()?,
        )?;
        let v: serde_json::Value = res.json()?;
        Ok(v.get("text").and_then(|t| t.as_str()).unwrap_or("").to_string())
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

    pub fn answer_question(&self, req_id: &str, answers: &[String]) -> Result<(), SdkError> {
        Self::check(
            self.req(reqwest::Method::POST, &format!("/api/v1/questions/{req_id}"))
                .json(&serde_json::json!({"answers": answers}))
                .send()?,
        )?;
        Ok(())
    }

    pub fn router(&self) -> Result<serde_json::Value, SdkError> {
        Ok(Self::check(self.req(reqwest::Method::GET, "/api/v1/router").send()?)?.json()?)
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
