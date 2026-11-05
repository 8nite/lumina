//! Daemon HTTP layer: axum router + SSE + embedded SPA.
//! Route surface (M1: sessions, prompt,
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
    /// Monotonic SSE sequence number: clients detect dropped (lagged)
    /// broadcast events by gaps and refetch /api/state to reconcile.
    pub seq: Arc<std::sync::atomic::AtomicU64>,
    /// Shared language-server manager: servers persist for daemon lifetime,
    /// cloned into each per-turn Runner (never rebuilt per turn).
    pub lsp: Arc<crate::lsp::Manager>,
}

impl AppState {
    pub fn new(store: Store, router: LlmRouter, cfg: Config, cfg_path: Option<PathBuf>) -> Self {
        let (events, _) = broadcast::channel(1024);
        // 1024 (was 256): long turns burst hundreds of events (each tool
        // emits start+done plus deltas; the stuck session ran 128 reads).
        // A full buffer drops the oldest — including turn.completed — and
        // the web client spins forever. Terminal events must not shed.
        // Background task completions surface as turn.progress stages
        // (Go OnBackgroundDone parity).
        let tx = events.clone();
        let seq = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let tx_seq = seq.clone();
        crate::tools::set_background_hook(std::sync::Arc::new(move |session: &str, id: &str, kind: &str, _label: &str| {
            let mut v = serde_json::json!({
                "type": "turn.progress",
                "session": session,
                "stage": format!("background done: {id} ({kind})"),
            });
            if let Some(o) = v.as_object_mut() {
                o.insert("seq".into(), tx_seq.fetch_add(1, std::sync::atomic::Ordering::SeqCst).into());
            }
            let _ = tx.send(v);
        }));
        let lsp = Arc::new(crate::lsp::Manager::new(cfg.lsp.clone()));
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
            seq,
            lsp,
        }
    }

    fn emit(&self, mut v: serde_json::Value) {
        if let Some(o) = v.as_object_mut() {
            o.insert("seq".into(), self.seq.fetch_add(1, std::sync::atomic::Ordering::SeqCst).into());
        }
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
    // Cooperative stop must land mid-tool: foreground bash polls this and
    // kills the child instead of running the command out (daemon-only;
    // headless `run` leaves it unset and keeps prior behavior).
    let stop_store = state.store.clone();
    crate::tools::set_stop_hook(std::sync::Arc::new(move |sess: &str| stop_store.stop_requested(sess)));
    Router::new()
        .route("/openapi.json", get(openapi))
        .route("/api/v1/events", get(events))
        .route("/api/v1/sessions", get(list_sessions).post(create_session))
        .route("/api/v1/sessions/:id", get(get_session).patch(patch_session).delete(delete_session))
        .route("/api/v1/sessions/:id/prompt", axum::routing::post(prompt))
        .route("/api/v1/sessions/:id/stop", axum::routing::post(stop_turn))
        .route("/api/v1/sessions/:id/queue/cancel", axum::routing::post(queue_cancel))
        .route("/api/v1/sessions/:id/queue/steer", axum::routing::post(queue_steer))
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
    tokens_input: i64,
    tokens_output: i64,
    tokens_cache_read: i64,
    tokens_cache_write: i64,
    /// Live turn state (same-daemon memory, not stored): drives the
    /// sidebar working dot so background turns are visible from any view.
    #[serde(default)]
    running: bool,
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
        tokens_input: s.tokens_input,
        tokens_output: s.tokens_output,
        tokens_cache_read: s.tokens_cache_read,
        tokens_cache_write: s.tokens_cache_write,
        running: false,
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
        Ok(v) => {
            // Overlay the live turn set so the sidebar can show which
            // sessions are working without polling per-session state.
            let running = st.running.lock().unwrap();
            Json(
                v.iter()
                    .map(|s| {
                        let mut sv = view(s);
                        sv.running = running.contains(&s.id);
                        sv
                    })
                    .collect::<Vec<_>>(),
            )
            .into_response()
        }
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
        Ok(s) => {
            st.emit(ev("session.created", &s.id, serde_json::json!(view(&s))));
            (StatusCode::CREATED, Json(serde_json::json!(view(&s)))).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error": e.to_string()}))).into_response(),
    }
}

async fn get_session(State(st): State<AppState>, Path(id): Path<String>) -> impl IntoResponse {
    let store = &st.store;
    match store.get_session(&id) {
        Ok(Some(s)) => {
            let mut sv = view(&s);
            sv.running = st.running.lock().unwrap().contains(&s.id);
            Json(serde_json::json!(sv)).into_response()
        }
        Ok(None) => (StatusCode::NOT_FOUND, Json(serde_json::json!({"error": "not found"}))).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error": e.to_string()}))).into_response(),
    }
}

async fn delete_session(State(st): State<AppState>, Path(id): Path<String>) -> impl IntoResponse {
    let store = &st.store;
    match store.delete_session(&id) {
        Ok(true) => {
            // Drop the per-session TMPDIR (Go CleanupSessionDir parity).
            let _ = std::fs::remove_dir_all(std::env::temp_dir().join(format!("lumina-ses-{id}")));
            Json(serde_json::json!({"deleted": true})).into_response()
        }
        Ok(false) => (StatusCode::NOT_FOUND, Json(serde_json::json!({"error": "not found"}))).into_response(),
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
        Ok(Some(s)) => {
            st.emit(ev("session.updated", &s.id, serde_json::json!(view(&s))));
            Json(serde_json::json!(view(&s))).into_response()
        }
        Ok(None) => (StatusCode::NOT_FOUND, Json(serde_json::json!({"error": "not found"}))).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error": e.to_string()}))).into_response(),
    }
}

/// SSE envelope: Go `Event{Type, SessionID, Data}` parity. The web UI and
/// TUI classify on `type` + `session_id`, reading payloads from `data`.
pub fn ev(kind: &str, session: &str, data: serde_json::Value) -> serde_json::Value {
    if session.is_empty() {
        serde_json::json!({"type": kind, "data": data})
    } else {
        serde_json::json!({"type": kind, "session_id": session, "data": data})
    }
}

/// A prompt chained behind a running turn (server-side queue).
#[derive(Clone, Serialize)]
pub(crate) struct QueuedPrompt {
    text: String,
    effort: String,
    images: Vec<crate::llm::ImageAttachment>,
}

/// Retry idempotency window: same span as the cross-daemon turn claim.
const IDEM_TTL_SECS: u64 = 600;

#[derive(Deserialize)]
struct PromptBody {
    #[serde(default)]
    text: String,
    #[serde(default)]
    effort: String,
    /// Delivery while a turn is running: steer (default) joins the active
    /// turn at the next round boundary; queue chains after it completes.
    /// Ignored when the session is idle (Go parity).
    #[serde(default)]
    delivery: String,
    /// Client-generated idempotency key (one per submit; retries reuse it).
    /// A resubmit with a seen key returns the live status without
    /// steering/queueing the text again.
    #[serde(default)]
    request_id: String,
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

/// Panic-proof turn release: the prompt drain task must never leak the
/// cross-daemon claim or the same-daemon `running` entry. A leaked pair
/// steers every later prompt into a dead drain — a permanently wedged
/// session that no browser refresh can heal (backend owns the work, so the
/// backend must guarantee release). Drop runs on normal completion and on
/// task-panic unwind alike.
struct TurnRelease {
    store: Arc<Store>,
    running: Arc<Mutex<HashSet<String>>>,
    id: String,
    owner: String,
}
impl Drop for TurnRelease {
    fn drop(&mut self) {
        self.store.release_turn(&self.id, &self.owner);
        if let Ok(mut r) = self.running.lock() {
            r.remove(&self.id);
        }
    }
}

async fn prompt(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Json(b): Json<PromptBody>,
) -> impl IntoResponse {
    if b.text.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "empty prompt"}))).into_response();
    }
    if !b.effort.is_empty() && !crate::llm::valid_effort(&b.effort) {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "invalid effort (want auto|none|minimal|low|medium|high|xhigh|max)"}))).into_response();
    }
    let effort = crate::llm::normalize_effort(&b.effort).to_string();
    let delivery = if b.delivery.trim().is_empty() { "steer".to_string() } else { b.delivery.trim().to_string() };
    // Retry idempotency: a resubmit with a seen request_id (client retry
    // after a lost response) returns the live status without steering or
    // queueing the text again. Checked before the claim so a duplicate
    // never refreshes it either.
    let request_id = b.request_id.trim().to_string();
    if !request_id.is_empty() {
        // Fail-open: a DB error must not block the prompt.
        let dup = st.store.seen_request(&id, &request_id, IDEM_TTL_SECS as i64).unwrap_or(false);
        if dup {
            let running = st.running.lock().unwrap().contains(&id);
            let status = if running { "steered" } else { "accepted" };
            return (
                StatusCode::ACCEPTED,
                Json(serde_json::json!({"status": status, "session_id": id, "deduped": true})),
            )
                .into_response();
        }
    }
    let _ = st.store.touch(&id);
    // Cross-daemon turn claim: 409 when another daemon holds the turn
    // (mirrors Go ClaimTurn: free, ours, or stale >10min).
    let owner = daemon_owner();
    if !st.store.claim_turn(&id, &owner, 600) {
        let detail = st
            .store
            .turn_owner(&id)
            .map(|(o, since)| {
                let ts = chrono::DateTime::from_timestamp(since, 0)
                    .map(|t| t.to_rfc3339())
                    .unwrap_or_default();
                format!("turn in flight on another daemon (owner {o} since {ts})")
            })
            .unwrap_or_else(|| "turn in flight (another daemon holds this session)".to_string());
        return (StatusCode::CONFLICT, Json(serde_json::json!({"error": detail}))).into_response();
    }
    let mut runner = Runner::new();
    runner.approval = Some(st.gate.clone());
    runner.lsp = st.lsp.clone();
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
    // Same-daemon gate: while a turn is in flight, steer into it or chain
    // behind it instead of starting a second concurrent loop (Go parity).
    if st.running.lock().unwrap().contains(&id) {
        if delivery == "queue" {
            let pos = {
                let mut q = st.queued.lock().unwrap();
                let queue = q.entry(id.clone()).or_default();
                queue.push_back(QueuedPrompt { text: b.text, effort, images });
                queue.len()
            };
            st.emit(ev("turn.progress", &id, serde_json::json!({"stage": format!("queued #{pos}")})));
            return (StatusCode::ACCEPTED, Json(serde_json::json!({"status": "queued", "session_id": id, "position": pos}))).into_response();
        }
        st.store.steer(&id, &b.text);
        let preview: String = b.text.chars().take(80).collect();
        let preview = if b.text.chars().count() > 80 { format!("{preview}…") } else { preview };
        st.emit(ev("turn.progress", &id, serde_json::json!({"stage": format!("steered: {preview}")})));
        return (StatusCode::ACCEPTED, Json(serde_json::json!({"status": "steered", "session_id": id}))).into_response();
    }
    st.running.lock().unwrap().insert(id.clone());
    let first_q = QueuedPrompt { text: b.text, effort, images };
    // Drain the initial turn plus anything queued while it ran in a
    // detached task: a client disconnect must not cancel the in-flight
    // turn (which would leak `running`/claim and orphan the queue,
    // stranding queued turns with no assistant replies). The first turn's
    // result returns over a oneshot so the blocking prompt contract
    // (200 + text) is preserved while the task outlives the request.
    let (first_tx, first_rx) = tokio::sync::oneshot::channel();
    let st2 = st.clone();
    let id2 = id.clone();
    tokio::spawn(async move {
        // Guaranteed release (TurnRelease): a panic anywhere below still
        // frees the claim and the running entry instead of wedging the
        // session with a dead drain.
        let _release = TurnRelease { store: st2.store.clone(), running: st2.running.clone(), id: id2.clone(), owner };
        let first = drive_one(&st2, &runner, &id2, &_release.owner, first_q).await;
        // The request may be gone (disconnect); the drain continues anyway.
        let stopped = matches!(&first, Ok(d) if d.stopped);
        let _ = first_tx.send(first);
        if stopped {
            // Stop honored: the turn ended by request and the pending
            // queue/steer inbox was dropped with it — no chained drain.
            return;
        }
        let mut runner2 = Runner::new();
        runner2.approval = Some(st2.gate.clone());
        runner2.lsp = st2.lsp.clone();
        if let Ok(Some(sess)) = st2.store.get_session(&id2) {
            let host = st2.plugins.host_for(std::path::Path::new(&sess.workspace_dir));
            if !host.is_empty() {
                runner2.plugins = Some(host);
            }
        }
        loop {
            // Steer text that arrived after the last round boundary chains
            // as a queued turn (Go leftover-steer parity).
            let rest = st2.store.take_steered(&id2);
            if !rest.trim().is_empty() {
                let _ = drive_one(&st2, &runner2, &id2, &_release.owner, QueuedPrompt {
                    text: rest,
                    effort: String::new(),
                    images: Vec::new(),
                })
                .await;
            }
            let next = {
                st2.queued.lock().unwrap().get_mut(&id2).and_then(|q| q.pop_front())
            };
            let Some(q) = next else { break };
            let _ = drive_one(&st2, &runner2, &id2, &_release.owner, q).await;
        }
        // TurnRelease drops here: claim + running entry freed on every path.
    });
    // Cap the blocking wait below typical proxy/gateway timeouts (60s):
    // slow turns return 202 and finish over SSE instead of surfacing as a
    // 504 that tempts the client into a duplicate submit. The drain task is
    // detached and continues either way.
    match tokio::time::timeout(std::time::Duration::from_secs(50), first_rx).await {
        Ok(Ok(Ok(done))) => Json(serde_json::json!({"session": id, "text": done.text})).into_response(),
        Ok(Ok(Err(s))) => {
            if s.contains("not found") {
                (StatusCode::NOT_FOUND, Json(serde_json::json!({"error": s}))).into_response()
            } else {
                (StatusCode::BAD_GATEWAY, Json(serde_json::json!({"error": s}))).into_response()
            }
        }
        // Slow turn (still running; result arrives over SSE) or the drain
        // task panicked before reporting (the queue still drains).
        Ok(Err(_)) | Err(_) => (StatusCode::ACCEPTED, Json(serde_json::json!({"session": id, "status": "accepted"}))).into_response(),
    }
}

/// Cooperative stop: flag the running turn to end at the next round
/// boundary (or mid-stream poll) and drop everything chained behind it
/// (pending steer inbox + queued turns) so the session idles out. The
/// active turn emits turn.completed ("Stopped by user.") when it lands.
/// Idle sessions are a noop — the flag is only ever set while running,
/// so a stale flag can never kill a later turn.
async fn stop_turn(State(st): State<AppState>, Path(id): Path<String>) -> impl IntoResponse {
    if !st.running.lock().unwrap().contains(&id) {
        return (StatusCode::ACCEPTED, Json(serde_json::json!({"status": "idle", "session_id": id}))).into_response();
    }
    st.store.request_stop(&id);
    // Halt the whole chain: anything chained behind the active turn must
    // not start once it lands.
    let _ = st.store.take_steered(&id);
    st.queued.lock().unwrap().remove(&id);
    st.emit(ev("turn.progress", &id, serde_json::json!({"stage": "stopping…"})));
    (StatusCode::ACCEPTED, Json(serde_json::json!({"status": "stopping", "session_id": id}))).into_response()
}

#[derive(Deserialize)]
struct QueueIndexBody {
    index: usize,
}

/// Cancel one queued prompt: drop it from the chain and hand its payload
/// back so the client can restore it into the composer.
async fn queue_cancel(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Json(b): Json<QueueIndexBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let removed = st
        .queued
        .lock()
        .unwrap()
        .get_mut(&id)
        .and_then(|q| q.remove(b.index));
    match removed {
        Some(qp) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "status": "canceled", "session_id": id,
                "text": qp.text, "images": qp.images,
            })),
        ),
        None => (StatusCode::NOT_FOUND, Json(serde_json::json!({"error": "queued prompt not found"}))),
    }
}

/// Steer one queued prompt into the running turn now: drop it from the
/// chain and buffer its text via the steer inbox (answered at the next
/// round boundary). Text-only like all steer traffic: an item carrying
/// images is refused in place — cancel restores those into the composer
/// intact. When the session is idle the text waits in the inbox for the
/// next turn.
async fn queue_steer(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Json(b): Json<QueueIndexBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    enum Pop {
        /// Refused (carries images): already restored to its exact slot.
        Refused,
        Ready(String),
    }
    // Single lock for pop + check + maybe-restore: a refused item goes
    // back to its exact slot instead of being lost or reordered.
    let popped = {
        let mut map = st.queued.lock().unwrap();
        let q = match map.get_mut(&id) {
            Some(q) => q,
            None => return (StatusCode::NOT_FOUND, Json(serde_json::json!({"error": "queued prompt not found"}))),
        };
        let Some(qp) = q.remove(b.index) else {
            return (StatusCode::NOT_FOUND, Json(serde_json::json!({"error": "queued prompt not found"})));
        };
        if qp.images.is_empty() {
            Pop::Ready(qp.text)
        } else {
            q.insert(b.index.min(q.len()), qp);
            Pop::Refused
        }
    };
    match popped {
        Pop::Refused => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "queued prompt has attachments; cancel it to edit and resend"})),
        ),
        Pop::Ready(text) => {
            st.store.steer(&id, &text);
            let preview: String = text.chars().take(80).collect();
            let preview = if text.chars().count() > 80 { format!("{preview}…") } else { preview };
            st.emit(ev("turn.progress", &id, serde_json::json!({"stage": format!("steered: {preview}")})));
            (StatusCode::ACCEPTED, Json(serde_json::json!({"status": "steered", "session_id": id, "text": text})))
        }
    }
}

/// Run one queued turn: events, context gauge, standing-ops. Returns text or error string.
#[derive(Debug)]
struct DriveDone {
    text: String,
    stopped: bool,
}

async fn drive_one(st: &AppState, runner: &Runner, id: &str, owner: &str, q: QueuedPrompt) -> Result<DriveDone, String> {
    st.emit(ev("turn.started", id, serde_json::json!({"text": q.text})));
    let tx = st.events.clone();
    let sid = id.to_string();
    let started = std::time::Instant::now();
    let tx2 = tx.clone();
    let sid2 = sid.clone();
    let tx3 = tx.clone();
    let sid3 = sid.clone();
    let mut on_event = |d: &str, t: &str| {
        let _ = tx.send(ev("turn.delta", &sid, serde_json::json!({"delta": d, "thinking": t, "elapsed_ms": started.elapsed().as_millis() as u64})));
    };
    let mut on_stage = |stage: &str| {
        let _ = tx2.send(ev("turn.progress", &sid2, serde_json::json!({"stage": stage, "elapsed_ms": started.elapsed().as_millis() as u64})));
    };
    let mut on_tool = |tool: crate::agent::ToolEvent| match tool {
        crate::agent::ToolEvent::Start { name, target } => {
            let _ = tx3.send(ev("turn.tool", &sid3, serde_json::json!({"phase": "start", "tool": name, "target": target, "elapsed_ms": started.elapsed().as_millis() as u64})));
        }
        crate::agent::ToolEvent::Done { name, target, preview, error, duration_ms } => {
            let _ = tx3.send(ev("turn.tool", &sid3, serde_json::json!({"phase": "done", "tool": name, "target": target, "preview": preview, "error": error, "duration_ms": duration_ms, "elapsed_ms": started.elapsed().as_millis() as u64})));
        }
    };
    // Claim heartbeat: refresh our ownership while the turn runs so a
    // second daemon can't steal the slot mid-turn (600s steal window vs
    // turns that run far longer). Stops refreshing once the claim is lost.
    let hb_store = st.store.clone();
    let hb_id = id.to_string();
    let hb_owner = owner.to_string();
    let hb = tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            if !hb_store.claim_turn(&hb_id, &hb_owner, 600) {
                break;
            }
        }
    });
    // Backend owns the work: a transient LLM/gateway failure must not
    // silently kill the turn (stuck session d3539f35 ended mid-work with
    // no transcript trace after the no-progress gate pushed it onward).
    // One retry after a short cool-down, then a persisted failure note so
    // `continue` resumes with context instead of a dead transcript.
    // One persisted user turn per queued prompt: the retry below reuses it
    // instead of storing a duplicate and replaying executed tools.
    let user_turn = runner
        .begin_turn(&st.store, id, &q.text)
        .map_err(|e| e.to_string())?;
    let mut out = runner
        .run_prompt(&st.store, &st.router, &st.cfg, id, &user_turn, &q.effort, &q.images, &mut on_event, &mut on_stage, &mut on_tool)
        .await;
    // No retry on a cooperative stop: the user asked to end the turn.
    let stopped = matches!(out, Err(crate::agent::AgentError::Stopped));
    if !stopped && out.is_err() {
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        out = runner
            .run_prompt(&st.store, &st.router, &st.cfg, id, &user_turn, &q.effort, &q.images, &mut on_event, &mut on_stage, &mut on_tool)
            .await;
    }
    // Auto-continue until green (Always mode): an unfinished outcome
    // (unverified edits, zero-edit work request, budget ceiling) loops
    // back in with a synthetic continue turn instead of asking the user
    // to type `continue`. Bounded by config; every break below is
    // load-bearing (infinite-loop / cost-blowout guards).
    // Heartbeat stays alive across auto turns (they can run minutes).
    let mut auto_usage = crate::llm::Usage::default();
    let mut auto_n = 0usize;
    let mut prev_text = String::new();
    loop {
        if !matches!(&out, Ok(o) if o.needs_continue) {
            break;
        }
        let cap = st.cfg.agent.max_auto_continues as usize;
        if !st.cfg.agent.auto_continue || auto_n >= cap {
            break;
        }
        // Cooperative stop lands as Stopped so the normal stop path
        // below (note + flag clear) handles it.
        if st.store.stop_requested(id) {
            out = Err(crate::agent::AgentError::Stopped);
            break;
        }
        let (cur_text, reason) = match &out {
            Ok(o) => (o.text.clone(), o.continue_reason.clone()),
            Err(_) => break,
        };
        // Identical consecutive tails = the model is echoing, not
        // working. Stop instead of burning turns.
        if !prev_text.is_empty() && cur_text == prev_text {
            on_stage("auto-continue stopped: identical result twice");
            break;
        }
        prev_text = cur_text;
        // Context guard: stop before the window fills (85%+).
        let (_, used, max) = context_snapshot(st, id).await;
        if context_full(used, max) {
            on_stage("auto-continue stopped: context 85%+ full");
            break;
        }
        auto_n += 1;
        on_stage(&format!("auto-continuing {auto_n}/{cap}…"));
        let next_turn = match runner.begin_turn(&st.store, id, &reason.auto_prompt(auto_n, cap)) {
            Ok(t) => t,
            Err(_) => break,
        };
        // No retry on auto turns: a gateway failure ends the turn
        // through the normal failure path below (the transcript keeps
        // everything completed so far).
        out = runner
            .run_prompt(&st.store, &st.router, &st.cfg, id, &next_turn, &q.effort, &q.images, &mut on_event, &mut on_stage, &mut on_tool)
            .await;
        if let Ok(o) = &out {
            auto_usage.add(&o.usage);
        }
    }
    if let Ok(o) = &mut out {
        o.usage.add(&auto_usage);
    }
    hb.abort();
    if matches!(out, Err(crate::agent::AgentError::Stopped)) {
        // Stop honored: the loop already persisted the note. Complete
        // (not fail) so clients reconcile via the normal completed path,
        // and clear the flag so it can't leak into a later turn.
        // No context snapshot here: resolving max_tokens would cost a
        // gateway round trip on a path whose whole point is to stop.
        st.store.clear_stop(id);
        let ms = started.elapsed().as_millis() as u64;
        st.emit(ev("turn.completed", id, serde_json::json!({
            "text": crate::agent::STOPPED_TEXT, "model": "", "duration_ms": ms,
            "input_tokens": 0, "output_tokens": 0,
            "cache_read_tokens": 0, "cache_write_tokens": 0,
            "context_tokens": 0, "max_tokens": 0,
        })));
        standing_ops(st, id);
        return Ok(DriveDone { text: crate::agent::STOPPED_TEXT.to_string(), stopped: true });
    }
    match out {
        Ok(outcome) => {
            let ms = started.elapsed().as_millis() as u64;
            let (model, used, max) = context_snapshot(st, id).await;
            // Per-turn cache telemetry: the log is the only place that shows
            // hit-rate drift (compacts/subagent bursts/model swaps reset the
            // prefix). Zero behavior change; silent unless RUST_LOG=info.
            tracing::info!(session = %id, model = %model, input = outcome.usage.input,
                output = outcome.usage.output, cache_read = outcome.usage.cache_read,
                hit_pct = outcome.usage.cache_hit_pct(), "turn completed");
            st.emit(ev("turn.completed", id, serde_json::json!({
                "text": outcome.text, "model": model, "duration_ms": ms,
                "input_tokens": outcome.usage.input, "output_tokens": outcome.usage.output,
                "cache_read_tokens": outcome.usage.cache_read, "cache_write_tokens": outcome.usage.cache_write,
                "context_tokens": used, "max_tokens": max,
            })));
            st.emit(ev("turn.context", id, serde_json::json!({"context_tokens": used, "max_tokens": max})));
            standing_ops(st, id);
            Ok(DriveDone { text: outcome.text, stopped: false })
        }
        Err(e) => {
            let s = e.to_string();
            // Persist the failure where the transcript lives: SSE-only
            // errors leave a dead transcript that reads as "agent gave up".
            tracing::warn!(session = %id, error = %s, "turn failed after retry");
            let _ = st.store.add_turn(id, "assistant", &format!("Turn failed ({s}). Say `continue` to resume."));
            st.emit(ev("turn.failed", id, serde_json::json!({"error": s})));
            standing_ops(st, id);
            Err(s)
        }
    }
}

/// Pure context-full check for the auto-continue guard (85%+ of window).
fn context_full(used: usize, max: usize) -> bool {
    max > 0 && used.saturating_mul(100) >= max.saturating_mul(85)
}

/// Shared model + usage snapshot for the completed payload + gauge event.
async fn context_snapshot(st: &AppState, id: &str) -> (String, usize, usize) {    let Ok(Some(sess)) = st.store.get_session(id) else { return (String::new(), 0, 0) };
    let model = if sess.model.is_empty() { st.router.model_for("executor", "build") } else { sess.model.clone() };
    let used = st
        .store
        .turns(id)
        .map(|turns| turns.iter().map(|t| t.text.len() + t.role.len()).sum::<usize>() / 4)
        .unwrap_or(0);
    let max = st.router.context_window(&model).await as usize;
    (model, used, max)
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
                    let _ = tx.send(ev("kb.refreshed", &sid, serde_json::json!({})));
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
        Ok(s) => {
            st.emit(ev("session.forked", &s.id, serde_json::json!(view(&s))));
            (StatusCode::CREATED, Json(serde_json::json!(view(&s)))).into_response()
        }
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
    // Go-daemon-compatible transcript JSON for the web UI
    // (`?format=json`); markdown stays the default for the CLI + e2e.
    if q.get("format").map(|v| v == "json").unwrap_or(false) {
        return match store.transcript_json(&id, sanitize) {
            Ok(v) => Json(v).into_response(),
            Err(e) => (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": e.to_string()})),
            )
                .into_response(),
        };
    }
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
            "/api/v1/events": "SSE {type,session_id?,data}: turn.started/progress/context/delta/tool/completed/failed, permission.asked/resolved, question.asked/resolved, session.*, config.updated",
            "/api/v1/sessions": "list (?directory=, ?latest=1) / create",
            "/api/v1/sessions/{id}": "fetch / patch agent,permission,model,title",
            "/api/v1/sessions/{id}/state": "resync: session + running + pending permission/question",
            "/api/v1/sessions/{id}/prompt": "submit turn (request_id dedups client retries)",
            "/api/v1/sessions/{id}/stop": "cooperative stop: end running turn + drop queued chain",
            "/api/v1/sessions/{id}/queue/cancel": "drop queued prompt, payload returns for composer restore",
            "/api/v1/sessions/{id}/queue/steer": "steer queued prompt text into the running turn",
            "/api/v1/sessions/{id}/fork": "branch at turn",
            "/api/v1/sessions/{id}/export": "transcript (?sanitize=1)",
            "/api/v1/sessions/{id}/revert": "shadow-git undo (?turn=)",
            "/api/v1/sessions/{id}/unrevert": "shadow-git redo",
            "/api/v1/sessions/{id}/compact": "auto-compact session",
            "/api/v1/permissions/{reqId}": "allow_once/session/always|deny",
            "/api/v1/questions/{reqId}": "answer lists or {\"dismissed\": true}",
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
    let fast = r.model_for("fast", "build");
    let executor = r.model_for("executor", "build");
    let planner = r.model_for("planner", "plan");
    let improver = r.model_for("improver", "build");
    // Flat keys (Rust SDK / e2e grep) + nested `router` (Go daemon / web
    // client parity). Both shapes carry the same values.
    Json(serde_json::json!({
        "fast": fast,
        "executor": executor,
        "planner": planner,
        "improver": improver,
        "router": {
            "fast": fast,
            "executor": executor,
            "planner": planner,
            "improver": improver,
        },
    }))
    .into_response()
}

async fn router_set(State(st): State<AppState>, body: axum::body::Bytes) -> impl IntoResponse {
    // Accept both shapes: Rust SDK `{"role","model"}` and web/Go SDK
    // `{[role]: model}` (possibly multi-role). Reject only when nothing
    // recognizable is present.
    let v: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": format!("bad json: {e}")}))).into_response();
        }
    };
    let mut pairs: Vec<(String, String)> = Vec::new();
    if let (Some(role), Some(model)) = (
        v.get("role").and_then(|x| x.as_str()),
        v.get("model").and_then(|x| x.as_str()),
    ) {
        pairs.push((role.to_string(), model.to_string()));
    }
    if let Some(obj) = v.as_object() {
        for role in ["fast", "executor", "planner", "improver"] {
            if let Some(m) = obj.get(role).and_then(|x| x.as_str()) {
                if !pairs.iter().any(|(r, _)| r == role) {
                    pairs.push((role.to_string(), m.to_string()));
                }
            }
        }
        // Also accept nested `{"router": {...}}` (Go round-trip shape).
        if let Some(inner) = obj.get("router").and_then(|x| x.as_object()) {
            for role in ["fast", "executor", "planner", "improver"] {
                if let Some(m) = inner.get(role).and_then(|x| x.as_str()) {
                    if !pairs.iter().any(|(r, _)| r == role) {
                        pairs.push((role.to_string(), m.to_string()));
                    }
                }
            }
        }
    }
    if pairs.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "missing field `role`"}))).into_response();
    }
    for (role, _) in &pairs {
        if !["fast", "executor", "planner", "improver"].contains(&role.as_str()) {
            return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "bad role"}))).into_response();
        }
    }
    // Apply all pairs; emit + persist per role (last wins for response).
    let mut last = pairs.last().cloned().unwrap();
    for (role, model) in pairs {
        st.router.set_model(&role, &model);
        if let Some(p) = &st.cfg_path {
            if let Err(e) = crate::config::persist_router_model(p, &role, &model) {
                return (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error": e.to_string()}))).into_response();
            }
        }
        st.emit(ev("config.updated", "", serde_json::json!({"role": role, "model": model})));
        last = (role, model);
    }
    Json(serde_json::json!({"ok": true, "role": last.0, "model": last.1})).into_response()
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
    match st.router.list_models(&q.provider).await {
        Ok(models) => {
            let models = plugin_models(&st, &q.provider, &def, models);
            Json(serde_json::json!({"provider": q.provider, "models": models})).into_response()
        }
        Err(e) => (StatusCode::BAD_GATEWAY, Json(serde_json::json!({"error": e.to_string()}))).into_response(),
    }
}

/// `models.list` plugin hook: lets plugins replace/enrich the core model
/// list. Runs on the
/// blocking pool; a silent/missing plugin keeps the core list.
fn plugin_models(
    st: &AppState,
    provider: &str,
    def: &crate::llm::ProviderDef,
    mut models: Vec<crate::llm::ModelMeta>,
) -> Vec<crate::llm::ModelMeta> {
    let payload = serde_json::json!({
        "provider": provider,
        "base_url": def.base_url,
        "protocol": def.protocol,
        "api_key_env": st.cfg.providers.get(provider).map(|p| p.api_key_env.clone()).unwrap_or_default(),
        "api_key_cmd": st.cfg.providers.get(provider).map(|p| p.api_key_cmd.clone()).unwrap_or_default(),
        "models": models,
    });
    let out = tokio::task::block_in_place(|| {
        st.plugins.global().dispatch(
            "models.list",
            &payload,
            std::time::Duration::from_secs(10),
        )
    });
    let extra = out
        .as_ref()
        .and_then(|o| o.get("patch"))
        .and_then(|p| p.get("models"))
        .and_then(|m| m.as_array())
        .cloned()
        .unwrap_or_default();
    if extra.is_empty() {
        return models;
    }
    merge_models(&mut models, &extra);
    models
}

/// Merge plugin-supplied model entries into the core list: entries with an
/// `id` upsert by id (plugin wins on metadata), then sort by id.
fn merge_models(core: &mut Vec<crate::llm::ModelMeta>, extra: &[serde_json::Value]) {
    use std::collections::HashMap;
    let mut idx: HashMap<String, usize> =
        core.iter().enumerate().map(|(i, m)| (m.id.clone(), i)).collect();
    for e in extra {
        let Some(id) = e.get("id").and_then(|i| i.as_str()).filter(|s| !s.is_empty()) else { continue };
        let meta = crate::llm::ModelMeta {
            id: id.to_string(),
            name: e
                .get("name")
                .and_then(|n| n.as_str())
                .filter(|s| !s.is_empty())
                .unwrap_or(id)
                .to_string(),
            context_window: e.get("context_window").and_then(|n| n.as_u64()).unwrap_or(0) as u32,
            supported_efforts: e
                .get("supported_efforts")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect()
                })
                .unwrap_or_default(),
        };
        if let Some(&i) = idx.get(id) {
            core[i] = meta;
        } else {
            idx.insert(id.to_string(), core.len());
            core.push(meta);
        }
    }
    core.sort_by(|a, b| a.id.cmp(&b.id));
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

async fn kb_query(State(st): State<AppState>, Json(b): Json<KbQueryBody>) -> impl IntoResponse {
    let ws = if b.directory.is_empty() {
        std::env::current_dir().unwrap_or_else(|_| ".".into())
    } else {
        PathBuf::from(b.directory)
    };
    let ov = if st.cfg.kb.memory_dir.trim().is_empty() { None } else { Some(st.cfg.kb.memory_dir.as_str()) };
    match crate::kb::query_in(&ws, &b.query, b.budget, ov) {
        Ok(text) => Json(serde_json::json!({"query": b.query, "context": text})).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, Json(serde_json::json!({"error": e.to_string()}))).into_response(),
    }
}

/// Browse rule-declared KB dirs (*.md) + graphify-out docs (?path= to read one).
async fn kb_browse(Query(q): Query<HashMap<String, String>>) -> impl IntoResponse {
    let base = std::env::current_dir().unwrap_or_else(|_| ".".into());
    // Allowed roots: each resolved KB dir (+ its graphify-out), the legacy
    // memory/ tree, and the workspace code graph. Anything else is 403.
    let mut roots: Vec<String> = crate::kb::resolve_memory_dirs(&base)
        .iter()
        .map(|d| {
            d.strip_prefix(&base)
                .map(|r| r.to_string_lossy().replace('\\', "/"))
                .unwrap_or_else(|_| "memory".into())
        })
        .collect();
    for legacy in ["memory", "graphify-out"] {
        if !roots.contains(&legacy.to_string()) {
            roots.push(legacy.into());
        }
    }
    if let Some(p) = q.get("path") {
        // Contain reads to the allowed roots (credential fence applies at the tool layer).
        let abs = base.join(p);
        let first_two = {
            let mut it = std::path::Path::new(p).iter();
            let a = it.next().map(|c| c.to_string_lossy().into_owned()).unwrap_or_default();
            let b = it.next().map(|c| c.to_string_lossy().into_owned()).unwrap_or_default();
            if b == "graphify-out" { format!("{a}/graphify-out") } else { a }
        };
        let ok = roots.iter().any(|r| first_two == *r || p == r || p.starts_with(&format!("{r}/")));
        if !ok {
            return (StatusCode::FORBIDDEN, Json(serde_json::json!({"error": "path must be under a rule-declared KB dir, memory/, or graphify-out/"}))).into_response();
        }
        return match std::fs::read_to_string(&abs) {
            Ok(text) => Json(serde_json::json!({"path": p, "content": text.chars().take(32_000).collect::<String>()})).into_response(),
            Err(e) => (StatusCode::NOT_FOUND, Json(serde_json::json!({"error": e.to_string()}))).into_response(),
        };
    }
    let mut files = Vec::new();
    let mut dirs: Vec<String> = roots.clone();
    for r in &roots {
        if !r.ends_with("graphify-out") {
            dirs.push(format!("{r}/graphify-out"));
        }
    }
    dirs.push("graphify-out".into());
    dirs.sort();
    dirs.dedup();
    for dir in dirs {
        let d = base.join(&dir);
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
    answers: serde_json::Value,
    #[serde(default)]
    dismissed: bool,
}

/// Normalize answer payloads: canonical `[[...], ...]` per-question lists,
/// or a flat `["a", ...]` shorthand meaning one answer per question
/// (lenient clients, scripts). Anything else is an empty answer set.
fn normalize_answers(v: &serde_json::Value) -> Vec<Vec<String>> {
    let Some(items) = v.as_array() else { return Vec::new() };
    if items.iter().all(|x| x.as_str().is_some()) {
        return items
            .iter()
            .filter_map(|x| x.as_str().map(|s| vec![s.to_string()]))
            .collect();
    }
    items
        .iter()
        .map(|q| {
            q.as_array()
                .map(|a| a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect())
                .unwrap_or_default()
        })
        .collect()
}

async fn answer_question(
    State(st): State<AppState>,
    Path(req_id): Path<String>,
    Json(b): Json<QuestionAnswer>,
) -> impl IntoResponse {
    if b.dismissed {
        if st.gate.dismiss(&req_id) {
            return Json(serde_json::json!({"ok": true})).into_response();
        }
    } else if st.gate.answer(&req_id, normalize_answers(&b.answers)) {
        return Json(serde_json::json!({"ok": true})).into_response();
    }
    (StatusCode::NOT_FOUND, Json(serde_json::json!({"error": "unknown or expired request"}))).into_response()
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
            "expires_in": dc.expires_in,
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

async fn auth_device_poll(State(st): State<AppState>, Query(q): Query<DevicePollQ>) -> impl IntoResponse {
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
            // Fresh token: drop any cached session so the next call
            // re-exchanges at once (no daemon restart).
            st.router.drop_copilot_session(&provider);
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
    st.emit(ev("harness.evolve.started", "", serde_json::json!({})));
    let ws = std::env::current_dir().unwrap_or_else(|_| ".".into());
    // Blocking git worktree ops: run off the async executor.
    let out = tokio::task::spawn_blocking(move || crate::harness::offline_pass(&ws)).await;
    match out {
        Ok(Ok(msg)) => {
            st.emit(ev("harness.evolve.completed", "", serde_json::json!({"result": msg})));
            Json(serde_json::json!({"status": "completed", "result": msg})).into_response()
        }
        Ok(Err(e)) => {
            st.emit(ev("harness.evolve.failed", "", serde_json::json!({"error": e.to_string()})));
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
    // Remember the pre-revert working tree (including uncommitted turn
    // output) so /redo can come back. Best-effort: without it, unrevert 404s.
    match crate::checkpoints::snapshot(&ws, &id, "pre-revert") {
        Ok(pre) => {
            let _ = st.store.add_trace(&id, "revert_from", &serde_json::json!({"commit": pre}).to_string());
        }
        Err(_) => {
            let head = std::process::Command::new("git").arg("-C").arg(&ws).arg("rev-parse").arg("HEAD").output();
            if let Ok(o) = head {
                if o.status.success() {
                    let _ = st.store.add_trace(&id, "revert_from", &serde_json::json!({"commit": String::from_utf8_lossy(&o.stdout).trim()}).to_string());
                }
            }
        }
    }
    match crate::checkpoints::restore(&ws, &commit) {
        Ok(_) => {
            st.emit(ev("session.reverted", &id, serde_json::json!({"turn": turn})));
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
            st.emit(ev("session.unreverted", &id, serde_json::json!({})));
            Json(serde_json::json!({"session": id, "commit": commit})).into_response()
        }
        Err(e) => (StatusCode::BAD_GATEWAY, Json(serde_json::json!({"error": e.to_string()}))).into_response(),
    }
}

async fn compact(State(st): State<AppState>, Path(id): Path<String>) -> impl IntoResponse {
    // Never slice history out from under a live turn (its messages snapshot
    // and per-round appends assume stable history).
    let claimed_live = st
        .store
        .turn_owner(&id)
        .map(|(_, since)| chrono::Utc::now().timestamp() - since < 600)
        .unwrap_or(false);
    if st.running.lock().unwrap().contains(&id) || claimed_live {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({"error": "turn in flight; compact after it completes"})),
        )
            .into_response();
    }
    match Runner::compact_session(&st.store, &st.router, &st.cfg, &id).await {
        Ok(msg) => {
            st.emit(ev("session.compacted", &id, serde_json::json!({"result": msg})));
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

/// MIME type for an embedded SPA path (extension-based; unknown blobs
/// download as octet-stream).
fn spa_mime(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or("") {
        "js" | "mjs" => "application/javascript",
        "css" => "text/css",
        "html" => "text/html",
        "json" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        _ => "application/octet-stream",
    }
}

async fn spa_fallback(uri: axum::http::Uri) -> impl IntoResponse {
    // Serve the actual embedded asset (with a real MIME type) when the
    // path names one; only unknown paths fall back to index.html for
    // client-side routing. (Serving index.html for /assets/*.js breaks
    // module loading: the browser rejects text/html for <script type=module>.)
    let rel = uri.path().trim_start_matches('/');
    if !rel.is_empty() {
        if let Some(f) = Spa::get(rel) {
            // Hashed build output is immutable; entry documents are not.
            let cache = if rel.starts_with("assets/") {
                "public, max-age=31536000, immutable"
            } else {
                "no-cache"
            };
            return (
                StatusCode::OK,
                [
                    ("content-type", spa_mime(rel)),
                    ("cache-control", cache),
                ],
                f.data.into_owned(),
            )
                .into_response();
        }
    }
    if let Some(f) = Spa::get("index.html") {
        let html = String::from_utf8_lossy(&f.data).into_owned();
        // Mirror web/index.html's own policy: bare `default-src 'self'`
        // would additionally kill unsafe-inline styles and the ws: PTY
        // terminal (policies combine restrictively).
        const CSP: &str = "default-src 'self'; connect-src 'self' ws:; style-src 'self' 'unsafe-inline'; font-src 'self'";
        (
            StatusCode::OK,
            [(axum::http::header::CONTENT_SECURITY_POLICY, CSP)],
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

#[cfg(test)]
mod models_hook_tests {
    use super::*;

    #[test]
    fn prompt_idempotency_dedups_within_ttl() {
        let s = crate::store::Store::open(Some(":memory:")).unwrap();
        assert!(!s.seen_request("s1", "k1", IDEM_TTL_SECS as i64).unwrap(), "first submit passes");
        assert!(s.seen_request("s1", "k1", IDEM_TTL_SECS as i64).unwrap(), "immediate resubmit dedups");
        assert!(!s.seen_request("s1", "k2", IDEM_TTL_SECS as i64).unwrap(), "different key passes");
        assert!(!s.seen_request("s2", "k1", IDEM_TTL_SECS as i64).unwrap(), "different session passes");
        // Negative TTL ages everything out: exercises the prune path.
        assert!(!s.seen_request("s1", "k1", -1).unwrap(), "expired key passes again");
    }

    fn meta(id: &str, window: u32) -> crate::llm::ModelMeta {
        crate::llm::ModelMeta {
            id: id.into(),
            name: id.into(),
            context_window: window,
            supported_efforts: Vec::new(),
        }
    }

    #[test]
    fn merge_models_upserts_by_id_and_sorts() {
        let mut core = vec![meta("b-model", 0), meta("a-model", 100)];
        merge_models(
            &mut core,
            serde_json::json!([
                {"id": "b-model", "name": "B", "context_window": 1048576,
                 "supported_efforts": ["low", "high"]},
                {"id": "c-model"},
                {"name": "no-id-skipped"},
                {"id": ""},
            ])
            .as_array()
            .unwrap(),
        );
        assert_eq!(core.len(), 3, "{core:?}");
        assert_eq!(core[0].id, "a-model");
        assert_eq!(core[0].context_window, 100);
        assert_eq!(core[1].id, "b-model");
        assert_eq!(core[1].name, "B");
        assert_eq!(core[1].context_window, 1048576);
        assert_eq!(core[1].supported_efforts, vec!["low", "high"]);
        // Missing name falls back to id; missing window/efforts to zero/empty.
        assert_eq!(core[2].id, "c-model");
        assert_eq!(core[2].name, "c-model");
        assert_eq!(core[2].context_window, 0);
        assert!(core[2].supported_efforts.is_empty());
    }

    /// Backend-continuation guarantee: dropping the drain guard frees the
    /// cross-daemon claim and the same-daemon `running` entry even on
    /// panic-unwind paths — a dead drain must never wedge the session.
    #[test]
    fn turn_release_guard_frees_claim_and_running() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.db").display().to_string();
        let store = Arc::new(Store::open(Some(Box::leak(db.into_boxed_str()))).unwrap());
        let ws = dir.path().display().to_string();
        let sess = store.create_session(&ws, "build", "bypass", "").unwrap();
        let running = Arc::new(Mutex::new(HashSet::new()));
        running.lock().unwrap().insert(sess.id.clone());
        assert!(store.claim_turn(&sess.id, "owner-a", 600));
        {
            let _release = TurnRelease {
                store: store.clone(),
                running: running.clone(),
                id: sess.id.clone(),
                owner: "owner-a".into(),
            };
        }
        assert!(!running.lock().unwrap().contains(&sess.id), "running entry must clear on drop");
        assert!(store.claim_turn(&sess.id, "owner-b", 600), "claim must free on drop");
    }
}

#[cfg(test)]
mod drive_tests {
    use super::*;
    use std::io::{Read, Write};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Scripted gateway: each accepted POST gets the next canned response
    /// (raw HTTP status line + body). Serves `plan.len()` connections.
    fn scripted_gateway(plan: Vec<(&'static str, &'static str)>) -> (String, std::sync::Arc<AtomicUsize>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let hits = std::sync::Arc::new(AtomicUsize::new(0));
        let hits2 = hits.clone();
        std::thread::spawn(move || {
            for (status, body) in plan {
                let Ok((mut s, _)) = listener.accept() else { return };
                let mut head = Vec::new();
                while !head.ends_with(b"\r\n\r\n") && head.len() < 65536 {
                    let mut b = [0u8; 1];
                    if s.read_exact(&mut b).is_err() {
                        return;
                    }
                    head.push(b[0]);
                }
                let head_str = String::from_utf8_lossy(&head);
                let len = head_str
                    .lines()
                    .find_map(|l| {
                        l.strip_prefix("Content-Length:")
                            .or_else(|| l.strip_prefix("content-length:"))
                    })
                    .and_then(|v| v.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                let mut rest = vec![0u8; len];
                if len > 0 && s.read_exact(&mut rest).is_err() {
                    return;
                }
                hits2.fetch_add(1, Ordering::SeqCst);
                let resp = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = s.write_all(resp.as_bytes());
            }
        });
        (addr, hits)
    }

    fn drive_state(payloads: Vec<(&'static str, &'static str)>) -> (AppState, Runner, String, std::sync::Arc<AtomicUsize>) {
        let (addr, hits) = scripted_gateway(payloads);
        let (st, runner, id) = app_state_for(&format!("http://{addr}"));
        (st, runner, id, hits)
    }

    /// Chat-scripted gateway for auto-continue tests: POSTs serve payloads
    /// in order (counted); GET /models answers `{"data":[]}` uncounted so
    /// the context guard's window lookup can't eat a chat payload.
    fn chat_gateway(payloads: Vec<String>) -> (String, std::sync::Arc<AtomicUsize>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let hits = std::sync::Arc::new(AtomicUsize::new(0));
        let hits2 = hits.clone();
        std::thread::spawn(move || {
            let mut payloads = payloads.into_iter();
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { return };
                let mut head = Vec::new();
                while !head.ends_with(b"\r\n\r\n") && head.len() < 65536 {
                    let mut b = [0u8; 1];
                    if s.read_exact(&mut b).is_err() {
                        return;
                    }
                    head.push(b[0]);
                }
                let head_str = String::from_utf8_lossy(&head).into_owned();
                if head_str.starts_with("GET") {
                    let body = "{\"data\":[]}";
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = s.write_all(resp.as_bytes());
                    continue;
                }
                let len = head_str
                    .lines()
                    .find_map(|l| {
                        l.strip_prefix("Content-Length:")
                            .or_else(|| l.strip_prefix("content-length:"))
                    })
                    .and_then(|v| v.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                let mut rest = vec![0u8; len];
                if len > 0 && s.read_exact(&mut rest).is_err() {
                    return;
                }
                let Some(p) = payloads.next() else { return };
                hits2.fetch_add(1, Ordering::SeqCst);
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    p.len(),
                    p
                );
                let _ = s.write_all(resp.as_bytes());
            }
        });
        (base, hits)
    }

    fn app_state_for(base_url: &str) -> (AppState, Runner, String) {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().display().to_string();
        // tempdir must outlive the test: leak the path join like the
        // agent harness leaks its DB path (Store holds no dir handle,
        // sessions only carry the workspace string).
        let ws_leak: &'static str = Box::leak(ws.into_boxed_str());
        let db = std::path::Path::new(ws_leak).join("t.db");
        let db_leak: &'static str = Box::leak(db.display().to_string().into_boxed_str());
        // Keep the tempdir alive for the test (Store only carries paths).
        std::mem::forget(dir);
        let store = Store::open(Some(db_leak)).unwrap();
        let mut providers = HashMap::new();
        providers.insert(
            "testprov".into(),
            crate::config::ProviderConfig {
                protocol: "openai-compatible".into(),
                base_url: base_url.into(),
                api_key_env: String::new(),
                api_key_cmd: String::new(),
                token_url: String::new(),
            },
        );
        let cfg = crate::config::Config {
            router: crate::config::RouterConfig {
                fast: String::new(),
                executor: "testprov/echo".into(),
                planner: String::new(),
                improver: String::new(),
            },
            providers,
            ..Default::default()
        };
        let router = crate::llm::Router::new(&cfg).unwrap();
        let st = AppState::new(store, router, cfg, None);
        let sess = st.store.create_session(ws_leak, "build", "bypass", "").unwrap();
        (st, Runner::new(), sess.id)
    }

    const FINISH: &str = "data: {\"choices\":[{\"delta\":{\"content\":\"recovered\"}}]}\n\n";

    /// Transient gateway 500 then success: the turn recovers via the one
    /// retry instead of dying with an empty transcript.
    #[tokio::test]
    async fn drive_one_retries_transient_failure() {
        let (st, runner, id, hits) =
            drive_state(vec![("500 Internal Server Error", ""), ("200 OK", FINISH)]);
        let q = QueuedPrompt { text: "hi there".into(), effort: String::new(), images: Vec::new() };
        let out = drive_one(&st, &runner, &id, "test-owner", q).await;
        let done = out.unwrap();
        assert_eq!(done.text, "recovered", "retry must recover the turn");
        assert!(!done.stopped);
        assert_eq!(hits.load(Ordering::SeqCst), 2, "one failed attempt plus one retry");
        let user_turns = st.store.turns(&id).unwrap().into_iter().filter(|t| t.role == "user" && t.text == "hi there").count();
        assert_eq!(user_turns, 1, "retry must reuse the user turn, not duplicate it");
    }

    /// Persistent gateway failure: two attempts, then a failure note lands
    /// in the transcript so `continue` resumes with context.
    #[tokio::test]
    async fn drive_one_persists_failure_note() {
        let (st, runner, id, hits) =
            drive_state(vec![("500 Internal Server Error", ""), ("500 Internal Server Error", "")]);
        let q = QueuedPrompt { text: "hi there".into(), effort: String::new(), images: Vec::new() };
        let out = drive_one(&st, &runner, &id, "test-owner", q).await;
        assert!(out.is_err(), "persistent failure must still surface, got {out:?}");
        assert_eq!(hits.load(Ordering::SeqCst), 2, "exactly one retry, no loop");
        let turns = st.store.turns(&id).unwrap();
        let last = turns.last().unwrap();
        assert_eq!(last.role, "assistant");
        assert!(last.text.contains("Turn failed"), "failure note must persist, got {:?}", last.text);
        assert!(last.text.contains("continue"), "note must carry the resume pointer");
    }

    /// Pre-set stop flag: the turn ends immediately without touching the
    /// gateway, persists the stop note, and clears the flag — no retry.
    #[tokio::test]
    async fn drive_one_honors_stop_flag() {
        let (st, runner, id, hits) = drive_state(vec![("200 OK", FINISH)]);
        st.store.request_stop(&id);
        let q = QueuedPrompt { text: "hi there".into(), effort: String::new(), images: Vec::new() };
        let out = drive_one(&st, &runner, &id, "test-owner", q).await;
        let done = out.unwrap();
        assert!(done.stopped, "stop flag must end the turn as stopped");
        assert_eq!(done.text, crate::agent::STOPPED_TEXT);
        assert_eq!(hits.load(Ordering::SeqCst), 0, "stop must not call the gateway");
        assert!(!st.store.stop_requested(&id), "stop flag must clear once honored");
        let turns = st.store.turns(&id).unwrap();
        let last = turns.last().unwrap();
        assert_eq!(last.role, "assistant");
        assert!(last.text.contains("Stopped by user"), "stop note must persist, got {:?}", last.text);
    }

    fn text_payload(s: &str) -> String {
        let body = serde_json::json!({"choices": [{"delta": {"content": s}}]});
        format!("data: {body}\n\n")
    }

    /// Auto-continue: a work request ending with zero edits loops back in
    /// server-side (cap 1 → 2 run_prompts = 6 gateway hits) instead of
    /// asking the user to type `continue`.
    #[tokio::test]
    async fn drive_one_auto_continues_unfinished_work() {
        let payloads: Vec<String> = ["w1", "w2", "fin-one", "x1", "x2", "fin-two"]
            .iter()
            .map(|s| text_payload(s))
            .collect();
        let (base, hits) = chat_gateway(payloads);
        let (mut st, runner, id) = app_state_for(&base);
        st.cfg.agent.max_auto_continues = 1;
        let q = QueuedPrompt { text: "fix the orange glow".into(), effort: String::new(), images: Vec::new() };
        let out = drive_one(&st, &runner, &id, "test-owner", q).await;
        let done = out.unwrap();
        assert!(!done.stopped);
        assert_eq!(hits.load(Ordering::SeqCst), 6, "initial turn + 1 auto turn (3 rounds each)");
        assert!(done.text.contains("fin-two"), "final text is the auto turn's, got {:?}", done.text);
        assert!(done.text.contains("NOT done"), "pointer preserved, got {:?}", done.text);
        let synth = st.store.turns(&id).unwrap().into_iter().filter(|t| t.role == "user" && t.text.starts_with("auto-continue")).count();
        assert_eq!(synth, 1, "exactly one synthetic continue turn persisted");
    }

    /// Identical consecutive tails stop the loop early: same payloads twice
    /// with cap 3 ends after 2 run_prompts (6 hits), not 4 (12 hits).
    #[tokio::test]
    async fn drive_one_auto_continue_stops_on_identical_tail() {
        let payloads: Vec<String> = ["s1", "s2", "same-end", "s1", "s2", "same-end"]
            .iter()
            .map(|s| text_payload(s))
            .collect();
        let (base, hits) = chat_gateway(payloads);
        let (st, runner, id) = app_state_for(&base);
        assert_eq!(st.cfg.agent.max_auto_continues, 3, "default cap covers 4 run_prompts");
        let q = QueuedPrompt { text: "fix the orange glow".into(), effort: String::new(), images: Vec::new() };
        let out = drive_one(&st, &runner, &id, "test-owner", q).await;
        let done = out.unwrap();
        assert_eq!(hits.load(Ordering::SeqCst), 6, "identical tail must break before the cap, got {:?}", done.text);
        assert!(done.text.contains("same-end"), "keeps the completed text, got {:?}", done.text);
    }

    #[test]
    fn context_full_trips_at_85_percent() {
        assert!(!context_full(0, 0), "unknown window never trips");
        assert!(!context_full(84, 100));
        assert!(context_full(85, 100));
        assert!(context_full(9_000, 10_000));
    }

    fn queued(
        st: &AppState,
        id: &str,
        items: Vec<(&str, bool)>,
    ) {
        // (text, with_image): image payloads stand in for real attachments.
        let mut map = st.queued.lock().unwrap();
        let q = map.entry(id.to_string()).or_default();
        for (text, with_image) in items {
            q.push_back(QueuedPrompt {
                text: text.into(),
                effort: String::new(),
                images: if with_image {
                    vec![crate::llm::ImageAttachment {
                        name: "a.png".into(),
                        media_type: "image/png".into(),
                        data: "AAA".into(),
                    }]
                } else {
                    Vec::new()
                },
            });
        }
    }

    /// Cancel drops the item and hands its payload back for restore.
    #[tokio::test]
    async fn queue_cancel_returns_payload() {
        let (st, _runner, id, _hits) = drive_state(vec![]);
        queued(&st, &id, vec![("first", false), ("with pic", true)]);
        let (code, Json(v)) = queue_cancel(State(st.clone()), Path(id.clone()), Json(QueueIndexBody { index: 1 })).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(v["text"], "with pic");
        assert_eq!(v["images"][0]["name"], "a.png");
        assert_eq!(v["images"][0]["media_type"], "image/png");
        assert_eq!(st.queued.lock().unwrap().get(&id).map(|q| q.len()), Some(1));
        let (code, _) = queue_cancel(State(st.clone()), Path(id.clone()), Json(QueueIndexBody { index: 5 })).await;
        assert_eq!(code, StatusCode::NOT_FOUND);
    }

    /// Steer moves text-only items into the steer inbox, in order.
    #[tokio::test]
    async fn queue_steer_moves_text_to_inbox() {
        let (st, _runner, id, _hits) = drive_state(vec![]);
        queued(&st, &id, vec![("now please", false), ("later", false)]);
        let (code, Json(v)) = queue_steer(State(st.clone()), Path(id.clone()), Json(QueueIndexBody { index: 0 })).await;
        assert_eq!(code, StatusCode::ACCEPTED);
        assert_eq!(v["text"], "now please");
        assert_eq!(st.store.take_steered(&id), "now please");
        assert_eq!(st.queued.lock().unwrap().get(&id).map(|q| q.len()), Some(1));
    }

    /// Steer refuses items with attachments in place (cancel restores them).
    #[tokio::test]
    async fn queue_steer_refuses_attachments() {
        let (st, _runner, id, _hits) = drive_state(vec![]);
        queued(&st, &id, vec![("with pic", true)]);
        let (code, _) = queue_steer(State(st.clone()), Path(id.clone()), Json(QueueIndexBody { index: 0 })).await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert_eq!(st.queued.lock().unwrap().get(&id).map(|q| q.len()), Some(1));
        assert_eq!(st.store.take_steered(&id), "", "refused steer must not touch the inbox");
    }
}

#[cfg(test)]
mod spa_tests {
    use super::*;
    use axum::http::Uri;

    #[test]
    fn spa_mime_covers_bundle_types() {
        for (path, want) in [
            ("a.js", "application/javascript"),
            ("a.mjs", "application/javascript"),
            ("a.css", "text/css"),
            ("a.html", "text/html"),
            ("a.json", "application/json"),
            ("a.svg", "image/svg+xml"),
            ("a.png", "image/png"),
            ("a.ico", "image/x-icon"),
            ("a.woff", "font/woff"),
            ("a.woff2", "font/woff2"),
            ("a.ttf", "font/ttf"),
            ("a.bin", "application/octet-stream"),
            ("noext", "application/octet-stream"),
        ] {
            assert_eq!(spa_mime(path), want, "{path}");
        }
    }

    #[tokio::test]
    async fn spa_fallback_serves_assets_with_mime() {
        let js = Spa::iter()
            .find(|p| p.ends_with(".js"))
            .expect("bundle must embed a .js asset");
        let uri = Uri::from_maybe_shared(format!("/{js}")).unwrap();
        let resp = spa_fallback(uri).await.into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get("content-type").unwrap(),
            "application/javascript",
            "bundled .js must not be served as text/html (breaks <script type=module>)"
        );
        let body = axum::body::to_bytes(resp.into_body(), 32 * 1024 * 1024).await.unwrap();
        let embedded = Spa::get(js.as_ref()).unwrap();
        assert_eq!(&body[..], &embedded.data[..], "asset body must be the embedded file, not index.html");
    }

    #[tokio::test]
    async fn spa_fallback_unknown_route_serves_index_with_full_csp() {
        let resp = spa_fallback(Uri::from_static("/s/definitely-missing")).await.into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get("content-security-policy").unwrap(),
            "default-src 'self'; connect-src 'self' ws:; style-src 'self' 'unsafe-inline'; font-src 'self'",
            "server CSP must mirror web/index.html (bare default-src kills ws: PTY + inline styles)"
        );
        let body = axum::body::to_bytes(resp.into_body(), 32 * 1024 * 1024).await.unwrap();
        assert!(
            body.windows(14).any(|w| w == b"<div id=\"root\""),
            "unknown routes must serve the SPA entry"
        );
    }
}
