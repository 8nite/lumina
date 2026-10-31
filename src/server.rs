//! Daemon HTTP layer: axum router + SSE + embedded SPA.
//! Mirrors `internal/server/server.go` route surface (M1: sessions, prompt,
//! export, fork over rusqlite; events broker + PTY land in M2/M3).

use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{Html, IntoResponse, Json, Sse},
    routing::get,
    Router,
};
use rust_embed::RustEmbed;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, sync::Arc};

use crate::{agent::Runner, llm::Router as LlmRouter, store::Store};

#[derive(RustEmbed)]
#[folder = "web/dist/"]
#[exclude = "*.map"]
struct Spa;

#[derive(Clone)]
pub struct AppState {
    pub store: Arc<Store>,
    pub router: Arc<LlmRouter>,
    pub auth_token: String,
}

impl AppState {
    pub fn new(store: Store, router: LlmRouter) -> Self {
        Self {
            store: Arc::new(store),
            router: Arc::new(router),
            auth_token: std::env::var("LUMINA_TOKEN").unwrap_or_default(),
        }
    }
}

/// Non-loopback binds require LUMINA_TOKEN (checked per request in M2; helper here).
pub fn bearer_ok(headers: &HeaderMap, token: &str) -> bool {
    if token.is_empty() {
        return false;
    }
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v == format!("Bearer {token}"))
}

pub fn is_loopback(host: &str) -> bool {
    host == "127.0.0.1" || host == "::1" || host == "localhost"
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/openapi.json", get(openapi))
        .route("/api/v1/events", get(events))
        .route("/api/v1/sessions", get(list_sessions).post(create_session))
        .route("/api/v1/sessions/:id", get(get_session).patch(patch_session))
        .route("/api/v1/sessions/:id/prompt", axum::routing::post(prompt))
        .route("/api/v1/sessions/:id/fork", axum::routing::post(fork))
        .route("/api/v1/sessions/:id/export", get(export))
        .route("/api/v1/sessions/:id/revert", axum::routing::post(todo))
        .route("/api/v1/sessions/:id/unrevert", axum::routing::post(todo))
        .route("/api/v1/permissions/:id", axum::routing::post(todo))
        .route("/api/v1/questions/:id", axum::routing::post(todo))
        .route("/api/v1/files", get(todo))
        .route("/api/v1/git/diff", get(todo))
        .route("/api/v1/git/branch", get(todo))
        .route("/api/v1/pty", get(todo))
        .route("/api/v1/providers", get(todo))
        .route("/api/v1/router", get(todo).put(todo))
        .route("/api/v1/models", get(todo))
        .route("/api/v1/auth/device/start", get(todo))
        .route("/api/v1/auth/device/poll", get(todo))
        .route("/api/v1/kb/query", axum::routing::post(todo))
        .route("/api/v1/kb/", get(todo))
        .route("/api/v1/harness", get(todo))
        .route("/api/v1/harness/evolve", axum::routing::post(todo))
        .fallback(spa_fallback)
        .with_state(state)
}

#[derive(Deserialize)]
struct ListQ {
    directory: Option<String>,
    latest: Option<String>,
}

#[derive(Serialize)]
struct SessionView {
    id: String,
    parent_id: String,
    workspace_dir: String,
    agent: String,
    permission_mode: String,
    model: String,
    title: String,
}

fn view(s: &crate::store::Session) -> SessionView {
    SessionView {
        id: s.id.clone(),
        parent_id: s.parent_id.clone(),
        workspace_dir: s.workspace_dir.clone(),
        agent: s.agent.clone(),
        permission_mode: s.permission_mode.clone(),
        model: s.model.clone(),
        title: s.title.clone(),
    }
}

async fn list_sessions(
    State(st): State<AppState>,
    Query(q): Query<ListQ>,
) -> impl IntoResponse {
    let store = &st.store;
    if q.latest.as_deref() == Some("1") {
        let dir = q.directory.unwrap_or_default();
        return match store.latest_for_dir(&dir) {
            Ok(Some(s)) => Json(serde_json::json!(view(&s))).into_response(),
            Ok(None) => (StatusCode::NOT_FOUND, Json(serde_json::json!({"error": "no session"}))).into_response(),
            Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error": e.to_string()}))).into_response(),
        };
    }
    match store.list_for_dir(q.directory.as_deref()) {
        Ok(v) => Json(v.iter().map(view).collect::<Vec<_>>()).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error": e.to_string()}))).into_response(),
    }
}

#[derive(Deserialize)]
struct CreateBody {
    #[serde(default)]
    workspace_dir: String,
    #[serde(default = "default_agent")]
    agent: String,
    #[serde(default = "default_mode")]
    permission_mode: String,
    #[serde(default)]
    model: String,
}
fn default_agent() -> String {
    "build".into()
}
fn default_mode() -> String {
    "bypass".into()
}

async fn create_session(State(st): State<AppState>, Json(b): Json<CreateBody>) -> impl IntoResponse {
    let ws = if b.workspace_dir.is_empty() {
        std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_else(|_| ".".into())
    } else {
        b.workspace_dir
    };
    let store = &st.store;
    match store.create_session(&ws, &b.agent, &b.permission_mode, &b.model) {
        Ok(s) => (StatusCode::CREATED, Json(serde_json::json!(view(&s)))).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error": e.to_string()}))).into_response(),
    }
}

async fn get_session(State(st): State<AppState>, Path(id): Path<String>) -> impl IntoResponse {
    let store = &st.store;
    match store.get_session(&id) {
        Ok(Some(s)) => Json(serde_json::json!(view(&s))).into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, Json(serde_json::json!({"error": "not found"}))).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error": e.to_string()}))).into_response(),
    }
}

#[derive(Deserialize)]
struct PatchBody {
    agent: Option<String>,
    permission_mode: Option<String>,
    model: Option<String>,
    title: Option<String>,
}

async fn patch_session(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Json(b): Json<PatchBody>,
) -> impl IntoResponse {
    // ponytail: validate enums at the boundary, not in the store.
    if let Some(a) = &b.agent {
        if !["plan", "build"].contains(&a.as_str()) && !a.starts_with("custom:") && a != &"custom" {
            // Custom primary agents (agents/*.md) arrive in M2; accept `plan|build` now.
            if !["plan", "build"].contains(&a.as_str()) {
                return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "agent must be plan|build"}))).into_response();
            }
        }
    }
    if let Some(m) = &b.permission_mode {
        if !["ask", "auto-edit", "auto-approve", "bypass"].contains(&m.as_str()) {
            return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "bad permission_mode"}))).into_response();
        }
    }
    let store = &st.store;
    match store.update_session(&id, b.agent.as_deref(), b.permission_mode.as_deref(), b.model.as_deref(), b.title.as_deref()) {
        Ok(Some(s)) => Json(serde_json::json!(view(&s))).into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, Json(serde_json::json!({"error": "not found"}))).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error": e.to_string()}))).into_response(),
    }
}

#[derive(Deserialize)]
struct PromptBody {
    #[serde(default)]
    text: String,
    #[serde(default)]
    effort: String,
}

async fn prompt(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Json(b): Json<PromptBody>,
) -> impl IntoResponse {
    if b.text.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "empty prompt"}))).into_response();
    }
    let effort = crate::llm::normalize_effort(&b.effort).to_string();
    let runner = Runner::new();
    // Store/Router are internally synchronized (single-writer SQLite, RwLock
    // overrides); no outer lock is held across the turn loop. M2 moves this
    // onto a per-session task queue with SSE progress.
    let out = runner
        .run_prompt(&st.store, &st.router, &id, &b.text, &effort, |_, _| {})
        .await;
    match out {
        Ok(text) => Json(serde_json::json!({"session": id, "text": text})).into_response(),
        Err(e) => {
            let s = e.to_string();
            if s.contains("not found") {
                (StatusCode::NOT_FOUND, Json(serde_json::json!({"error": s}))).into_response()
            } else {
                (StatusCode::BAD_GATEWAY, Json(serde_json::json!({"error": s}))).into_response()
            }
        }
    }
}

#[derive(Deserialize)]
struct ForkBody {
    #[serde(default)]
    turn: Option<String>,
}

async fn fork(
    State(st): State<AppState>,
    Path(id): Path<String>,
    body: Option<Json<ForkBody>>,
) -> impl IntoResponse {
    let at = body.and_then(|b| b.turn.clone());
    let store = &st.store;
    match store.fork(&id, at.as_deref()) {
        Ok(s) => (StatusCode::CREATED, Json(serde_json::json!(view(&s)))).into_response(),
        Err(e) => (StatusCode::NOT_FOUND, Json(serde_json::json!({"error": e.to_string()}))).into_response(),
    }
}

async fn export(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let sanitize = q.get("sanitize").map(|v| v == "1" || v == "true").unwrap_or(false);
    let store = &st.store;
    match store.export_markdown(&id, sanitize) {
        Ok(md) => ([("content-type", "text/markdown")], md).into_response(),
        Err(e) => (StatusCode::NOT_FOUND, Json(serde_json::json!({"error": e.to_string()}))).into_response(),
    }
}

async fn openapi() -> impl IntoResponse {
    Json(serde_json::json!({
        "openapi": "3.1.0",
        "info": {"title": "Lumina daemon", "version": "0.1.0"},
        "paths": {
            "/api/v1/events": "SSE: turn/context/delta/tool/completed/failed, permission.asked/resolved, question.asked/resolved, session.*, config.updated",
            "/api/v1/sessions": "list (?directory=, ?latest=1) / create",
            "/api/v1/sessions/{id}": "fetch / patch agent,permission,model,title",
            "/api/v1/sessions/{id}/prompt": "submit turn",
            "/api/v1/sessions/{id}/fork": "branch at turn",
            "/api/v1/sessions/{id}/export": "transcript (?sanitize=1)",
            "/api/v1/sessions/{id}/revert": "shadow-git undo (M2)",
            "/api/v1/permissions/{reqId}": "allow_once/session/always|deny (M3)",
            "/api/v1/files": "workspace tree (M2)",
            "/api/v1/pty": "websocket shell (M2)",
            "/api/v1/kb/query": "graphify+memory query (M2)",
            "/api/v1/harness/evolve": "Propose-Probe-Compose (M5)"
        }
    }))
}

async fn events() -> Sse<impl futures_util::Stream<Item = Result<axum::response::sse::Event, axum::Error>>> {
    // M2 wires the real broadcast broker; keep the route + headers correct now.
    let stream = futures_util::stream::once(async {
        Ok(axum::response::sse::Event::default()
            .event("hello")
            .data("{\"status\":\"events land in M2\"}"))
    });
    Sse::new(stream)
}

async fn todo() -> impl IntoResponse {
    (StatusCode::NOT_IMPLEMENTED, Json(serde_json::json!({"error": "not yet ported (see plan.md milestones)"})))
}

async fn spa_fallback(uri: axum::http::Uri) -> impl IntoResponse {
    let _ = uri;
    if let Some(f) = Spa::get("index.html") {
        let html = String::from_utf8_lossy(&f.data).into_owned();
        (
            StatusCode::OK,
            [(axum::http::header::CONTENT_SECURITY_POLICY, "default-src 'self'")],
            Html(html),
        )
            .into_response()
    } else {
        (
            StatusCode::OK,
            Json(serde_json::json!({"ok": true, "hint": "run `pnpm --dir web install && pnpm --dir web build`, then restart `lumina serve`"})),
        )
            .into_response()
    }
}
