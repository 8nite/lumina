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
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::Arc,
};
use tokio::sync::broadcast;

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
    pub compat: (bool, bool),
    pub events: broadcast::Sender<serde_json::Value>,
}

impl AppState {
    pub fn new(store: Store, router: LlmRouter, compat: (bool, bool)) -> Self {
        let (events, _) = broadcast::channel(256);
        Self {
            store: Arc::new(store),
            router: Arc::new(router),
            auth_token: std::env::var("LUMINA_TOKEN").unwrap_or_default(),
            compat,
            events,
        }
    }

    fn emit(&self, v: serde_json::Value) {
        let _ = self.events.send(v);
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
        .route("/api/v1/files", get(files))
        .route("/api/v1/git/diff", get(git_diff))
        .route("/api/v1/git/branch", get(git_branch))
        .route("/api/v1/pty", get(todo))
        .route("/api/v1/providers", get(providers))
        .route("/api/v1/router", get(router_info).put(router_set))
        .route("/api/v1/models", get(models))
        .route("/api/v1/auth/device/start", get(todo))
        .route("/api/v1/auth/device/poll", get(todo))
        .route("/api/v1/kb/query", axum::routing::post(kb_query))
        .route("/api/v1/kb/", get(kb_browse))
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
    // Store/Router are internally synchronized; no outer lock is held across
    // the turn loop. Deltas stream to SSE subscribers via the event broker.
    st.emit(serde_json::json!({"type": "turn.started", "session": id}));
    let tx = st.events.clone();
    let sid = id.clone();
    let out = runner
        .run_prompt(&st.store, &st.router, &id, &b.text, &effort, st.compat, |d, t| {
            let _ = tx.send(serde_json::json!({"type": "turn.delta", "session": sid, "delta": d, "thinking": t}));
        })
        .await;
    match out {
        Ok(text) => {
            st.emit(serde_json::json!({"type": "turn.completed", "session": id}));
            Json(serde_json::json!({"session": id, "text": text})).into_response()
        }
        Err(e) => {
            let s = e.to_string();
            st.emit(serde_json::json!({"type": "turn.failed", "session": id, "error": s}));
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

async fn events(
    State(st): State<AppState>,
) -> Sse<impl futures_util::Stream<Item = Result<axum::response::sse::Event, axum::Error>>> {
    let rx = st.events.subscribe();
    let stream = futures_util::stream::unfold(rx, |mut rx| async {
        loop {
            match rx.recv().await {
                Ok(v) => {
                    let ev = axum::response::sse::Event::default()
                        .event(v.get("type").and_then(|t| t.as_str()).unwrap_or("message"))
                        .data(v.to_string());
                    return Some((Ok(ev), rx));
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => return None,
            }
        }
    });
    Sse::new(stream).keep_alive(axum::response::sse::KeepAlive::default())
}

#[derive(Deserialize)]
struct FilesQ {
    #[serde(default)]
    path: String,
}

/// Workspace file tree (dir entries) or capped file preview.
async fn files(Query(q): Query<FilesQ>) -> impl IntoResponse {
    let base = std::env::current_dir().unwrap_or_else(|_| ".".into());
    let target = if q.path.is_empty() { base.clone() } else { base.join(&q.path) };
    if target.is_dir() {
        let mut entries: Vec<serde_json::Value> = std::fs::read_dir(&target)
            .map(|rd| {
                rd.flatten()
                    .map(|e| {
                        serde_json::json!({
                            "name": e.file_name().to_string_lossy(),
                            "dir": e.file_type().map(|t| t.is_dir()).unwrap_or(false),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        entries.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
        return Json(serde_json::json!({"path": q.path, "entries": entries})).into_response();
    }
    match std::fs::read_to_string(&target) {
        Ok(text) => {
            let capped: String = text.chars().take(32_000).collect();
            Json(serde_json::json!({"path": q.path, "content": capped, "truncated": text.len() > capped.len()})).into_response()
        }
        Err(e) => (StatusCode::NOT_FOUND, Json(serde_json::json!({"error": e.to_string()}))).into_response(),
    }
}

fn git_out(args: &[&str]) -> Result<String, String> {
    let out = std::process::Command::new("git")
        .args(args)
        .output()
        .map_err(|e| format!("git: {e}"))?;
    if !out.status.success() {
        return Err(format!("git {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

async fn git_diff() -> impl IntoResponse {
    match git_out(&["diff", "--stat", "HEAD"]) {
        Ok(stat) => Json(serde_json::json!({"stat": stat})).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, Json(serde_json::json!({"error": e}))).into_response(),
    }
}

async fn git_branch() -> impl IntoResponse {
    match git_out(&["branch", "--show-current"]) {
        Ok(b) => Json(serde_json::json!({"branch": b.trim()})).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, Json(serde_json::json!({"error": e}))).into_response(),
    }
}

/// Configured providers (never secrets — names/protocols/URLs only).
async fn providers(State(st): State<AppState>) -> impl IntoResponse {
    let r = st.router.clone();
    let list: Vec<serde_json::Value> = {
        let mut v: Vec<_> = r
            .providers
            .iter()
            .map(|(name, p)| serde_json::json!({"name": name, "protocol": p.protocol, "base_url": p.base_url}))
            .collect();
        v.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
        v
    };
    Json(serde_json::json!({"providers": list})).into_response()
}

async fn router_info(State(st): State<AppState>) -> impl IntoResponse {
    let r = &*st.router;
    Json(serde_json::json!({
        "fast": r.model_for("fast", "build"),
        "executor": r.model_for("executor", "build"),
        "planner": r.model_for("planner", "plan"),
        "improver": r.model_for("improver", "build"),
    }))
    .into_response()
}

#[derive(Deserialize)]
struct RouterSet {
    role: String,
    model: String,
}

async fn router_set(State(st): State<AppState>, Json(b): Json<RouterSet>) -> impl IntoResponse {
    if !["fast", "executor", "planner", "improver"].contains(&b.role.as_str()) {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "bad role"}))).into_response();
    }
    st.router.set_model(&b.role, &b.model);
    st.emit(serde_json::json!({"type": "config.updated", "role": b.role, "model": b.model}));
    Json(serde_json::json!({"ok": true})).into_response()
}

#[derive(Deserialize)]
struct ModelsQ {
    #[serde(default)]
    provider: String,
}

async fn models(State(st): State<AppState>, Query(q): Query<ModelsQ>) -> impl IntoResponse {
    let r = st.router.clone();
    if q.provider.is_empty() {
        let names: Vec<&String> = {
            let mut v: Vec<&String> = r.providers.keys().collect();
            v.sort();
            v
        };
        return Json(serde_json::json!({"providers": names})).into_response();
    }
    let Some(def) = r.providers.get(&q.provider).cloned() else {
        return (StatusCode::NOT_FOUND, Json(serde_json::json!({"error": "unknown provider"}))).into_response();
    };
    match crate::llm::list_models(&def).await {
        Ok(models) => Json(serde_json::json!({"provider": q.provider, "models": models})).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, Json(serde_json::json!({"error": e.to_string()}))).into_response(),
    }
}

#[derive(Deserialize)]
struct KbQueryBody {
    #[serde(default)]
    query: String,
    #[serde(default)]
    budget: usize,
    #[serde(default)]
    directory: String,
}

async fn kb_query(Json(b): Json<KbQueryBody>) -> impl IntoResponse {
    let ws = if b.directory.is_empty() {
        std::env::current_dir().unwrap_or_else(|_| ".".into())
    } else {
        PathBuf::from(b.directory)
    };
    match crate::kb::query(&ws, &b.query, b.budget) {
        Ok(text) => Json(serde_json::json!({"query": b.query, "context": text})).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, Json(serde_json::json!({"error": e.to_string()}))).into_response(),
    }
}

/// Browse ./memory/*.md + graphify-out docs (?path= to read one).
async fn kb_browse(Query(q): Query<HashMap<String, String>>) -> impl IntoResponse {
    let base = std::env::current_dir().unwrap_or_else(|_| ".".into());
    if let Some(p) = q.get("path") {
        // Contain reads to memory/ + graphify-out/ (credential fence applies at the tool layer).
        let abs = base.join(p);
        let first: String = std::path::Path::new(p)
            .iter()
            .next()
            .map(|c| c.to_string_lossy().into_owned())
            .unwrap_or_default();
        let ok_prefix = first == "memory" || first == "graphify-out";
        if !ok_prefix {
            return (StatusCode::FORBIDDEN, Json(serde_json::json!({"error": "path must be under memory/ or graphify-out/"}))).into_response();
        }
        return match std::fs::read_to_string(&abs) {
            Ok(text) => Json(serde_json::json!({"path": p, "content": text.chars().take(32_000).collect::<String>()})).into_response(),
            Err(e) => (StatusCode::NOT_FOUND, Json(serde_json::json!({"error": e.to_string()}))).into_response(),
        };
    }
    let mut files = Vec::new();
    for dir in ["memory", "memory/graphify-out", "graphify-out"] {
        let d = base.join(dir);
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            if e.path().extension().map(|x| x == "md" || x == "json" || x == "html").unwrap_or(false) {
                files.push(format!("{dir}/{}", e.file_name().to_string_lossy()));
            }
        }
    }
    files.sort();
    Json(serde_json::json!({"files": files})).into_response()
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
