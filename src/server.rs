//! Daemon HTTP layer: axum router + SSE + embedded SPA.
//! Mirrors `internal/server/server.go` route surface (M1: sessions, prompt,
//! export, fork over rusqlite; events broker + PTY land in M2/M3).

use axum::{
    extract::{ConnectInfo, Path, Query, State},
    http::{HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{Html, IntoResponse, Json, Sse},
    routing::get,
    Router,
};
use rust_embed::RustEmbed;
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::{Arc, Mutex},
};
use tokio::sync::broadcast;

use crate::{agent::Runner, approve::ApprovalGate, config::Config, kb::Sweeper as KbSweeper, llm::Router as LlmRouter, plugins::Cache as PluginCache, store::Store};

#[derive(RustEmbed)]
#[folder = "web/dist/"]
#[exclude = "*.map"]
struct Spa;

#[derive(Clone)]
pub struct AppState {
    pub store: Arc<Store>,
    pub router: Arc<LlmRouter>,
    pub auth_token: String,
    pub cfg: Config,
    pub events: broadcast::Sender<serde_json::Value>,
    pub gate: ApprovalGate,
    pub running: Arc<Mutex<HashSet<String>>>,
    pub queued: Arc<Mutex<HashMap<String, std::collections::VecDeque<QueuedPrompt>>>>,
    pub plugins: PluginCache,
    pub sweeper: KbSweeper,
    pub cfg_path: Option<PathBuf>,
}

impl AppState {
    pub fn new(store: Store, router: LlmRouter, cfg: Config, cfg_path: Option<PathBuf>) -> Self {
        let (events, _) = broadcast::channel(256);
        Self {
            gate: ApprovalGate::new(events.clone()),
            store: Arc::new(store),
            router: Arc::new(router),
            auth_token: std::env::var("LUMINA_TOKEN").unwrap_or_default(),
            cfg,
            events,
            running: Arc::new(Mutex::new(HashSet::new())),
            queued: Arc::new(Mutex::new(HashMap::new())),
            plugins: PluginCache::new(),
            sweeper: KbSweeper::new(),
            cfg_path,
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

/// Non-loopback binds require LUMINA_TOKEN (checked per request; loopback is
/// trusted like the Go daemon).
async fn require_token(
    State(token): State<String>,
    ConnectInfo(addr): ConnectInfo<std::net::SocketAddr>,
    req: axum::http::Request<axum::body::Body>,
    next: Next,
) -> impl IntoResponse {
    if is_loopback(&addr.ip().to_string()) || bearer_ok(req.headers(), &token) {
        next.run(req).await.into_response()
    } else {
        (StatusCode::UNAUTHORIZED, Json(serde_json::json!({"error": "LUMINA_TOKEN required"}))).into_response()
    }
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
        .route("/api/v1/sessions/:id/revert", axum::routing::post(revert))
        .route("/api/v1/sessions/:id/unrevert", axum::routing::post(unrevert))
        .route("/api/v1/sessions/:id/compact", axum::routing::post(compact))
        .route("/api/v1/sessions/:id/state", get(session_state))
        .route("/api/v1/permissions/:id", axum::routing::post(answer_permission))
        .route("/api/v1/questions/:id", axum::routing::post(answer_question))
        .route("/api/v1/files", get(files))
        .route("/api/v1/git/diff", get(git_diff))
        .route("/api/v1/git/branch", get(git_branch))
        .route("/api/v1/pty", get(crate::pty::pty))
        .route("/api/v1/providers", get(providers))
        .route("/api/v1/router", get(router_info).put(router_set))
        .route("/api/v1/models", get(models))
        .route("/api/v1/auth/device/start", get(auth_device_start))
        .route("/api/v1/auth/device/poll", get(auth_device_poll))
        .route("/api/v1/kb/query", axum::routing::post(kb_query))
        .route("/api/v1/kb/", get(kb_browse))
        .route("/api/v1/mcp/tools", get(mcp_tools))
        .route("/api/v1/commands", get(commands))
        .route("/api/v1/agents", get(agents))
        .route("/api/v1/harness", get(harness_info))
        .route("/api/v1/harness/evolve", axum::routing::post(harness_evolve))
        .fallback(spa_fallback)
        .with_state(state.clone())
        .route_layer(middleware::from_fn_with_state(
            state.auth_token.clone(),
            require_token,
        ))
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
        if !["plan", "build"].contains(&a.as_str()) {
            // Custom primary agents (agents/*.md with mode: primary).
            let ws = st
                .store
                .get_session(&id)
                .ok()
                .flatten()
                .map(|s| s.workspace_dir)
                .unwrap_or_else(|| ".".into());
            let bundle = crate::rules::Bundle::load(std::path::Path::new(&ws), true, true);
            let ok = bundle.agents.iter().any(|ag| &ag.name == a && ag.mode == "primary");
            if !ok {
                return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "agent must be plan|build or a custom primary agent"}))).into_response();
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

/// A prompt chained behind a running turn (server-side queue).
#[derive(Clone)]
struct QueuedPrompt {
    text: String,
    effort: String,
    images: Vec<crate::llm::ImageAttachment>,
}

#[derive(Deserialize)]
struct PromptBody {
    #[serde(default)]
    text: String,
    #[serde(default)]
    effort: String,
    /// Attached images (8MB cap each, vision-capable models only).
    #[serde(default)]
    images: Vec<ImageIn>,
}

#[derive(Deserialize)]
struct ImageIn {
    #[serde(default)]
    name: String,
    #[serde(default)]
    media_type: String,
    #[serde(default)]
    data: String,
}

/// This daemon's turn-claim identity (host/pid + random suffix).
fn daemon_owner() -> String {
    static ONCE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    ONCE.get_or_init(|| format!("{}-{}-{}", hostname_simple(), std::process::id(), &uuid::Uuid::new_v4().to_string()[..8])).clone()
}

fn hostname_simple() -> String {
    std::fs::read_to_string("/etc/hostname").map(|s| s.trim().to_string()).unwrap_or_else(|_| "host".into())
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
    let _ = st.store.touch(&id);
    // Cross-daemon turn claim: 409 when another daemon holds the turn
    // (mirrors Go ClaimTurn: free, ours, or stale >10min).
    let owner = daemon_owner();
    if !st.store.claim_turn(&id, &owner, 600) {
        return (StatusCode::CONFLICT, Json(serde_json::json!({"error": "turn in flight (another daemon holds this session)"}))).into_response();
    }
    let mut runner = Runner::new();
    runner.approval = Some(st.gate.clone());
    // Workspace plugin worker (started on demand, cached per workspace).
    if let Ok(Some(sess)) = st.store.get_session(&id) {
        let host = st.plugins.host_for(std::path::Path::new(&sess.workspace_dir));
        if !host.is_empty() {
            runner.plugins = Some(host);
        }
    }
    // Validate images before touching locks (an oversize payload must not
    // leak the turn claim).
    let mut images = Vec::new();
    for im in &b.images {
        if im.data.len() > 8 * 1024 * 1024 {
            st.store.release_turn(&id, &owner);
            return (StatusCode::PAYLOAD_TOO_LARGE, Json(serde_json::json!({"error": "image over 8MB cap"}))).into_response();
        }
        images.push(crate::llm::ImageAttachment {
            name: im.name.clone(),
            media_type: im.media_type.clone(),
            data: im.data.clone(),
        });
    }
    // Same-daemon gate: chain behind the running turn instead of interleaving
    // (steer/queue semantics; cross-daemon races 409 above via the claim).
    if st.running.lock().unwrap().contains(&id) {
        st.queued
            .lock()
            .unwrap()
            .entry(id.clone())
            .or_default()
            .push_back(QueuedPrompt { text: b.text, effort, images });
        return (StatusCode::ACCEPTED, Json(serde_json::json!({"session": id, "queued": true}))).into_response();
    }
    st.running.lock().unwrap().insert(id.clone());
    // Drain the initial turn plus anything queued while it ran.
    let mut pending = std::collections::VecDeque::from([QueuedPrompt { text: b.text, effort, images }]);
    let first = drive_one(&st, &runner, &id, pending.pop_front().unwrap()).await;
    // Chain the rest in the background so this request returns with its own
    // turn; SSE carries every turn's deltas/completion.
    if st.queued.lock().unwrap().get(&id).is_some_and(|q| !q.is_empty()) {
        let st2 = st.clone();
        let id2 = id.clone();
        let runner2 = Runner::new();
        tokio::spawn(async move {
            let mut runner2 = runner2;
            runner2.approval = Some(st2.gate.clone());
            if let Ok(Some(sess)) = st2.store.get_session(&id2) {
                let host = st2.plugins.host_for(std::path::Path::new(&sess.workspace_dir));
                if !host.is_empty() {
                    runner2.plugins = Some(host);
                }
            }
            loop {
                let next = {
                    st2.queued.lock().unwrap().get_mut(&id2).and_then(|q| q.pop_front())
                };
                let Some(q) = next else { break };
                let _ = drive_one(&st2, &runner2, &id2, q).await;
            }
            st2.store.release_turn(&id2, &owner);
            st2.running.lock().unwrap().remove(&id2);
        });
        // Ownership transfers to the background drain.
        return match first {
            Ok(text) => Json(serde_json::json!({"session": id, "text": text})).into_response(),
            Err(s) => {
                if s.contains("not found") {
                    (StatusCode::NOT_FOUND, Json(serde_json::json!({"error": s}))).into_response()
                } else {
                    (StatusCode::BAD_GATEWAY, Json(serde_json::json!({"error": s}))).into_response()
                }
            }
        };
    }
    st.store.release_turn(&id, &owner);
    st.running.lock().unwrap().remove(&id);
    match first {
        Ok(text) => Json(serde_json::json!({"session": id, "text": text})).into_response(),
        Err(s) => {
            if s.contains("not found") {
                (StatusCode::NOT_FOUND, Json(serde_json::json!({"error": s}))).into_response()
            } else {
                (StatusCode::BAD_GATEWAY, Json(serde_json::json!({"error": s}))).into_response()
            }
        }
    }
}

/// Run one queued turn: events, context gauge, standing-ops. Returns text or error string.
async fn drive_one(st: &AppState, runner: &Runner, id: &str, q: QueuedPrompt) -> Result<String, String> {
    st.emit(serde_json::json!({"type": "turn.started", "session": id}));
    let tx = st.events.clone();
    let sid = id.to_string();
    let started = std::time::Instant::now();
    let out = runner
        .run_prompt(&st.store, &st.router, &st.cfg, id, &q.text, &q.effort, &q.images, |d, t| {
            let _ = tx.send(serde_json::json!({"type": "turn.delta", "session": sid, "delta": d, "thinking": t}));
        })
        .await;
    match out {
        Ok(text) => {
            let ms = started.elapsed().as_millis() as u64;
            st.emit(serde_json::json!({"type": "turn.completed", "session": id,
                "ms": ms, "chars": text.len()}));
            emit_context(st, id);
            standing_ops(st, id);
            Ok(text)
        }
        Err(e) => {
            let s = e.to_string();
            st.emit(serde_json::json!({"type": "turn.failed", "session": id, "error": s}));
            standing_ops(st, id);
            Err(s)
        }
    }
}

/// Emit one `turn.context` gauge event (used tokens vs advertised window).
fn emit_context(st: &AppState, id: &str) {
    let Ok(Some(sess)) = st.store.get_session(id) else { return };
    let Ok(turns) = st.store.turns(id) else { return };
    let used: usize = turns.iter().map(|t| t.text.len() + t.role.len()).sum::<usize>() / 4;
    // Window resolution is async; best-effort snapshot via blocking park.
    let router = st.router.clone();
    let model = if sess.model.is_empty() { router.model_for("executor", "build") } else { sess.model.clone() };
    let max = tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(router.context_window(&model))) as usize;
    let _ = st.events.send(serde_json::json!({"type": "turn.context", "session": id, "used": used, "max": max}));
}

/// Standing-ops: debounced KB freshness pass after every turn (success or
/// failure), mirroring session-close distillation hooks.
fn standing_ops(st: &AppState, id: &str) {
    if !st.cfg.kb.standing_ops {
        return;
    }
    let Ok(Some(sess)) = st.store.get_session(id) else { return };
    let mins = st.cfg.kb.idle_debounce_minutes.max(1);
    let ws = sess.workspace_dir.clone();
    let sid = id.to_string();
    let tx = st.events.clone();
    let _ = st.sweeper.run_throttled(
        &ws,
        std::time::Duration::from_secs(mins * 60),
        || {
            crate::kb::refresh_memory_manifest(std::path::Path::new(&ws))
                .map(|_| {
                    let _ = tx.send(serde_json::json!({"type": "kb.refreshed", "session": sid}));
                })
                .map_err(|e| e.to_string())
        },
    );
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

/// Single source of truth for the daemon schema (routes + CLI `openapi`).
pub fn openapi_spec() -> serde_json::Value {
    serde_json::json!({
        "openapi": "3.1.0",
        "info": {"title": "Lumina daemon", "version": "0.1.0"},
        "paths": {
            "/api/v1/events": "SSE: turn/context/delta/tool/completed/failed, permission.asked/resolved, question.asked/resolved, session.*, config.updated",
            "/api/v1/sessions": "list (?directory=, ?latest=1) / create",
            "/api/v1/sessions/{id}": "fetch / patch agent,permission,model,title",
            "/api/v1/sessions/{id}/state": "resync: session + running + pending permission/question",
            "/api/v1/sessions/{id}/prompt": "submit turn",
            "/api/v1/sessions/{id}/fork": "branch at turn",
            "/api/v1/sessions/{id}/export": "transcript (?sanitize=1)",
            "/api/v1/sessions/{id}/revert": "shadow-git undo (?turn=)",
            "/api/v1/sessions/{id}/unrevert": "shadow-git redo",
            "/api/v1/sessions/{id}/compact": "auto-compact session",
            "/api/v1/permissions/{reqId}": "allow_once/session/always|deny",
            "/api/v1/questions/{reqId}": "answer lists",
            "/api/v1/files": "workspace tree (?path=)",
            "/api/v1/git/diff": "working-tree stat",
            "/api/v1/git/branch": "current branch",
            "/api/v1/pty": "websocket shell",
            "/api/v1/providers": "configured providers (no secrets)",
            "/api/v1/router": "get/set fast|executor|planner|improver (persisted)",
            "/api/v1/models": "live models (?provider=)",
            "/api/v1/mcp/tools": "list tools (?server=)",
            "/api/v1/auth/device/start": "copilot device flow",
            "/api/v1/auth/device/poll": "copilot device poll",
            "/api/v1/kb/query": "graphify+memory query",
            "/api/v1/kb/": "browse memory/ + graphify-out/",
            "/api/v1/harness": "8 components",
            "/api/v1/harness/evolve": "Propose-Probe-Compose"
        }
    })
}

async fn openapi() -> impl IntoResponse {
    Json(openapi_spec())
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
    // Model-persist: hot-swap now, survive restarts via the config file.
    if let Some(p) = &st.cfg_path {
        if let Err(e) = crate::config::persist_router_model(p, &b.role, &b.model) {
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error": e.to_string()}))).into_response();
        }
    }
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
    if crate::config::is_google_url(&def.base_url) {
        return (StatusCode::FORBIDDEN, Json(serde_json::json!({"error": "Google endpoints blocked"}))).into_response();
    }
    match st.router.list_models(&q.provider).await {
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

async fn session_state(State(st): State<AppState>, Path(id): Path<String>) -> impl IntoResponse {
    let running = st.running.lock().unwrap().contains(&id);
    let (perm, ques) = st.gate.pending_for_session(&id);
    match st.store.get_session(&id) {
        Ok(Some(s)) => Json(serde_json::json!({
            "session": view(&s), "running": running,
            "permission": perm, "question": ques,
        }))
        .into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, Json(serde_json::json!({"error": "not found"}))).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error": e.to_string()}))).into_response(),
    }
}

#[derive(Deserialize)]
struct PermissionAnswer {
    #[serde(default)]
    decision: String,
}

async fn answer_permission(
    State(st): State<AppState>,
    Path(req_id): Path<String>,
    Json(b): Json<PermissionAnswer>,
) -> impl IntoResponse {
    let d = if b.decision.is_empty() { "deny" } else { &b.decision };
    if !["allow_once", "allow_session", "allow_always", "deny"].contains(&d) {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "decision must be allow_once|allow_session|allow_always|deny"}))).into_response();
    }
    if st.gate.resolve(&req_id, d) {
        Json(serde_json::json!({"ok": true})).into_response()
    } else {
        (StatusCode::NOT_FOUND, Json(serde_json::json!({"error": "unknown or expired request"}))).into_response()
    }
}

#[derive(Deserialize)]
struct QuestionAnswer {
    #[serde(default)]
    answers: Vec<String>,
}

async fn answer_question(
    State(st): State<AppState>,
    Path(req_id): Path<String>,
    Json(b): Json<QuestionAnswer>,
) -> impl IntoResponse {
    if st.gate.answer(&req_id, b.answers) {
        Json(serde_json::json!({"ok": true})).into_response()
    } else {
        (StatusCode::NOT_FOUND, Json(serde_json::json!({"error": "unknown or expired request"}))).into_response()
    }
}

#[derive(Deserialize)]
struct DeviceQ {
    #[serde(default)]
    provider: String,
    #[serde(default)]
    github_url: String,
}

/// Begin GitHub device flow (copilot only). The daemon never prints the
/// token — it lands in the 0600 token store on poll success.
async fn auth_device_start(Query(q): Query<DeviceQ>) -> impl IntoResponse {
    let provider = if q.provider.is_empty() { "copilot".to_string() } else { q.provider };
    if provider != "copilot" {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "only copilot supports device auth"}))).into_response();
    }
    let token_url = if q.github_url.is_empty() { "https://api.github.com".to_string() } else { q.github_url };
    match crate::copilot::start_device_flow(&token_url, "").await {
        Ok(dc) => Json(serde_json::json!({
            "provider": provider, "device_code": dc.device_code, "user_code": dc.user_code,
            "verification_uri": dc.verification_uri, "interval": dc.interval,
        }))
        .into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, Json(serde_json::json!({"error": e.to_string()}))).into_response(),
    }
}

#[derive(Deserialize)]
struct DevicePollQ {
    #[serde(default)]
    provider: String,
    #[serde(default)]
    github_url: String,
    #[serde(default)]
    device_code: String,
}

async fn auth_device_poll(Query(q): Query<DevicePollQ>) -> impl IntoResponse {
    let provider = if q.provider.is_empty() { "copilot".to_string() } else { q.provider };
    let token_url = if q.github_url.is_empty() { "https://api.github.com".to_string() } else { q.github_url };
    if q.device_code.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "device_code required"}))).into_response();
    }
    match crate::copilot::poll_device_flow(&token_url, &q.device_code).await {
        Ok(crate::copilot::Poll::Token(t)) => {
            if let Err(e) = crate::copilot::write_token_store(&provider, &t) {
                return (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error": e.to_string()}))).into_response();
            }
            Json(serde_json::json!({"provider": provider, "connected": true})).into_response()
        }
        Ok(crate::copilot::Poll::Pending) => Json(serde_json::json!({"pending": true})).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, Json(serde_json::json!({"error": e.to_string()}))).into_response(),
    }
}

async fn harness_info() -> impl IntoResponse {
    Json(serde_json::json!({"components": crate::harness::COMPONENTS}))
}

async fn harness_evolve(State(st): State<AppState>) -> impl IntoResponse {
    st.emit(serde_json::json!({"type": "harness.evolve.started"}));
    let ws = std::env::current_dir().unwrap_or_else(|_| ".".into());
    // Blocking git worktree ops: run off the async executor.
    let out = tokio::task::spawn_blocking(move || crate::harness::offline_pass(&ws)).await;
    match out {
        Ok(Ok(msg)) => {
            st.emit(serde_json::json!({"type": "harness.evolve.completed", "result": msg}));
            Json(serde_json::json!({"status": "completed", "result": msg})).into_response()
        }
        Ok(Err(e)) => {
            st.emit(serde_json::json!({"type": "harness.evolve.failed", "error": e.to_string()}));
            (StatusCode::BAD_GATEWAY, Json(serde_json::json!({"error": e.to_string()}))).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error": format!("evolve task: {e}")}))).into_response(),
    }
}

#[derive(Deserialize)]
struct RevertBody {
    #[serde(default)]
    turn: Option<String>,
}

fn workspace_of(st: &AppState, id: &str) -> Result<String, String> {
    st.store
        .get_session(id)
        .map_err(|e| e.to_string())?
        .map(|s| s.workspace_dir)
        .ok_or_else(|| "session not found".to_string())
}

/// Shadow-Git /undo: restore tracked files from the turn checkpoint.
async fn revert(
    State(st): State<AppState>,
    Path(id): Path<String>,
    body: Option<Json<RevertBody>>,
) -> impl IntoResponse {
    let ws = match workspace_of(&st, &id) {
        Ok(w) => w,
        Err(e) => return (StatusCode::NOT_FOUND, Json(serde_json::json!({"error": e}))).into_response(),
    };
    let ws = PathBuf::from(ws);
    // Exact turn match, else the latest checkpoint.
    let checkpoint = match body.and_then(|b| b.0.turn) {
        Some(t) => find_checkpoint(&st, &id, &t).or_else(|| latest_checkpoint(&st, &id)),
        None => latest_checkpoint(&st, &id),
    };
    let Some((turn, commit)) = checkpoint else {
        return (StatusCode::NOT_FOUND, Json(serde_json::json!({"error": "no checkpoint for this session yet (mutating turns create them)"}))).into_response();
    };
    // Remember where we were so /redo can come back.
    let head = std::process::Command::new("git").arg("-C").arg(&ws).arg("rev-parse").arg("HEAD").output();
    if let Ok(o) = head {
        if o.status.success() {
            let _ = st.store.add_trace(&id, "revert_from", &serde_json::json!({"commit": String::from_utf8_lossy(&o.stdout).trim()}).to_string());
        }
    }
    match crate::checkpoints::restore(&ws, &commit) {
        Ok(_) => {
            st.emit(serde_json::json!({"type": "session.reverted", "session": id, "turn": turn}));
            Json(serde_json::json!({"session": id, "turn": turn, "commit": commit})).into_response()
        }
        Err(e) => (StatusCode::BAD_GATEWAY, Json(serde_json::json!({"error": e.to_string()}))).into_response(),
    }
}

fn parse_checkpoint(payload: &str) -> Option<(String, String)> {
    let v: serde_json::Value = serde_json::from_str(payload).ok()?;
    Some((v.get("turn")?.as_str()?.to_string(), v.get("commit")?.as_str()?.to_string()))
}

fn latest_checkpoint(st: &AppState, id: &str) -> Option<(String, String)> {
    let all = st.store.traces(id, "checkpoint").ok()?;
    all.into_iter().filter_map(|p| parse_checkpoint(&p)).next_back()
}

fn find_checkpoint(st: &AppState, id: &str, turn: &str) -> Option<(String, String)> {
    let all = st.store.traces(id, "checkpoint").ok()?;
    all.into_iter().filter_map(|p| parse_checkpoint(&p)).find(|(t, _)| t == turn)
}

/// Shadow-Git /redo: return to the pre-undo HEAD.
async fn unrevert(State(st): State<AppState>, Path(id): Path<String>) -> impl IntoResponse {
    let ws = match workspace_of(&st, &id) {
        Ok(w) => w,
        Err(e) => return (StatusCode::NOT_FOUND, Json(serde_json::json!({"error": e}))).into_response(),
    };
    let Some(payload) = st.store.latest_trace(&id, "revert_from").ok().flatten() else {
        return (StatusCode::NOT_FOUND, Json(serde_json::json!({"error": "nothing to redo"}))).into_response();
    };
    let commit = serde_json::from_str::<serde_json::Value>(&payload)
        .ok()
        .and_then(|v| v.get("commit").and_then(|c| c.as_str()).map(|s| s.to_string()))
        .unwrap_or_default();
    if commit.is_empty() {
        return (StatusCode::NOT_FOUND, Json(serde_json::json!({"error": "nothing to redo"}))).into_response();
    }
    match crate::checkpoints::restore(&PathBuf::from(ws), &commit) {
        Ok(_) => {
            st.emit(serde_json::json!({"type": "session.unreverted", "session": id}));
            Json(serde_json::json!({"session": id, "commit": commit})).into_response()
        }
        Err(e) => (StatusCode::BAD_GATEWAY, Json(serde_json::json!({"error": e.to_string()}))).into_response(),
    }
}

async fn compact(State(st): State<AppState>, Path(id): Path<String>) -> impl IntoResponse {
    match Runner::compact_session(&st.store, &st.router, &st.cfg, &id).await {
        Ok(msg) => {
            st.emit(serde_json::json!({"type": "session.compacted", "session": id, "result": msg}));
            Json(serde_json::json!({"session": id, "result": msg})).into_response()
        }
        Err(e) => (StatusCode::BAD_GATEWAY, Json(serde_json::json!({"error": e.to_string()}))).into_response(),
    }
}

/// List tools from a configured MCP server (powers TUI/Web tool pickers).
async fn mcp_tools(Query(q): Query<HashMap<String, String>>) -> impl IntoResponse {
    let Some(server) = q.get("server") else {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "server required"}))).into_response();
    };
    let ws = q.get("directory").map(PathBuf::from).unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| ".".into()));
    let cfg = crate::mcp::ConfigFile::load(&ws);
    let Some(conf) = cfg.servers.get(server) else {
        return (StatusCode::NOT_FOUND, Json(serde_json::json!({"error": "unknown MCP server"}))).into_response();
    };
    if crate::config::is_google_url(&conf.url) {
        return (StatusCode::FORBIDDEN, Json(serde_json::json!({"error": "Google endpoints blocked"}))).into_response();
    };
    match crate::mcp::Client::dial(conf.clone()).and_then(|mut c| c.list_tools().map_err(|e| crate::mcp::McpError::Msg(e.to_string()))) {
        Ok(tools) => Json(serde_json::json!({"server": server, "tools": tools})).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, Json(serde_json::json!({"error": e.to_string()}))).into_response(),
    }
}

/// Custom slash commands (prompt templates with $ARGUMENTS expansion).
async fn commands(Query(q): Query<HashMap<String, String>>) -> impl IntoResponse {
    let ws = q.get("directory").map(PathBuf::from).unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| ".".into()));
    let bundle = crate::rules::Bundle::load(&ws, true, true);
    Json(serde_json::json!({"commands": bundle.commands.iter().map(|c| serde_json::json!({"name": c.name, "body": c.body})).collect::<Vec<_>>()}))
    .into_response()
}

/// Custom agents (including `mode: primary` Tab-cycle targets).
async fn agents(Query(q): Query<HashMap<String, String>>) -> impl IntoResponse {
    let ws = q.get("directory").map(PathBuf::from).unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| ".".into()));
    let bundle = crate::rules::Bundle::load(&ws, true, true);
    Json(serde_json::json!({"agents": bundle.agents.iter().map(|a| serde_json::json!({"name": a.name, "mode": a.mode, "description": a.description})).collect::<Vec<_>>()}))
    .into_response()
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
