//! Daemon HTTP layer: axum router + SSE + embedded SPA.
//! Mirrors `internal/server/server.go` route surface (M0: skeleton with
//! real routing/auth/CSP; handlers return 501 until M1/M2 land).

use axum::{
    http::{HeaderMap, StatusCode},
    response::{Html, IntoResponse, Json},
    routing::get,
    Router,
};
use rust_embed::RustEmbed;
use serde_json::json;

#[derive(RustEmbed)]
#[folder = "web/dist/"]
#[exclude = "*.map"]
struct Spa;

#[derive(Clone)]
pub struct DaemonOpts {
    pub auth_token: String,
}

pub fn router(opts: DaemonOpts) -> Router {
    Router::new()
        .route("/openapi.json", get(openapi))
        .route("/api/v1/events", get(events))
        .route("/api/v1/sessions", get(todo).post(todo))
        .route("/api/v1/sessions/", get(todo).post(todo))
        .route("/api/v1/permissions/", axum::routing::post(todo))
        .route("/api/v1/questions/", axum::routing::post(todo))
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
        .layer(axum::Extension(opts))
}

pub fn is_loopback(host: &str) -> bool {
    host == "127.0.0.1" || host == "::1" || host == "localhost"
}

/// Non-loopback binds require LUMINA_TOKEN (checked per request in M1; helper here).
pub fn bearer_ok(headers: &HeaderMap, token: &str) -> bool {
    if token.is_empty() {
        return false;
    }
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v == format!("Bearer {token}"))
}

async fn openapi() -> impl IntoResponse {
    Json(json!({
        "openapi": "3.1.0",
        "info": {"title": "Lumina daemon", "version": "0.1.0"},
        "paths": {
            "/api/v1/events": "SSE: turn/context/delta/tool/completed/failed, permission.asked/resolved, session.*, config.updated",
            "/api/v1/sessions": "list (?directory=, ?latest=1) / create",
            "/api/v1/sessions/{id}": "fetch / patch agent,permission,model,title",
            "/api/v1/sessions/{id}/prompt": "submit turn",
            "/api/v1/sessions/{id}/fork": "branch at turn",
            "/api/v1/sessions/{id}/export": "transcript (?sanitize=1)",
            "/api/v1/sessions/{id}/revert": "shadow-git undo",
            "/api/v1/permissions/{reqId}": "allow_once/session/always|deny",
            "/api/v1/files": "workspace tree",
            "/api/v1/pty": "websocket shell",
            "/api/v1/kb/query": "graphify+memory query",
            "/api/v1/harness/evolve": "Propose-Probe-Compose"
        }
    }))
}

async fn events() -> impl IntoResponse {
    // M1 wires the real SSE broker; keep headers correct now.
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(json!({"error": "events broker lands in M1"})),
    )
}

async fn todo() -> impl IntoResponse {
    (StatusCode::NOT_IMPLEMENTED, Json(json!({"error": "not yet ported (M1/M2)"})))
}

async fn spa_fallback(uri: axum::http::Uri) -> impl IntoResponse {
    let _ = uri;
    // Serve embedded web/dist when `pnpm --dir web build` has run; else hint.
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
            Json(json!({"ok": true, "hint": "run `pnpm --dir web install && pnpm --dir web build`, then restart `lumina serve`"})),
        )
            .into_response()
    }
}
