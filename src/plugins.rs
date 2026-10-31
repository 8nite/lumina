//! Lumina-compatible JS/TS plugin host: on-demand Node v22 worker over a Unix
//! domain socket (line-delimited JSON-RPC). The worker script is embedded at
//! compile time (`plugins_host.mjs`, vendored from `internal/plugins/host.mjs`)
//! and materialized to a temp file at runtime, so the Rust binary stays
//! single-file with zero cost when no JS/TS plugins are configured.

use serde_json::Value;
use std::{
    collections::HashMap,
    io::{BufRead, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum PluginError {
    #[error("plugin: {0}")]
    Msg(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

const HOST_JS: &str = include_str!("plugins_host.mjs");

/// Discover plugin files: global `~/.config/lumina/plugins/*.{js,mjs,ts}` +
/// `<name>/plugin.{js,mjs,ts}`, workspace `.lumina/plugins/`, Harness²
/// `.lumina/harness/plugins/` (user-approved), plus opt-in compat dir.
pub fn discover(workspace: &Path, opt_in_external: bool) -> Vec<PathBuf> {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".into());
    let mut roots = vec![Path::new(&home).join(".config/lumina/plugins")];
    if !workspace.as_os_str().is_empty() {
        roots.push(workspace.join(".lumina/plugins"));
        roots.push(workspace.join(".lumina/harness/plugins"));
    }
    if opt_in_external {
        roots.push(Path::new(&home).join(".config/lumina/compat-plugins"));
    }
    let mut out = Vec::new();
    for root in roots {
        let Ok(rd) = std::fs::read_dir(&root) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                for cand in ["plugin.js", "plugin.mjs", "plugin.ts", "index.js"] {
                    let c = p.join(cand);
                    if c.is_file() {
                        out.push(c);
                    }
                }
                continue;
            }
            if let Some(ext) = p.extension().and_then(|e| e.to_str()) {
                if matches!(ext, "js" | "mjs" | "ts") {
                    out.push(p);
                }
            }
        }
    }
    out.sort();
    out
}

struct Proc {
    child: std::process::Child,
    sock: PathBuf,
}

/// Persistent worker for one plugin set. Blocking dispatch (5s default, longer
/// for attachment hooks) so a buggy plugin never hangs the daemon — the
/// caller decides timeouts per hook.
pub struct Host {
    plugins: Vec<PathBuf>,
    proc: Mutex<Option<Proc>>,
}

impl Host {
    pub fn new(plugins: Vec<PathBuf>) -> Self {
        Self { plugins, proc: Mutex::new(None) }
    }

    pub fn is_empty(&self) -> bool {
        self.plugins.is_empty()
    }

    pub fn loaded(&self) -> Vec<String> {
        self.plugins.iter().map(|p| p.display().to_string()).collect()
    }

    fn materialize() -> Result<PathBuf, PluginError> {
        let p = std::env::temp_dir().join(format!("lumina-host-{}.mjs", std::process::id()));
        // Refresh on every start (dev iterations of the worker apply immediately).
        std::fs::write(&p, HOST_JS)?;
        Ok(p)
    }

    fn ensure_started(&self) -> Result<(), PluginError> {
        let mut g = self.proc.lock().unwrap();
        if g.is_some() {
            return Ok(());
        }
        let sock = std::env::temp_dir().join(format!("lumina-plugins-{}.sock", uuid::Uuid::new_v4()));
        let _ = std::fs::remove_file(&sock);
        let script = Self::materialize()?;
        let defs: Vec<Value> = self
            .plugins
            .iter()
            .map(|p| serde_json::json!({"path": p.display().to_string()}))
            .collect();
        let mut child = std::process::Command::new("node")
            .arg(&script)
            .arg("--socket")
            .arg(&sock)
            .arg("--plugins")
            .arg(serde_json::to_string(&defs)?)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| PluginError::Msg(format!("plugin: node v22 required ({e})")))?;
        // Wait for the socket (worker unlinks + binds on boot).
        for _ in 0..50 {
            if sock.exists() {
                *g = Some(Proc { child, sock });
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
            if let Ok(Some(_)) = child.try_wait() {
                return Err(PluginError::Msg("plugin: node worker exited during boot".into()));
            }
        }
        let _ = child.kill();
        Err(PluginError::Msg("plugin: node worker did not bind its socket".into()))
    }

    /// Dispatch one hook; returns the worker's output object (or None when the
    /// worker is unreachable — hooks are advisory except `tool.execute.before`
    /// blocks and `permission.ask` verdicts, which callers enforce).
    pub fn dispatch(&self, hook: &str, payload: &Value, timeout: std::time::Duration) -> Option<Value> {
        if self.plugins.is_empty() {
            return None;
        }
        if self.ensure_started().is_err() {
            return None;
        }
        let sock = self.proc.lock().unwrap().as_ref().map(|p| p.sock.clone())?;
        let mut stream = std::os::unix::net::UnixStream::connect(&sock).ok()?;
        stream.set_read_timeout(Some(timeout)).ok()?;
        stream.set_write_timeout(Some(timeout)).ok()?;
        let line = serde_json::json!({"hook": hook, "payload": payload}).to_string() + "\n";
        stream.write_all(line.as_bytes()).ok()?;
        let mut reader = std::io::BufReader::new(&stream);
        let mut out = String::new();
        reader.read_line(&mut out).ok()?;
        serde_json::from_str(&out).ok()
    }

    /// Fire-and-forget (after-hooks, events): never blocks the turn.
    pub fn notify(&self, hook: &str, payload: &Value) {
        let _ = self.dispatch(hook, payload, std::time::Duration::from_secs(5));
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        if let Some(mut p) = self.proc.lock().unwrap().take() {
            let _ = p.child.kill();
            let _ = std::fs::remove_file(&p.sock);
        }
    }
}

/// One Host per workspace, started on demand.
#[derive(Clone, Default)]
pub struct Cache {
    hosts: Arc<Mutex<HashMap<String, Arc<Host>>>>,
}

impl Cache {
    pub fn new() -> Self {
        Self { hosts: Arc::new(Mutex::new(HashMap::new())) }
    }

    pub fn host_for(&self, workspace: &Path) -> Arc<Host> {
        let key = workspace.display().to_string();
        let mut hosts = self.hosts.lock().unwrap();
        hosts
            .entry(key)
            .or_insert_with(|| Arc::new(Host::new(discover(workspace, false))))
            .clone()
    }
}



#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovers_layouts() {
        let d = tempfile::tempdir().unwrap();
        let ws = d.path();
        std::fs::create_dir_all(ws.join(".lumina/plugins/foo")).unwrap();
        std::fs::write(ws.join(".lumina/plugins/top.js"), "export default {}").unwrap();
        std::fs::write(ws.join(".lumina/plugins/foo/plugin.mjs"), "export default {}").unwrap();
        let found = discover(ws, false);
        assert_eq!(found.len(), 2, "{found:?}");
        // Empty set short-circuits without spawning node.
        assert!(Host::new(vec![]).dispatch("config", &Value::Null, std::time::Duration::from_secs(1)).is_none());
    }
}

#[cfg(test)]
mod live_tests {
    use super::*;

    #[test]
    fn worker_round_trip() {
        let d = tempfile::tempdir().unwrap();
        let plug = d.path().join("p.mjs");
        // Factory style: echo the tool name back via a before-hook patch.
        std::fs::write(
            &plug,
            r#"export default async () => ({
                "tool.execute.before": async (input) => ({ patch: { args: { ...(input.args || {}), seen: input.tool } } }),
            })"#,
        )
        .unwrap();
        let host = Host::new(vec![plug]);
        let out = host
            .dispatch(
                "tool.execute.before",
                &serde_json::json!({"tool": "read", "args": {}}),
                std::time::Duration::from_secs(15),
            )
            .expect("worker should answer");
        let seen = out.get("patch").and_then(|p| p.get("args")).and_then(|a| a.get("seen"));
        assert_eq!(seen.and_then(|s| s.as_str()), Some("read"), "{out}");
    }
}
