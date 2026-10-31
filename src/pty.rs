//! Shell-over-websocket endpoint (`GET /api/v1/pty`).
//! Mirrors Go `wsServeLoop`: a minimal RFC6455 text loop where each message
//! executes via `sh -c` in the daemon workspace and the combined output is
//! written back. The web xterm talks to this; a full interactive PTY with
//! row/col resize lands only if the web client ever needs more than
//! line-execution (it doesn't today).

use axum::{
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    response::IntoResponse,
};
use std::time::Duration;

pub async fn pty(ws: WebSocketUpgrade) -> impl IntoResponse {
    ws.on_upgrade(serve_shell)
}

async fn serve_shell(mut socket: WebSocket) {
    let workspace = std::env::current_dir().unwrap_or_else(|_| ".".into());
    while let Some(msg) = socket.recv().await {
        let Ok(msg) = msg else { break };
        let line = match msg {
            Message::Text(t) => t.to_string(),
            Message::Binary(_) => continue,
            Message::Close(_) => break,
            _ => continue,
        };
        if line.trim().is_empty() {
            continue;
        }
        // ponytail: spawn sh per line with a hard timeout; no shell state kept.
        let out = tokio::task::spawn_blocking({
            let workspace = workspace.clone();
            let line = line.clone();
            move || {
                let mut child = std::process::Command::new("sh")
                    .arg("-c")
                    .arg(&line)
                    .current_dir(&workspace)
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .spawn()
                    .map_err(|e| e.to_string())?;
                let start = std::time::Instant::now();
                loop {
                    match child.try_wait() {
                        Ok(Some(_)) => break,
                        Ok(None) if start.elapsed() > Duration::from_secs(120) => {
                            let _ = child.kill();
                            break;
                        }
                        Ok(None) => std::thread::sleep(Duration::from_millis(25)),
                        Err(_) => break,
                    }
                }
                child.wait_with_output().map_err(|e| e.to_string())
            }
        })
        .await;
        let text = match out {
            Ok(Ok(o)) => {
                let mut t = String::from_utf8_lossy(&o.stdout).into_owned();
                let err = String::from_utf8_lossy(&o.stderr);
                if !err.is_empty() {
                    t.push_str("\n[stderr]\n");
                    t.push_str(&err);
                }
                if t.is_empty() {
                    "(exit ok, no output)".to_string()
                } else {
                    t.chars().take(64_000).collect()
                }
            }
            Ok(Err(e)) => format!("pty: {e}"),
            Err(e) => format!("pty: task failed: {e}"),
        };
        if socket.send(Message::Text(text.into())).await.is_err() {
            break;
        }
    }
}
