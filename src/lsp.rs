//! Workspace language-server client: real LSP over stdio.
//!
//! The `lsp` tool speaks JSON-RPC (Content-Length framing) to one server
//! process per (workspace root, language), spawned lazily on first use:
//! `textDocument/definition`, `references`, `documentSymbol`, and
//! diagnostics (pull when the server advertises it, push-cache otherwise).
//! A missing server binary fails the single tool call with an install
//! hint — never the turn.
//!
//! Concurrency: every request serializes on one mutex per server and
//! matches responses by id; parallel `lsp` tool batches simply queue.

use std::{
    collections::HashMap,
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, ChildStdout, Stdio},
    sync::{mpsc, Arc, Mutex},
    time::Duration,
};

use crate::config::LspConfig;

const INIT_TIMEOUT: Duration = Duration::from_secs(20);
const REQ_TIMEOUT: Duration = Duration::from_secs(30);
/// How long to drain push diagnostics after didOpen/didChange.
const DIAG_DRAIN: Duration = Duration::from_secs(2);
const MAX_ITEMS: usize = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Lang {
    Rust,
    Go,
    TypeScript,
    Python,
}

impl Lang {
    fn id(&self) -> &'static str {
        match self {
            Lang::Rust => "rust",
            Lang::Go => "go",
            Lang::TypeScript => "typescript",
            Lang::Python => "python",
        }
    }

    fn default_cmd(&self) -> &'static str {
        match self {
            Lang::Rust => "rust-analyzer",
            Lang::Go => "gopls",
            Lang::TypeScript => "typescript-language-server --stdio",
            Lang::Python => "pylsp",
        }
    }

    fn install_hint(&self) -> &'static str {
        match self {
            Lang::Rust => "rustup component add rust-analyzer",
            Lang::Go => "go install golang.org/x/tools/gopls@latest",
            Lang::TypeScript => "npm i -g typescript typescript-language-server",
            Lang::Python => "pip install python-lsp-server",
        }
    }

    /// Workspace-root markers, most specific first.
    fn markers(&self) -> &'static [&'static str] {
        match self {
            Lang::Rust => &["Cargo.toml"],
            Lang::Go => &["go.mod"],
            Lang::TypeScript => &["tsconfig.json", "package.json"],
            Lang::Python => &["pyproject.toml", "setup.py", "setup.cfg"],
        }
    }

    fn of(path: &Path) -> Option<Lang> {
        match path.extension().and_then(|e| e.to_str()) {
            Some("rs") => Some(Lang::Rust),
            Some("go") => Some(Lang::Go),
            Some("ts") | Some("tsx") | Some("js") | Some("jsx") | Some("mjs") | Some("cjs") => {
                Some(Lang::TypeScript)
            }
            Some("py") => Some(Lang::Python),
            _ => None,
        }
    }

    fn supported() -> &'static str {
        "rs (rust-analyzer), go (gopls), ts/tsx/js (typescript-language-server), py (pylsp)"
    }
}

/// Walk up from `file` looking for a language marker, then `.git`;
/// otherwise the file's parent dir. Never escapes to `/` silently — the
/// parent dir is the honest fallback for lone files.
fn root_for(file: &Path, lang: Lang) -> PathBuf {
    let mut dir = file.parent().map(|p| p.to_path_buf());
    while let Some(d) = dir.clone() {
        for m in lang.markers() {
            if d.join(m).exists() {
                return d;
            }
        }
        if d.join(".git").exists() {
            return d;
        }
        match d.parent() {
            Some(p) if p != d => dir = Some(p.to_path_buf()),
            _ => break,
        }
    }
    file.parent().map(|p| p.to_path_buf()).unwrap_or_else(|| PathBuf::from("."))
}

/// `file://` URI with minimal percent-encoding (spaces, non-ASCII).
fn uri_of(abs: &Path) -> String {
    let s = abs.display().to_string();
    let mut out = String::with_capacity(s.len() + 7);
    out.push_str("file://");
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' | b':' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// 1-based Unicode-scalar column → UTF-16 code units for LSP positions.
fn col_to_utf16(line_text: &str, col_1based: usize) -> usize {
    let n = col_1based.saturating_sub(1);
    line_text.chars().take(n).map(|c| c.len_utf16()).sum()
}

/// UTF-16 code units → 1-based Unicode-scalar column for display.
fn col_from_utf16(line_text: &str, units: usize) -> usize {
    let mut used = 0;
    let mut col = 1;
    for c in line_text.chars() {
        let w = c.len_utf16();
        if used + w > units {
            break;
        }
        used += w;
        col += 1;
    }
    col
}

fn encode(msg: &serde_json::Value) -> Vec<u8> {
    let body = serde_json::to_vec(msg).unwrap_or_default();
    let mut out = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    out.extend_from_slice(&body);
    out
}

/// Read one Content-Length frame from a language server.
fn read_frame(reader: &mut BufReader<ChildStdout>) -> std::io::Result<String> {
    let mut len: Option<usize> = None;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line)?;
        let t = line.trim();
        if t.is_empty() {
            break;
        }
        if let Some(v) = t.strip_prefix("Content-Length:") {
            len = v.trim().parse().ok();
        }
    }
    let len = len.ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "lsp: missing Content-Length"))?;
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf)?;
    String::from_utf8(buf).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))
}

#[derive(Debug, Clone)]
struct CachedDiag {
    line_1: u64,
    col_1: u64,
    severity: String,
    message: String,
}

struct ServerInner {
    child: Child,
    stdin: ChildStdin,
    next_id: u64,
    /// Files already opened (uri → (version, content snapshot)).
    opened: HashMap<String, (i32, String)>,
    /// Latest push diagnostics per file uri.
    diags: HashMap<String, Vec<CachedDiag>>,
    pull_supported: bool,
    /// Pumped stdout frames (filled by the background thread).
    rx: mpsc::Receiver<Result<String, String>>,
}

pub struct Server {
    inner: Mutex<ServerInner>,
}

impl Drop for Server {
    fn drop(&mut self) {
        // Best-effort clean exit first (killing mid-session logs a
        // server-side "exited without proper shutdown" error); the kill
        // below is the backstop. The server is a cache, never load-bearing.
        let _ = self.notify(&serde_json::json!({"jsonrpc": "2.0", "method": "exit"}));
        if let Ok(mut g) = self.inner.lock() {
            let _ = g.child.kill();
        }
    }
}

impl Server {
    fn spawn(root: &Path, cmdline: &str) -> Result<Self, String> {
        let mut parts = cmdline.split_whitespace();
        let bin = parts.next().unwrap_or("").to_string();
        if bin.is_empty() {
            return Err("lsp: empty server command".into());
        }
        let args: Vec<String> = parts.map(|s| s.to_string()).collect();
        let mut child = std::process::Command::new(&bin)
            .args(&args)
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    format!("lsp: server binary `{bin}` not found")
                } else {
                    format!("lsp: failed to spawn `{bin}`: {e}")
                }
            })?;
        let stdin = child.stdin.take().ok_or("lsp: no stdin")?;
        let stdout = child.stdout.take().ok_or("lsp: no stdout")?;
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                match read_frame(&mut reader) {
                    Ok(body) => {
                        if tx.send(Ok(body)).is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(Err(e.to_string()));
                        break;
                    }
                }
            }
        });
        let server = Self {
            inner: Mutex::new(ServerInner {
                child,
                stdin,
                next_id: 1,
                opened: HashMap::new(),
                diags: HashMap::new(),
                pull_supported: false,
                rx,
            }),
        };
        let root_uri = uri_of(root);
        let init = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 0,
            "method": "initialize",
            "params": {
                "processId": null,
                "rootUri": root_uri,
                "capabilities": {
                    "textDocument": {
                        "definition": {},
                        "references": {},
                        "documentSymbol": { "hierarchicalDocumentSymbolSupport": true },
                        "publishDiagnostics": { "relatedInformation": true },
                        "diagnostic": {}
                    }
                }
            }
        });
        let resp = server.request_raw(&init, INIT_TIMEOUT)?;
        if let Some(caps) = resp.get("result").and_then(|r| r.get("capabilities")) {
            let pull = caps.get("diagnosticProvider").is_some();
            server.inner.lock().map(|mut g| g.pull_supported = pull).map_err(|e| e.to_string())?;
        }
        server.notify(&serde_json::json!({"jsonrpc": "2.0", "method": "initialized", "params": {}}))?;
        Ok(server)
    }

    fn notify(&self, msg: &serde_json::Value) -> Result<(), String> {
        let bytes = encode(msg);
        self.inner.lock().map_err(|e| e.to_string())?.stdin.write_all(&bytes).map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Send a request and wait for the matching response id, harvesting
    /// push diagnostics seen along the way.
    fn request_raw(&self, msg: &serde_json::Value, timeout: Duration) -> Result<serde_json::Value, String> {
        let want = msg.get("id").and_then(|v| v.as_u64()).unwrap_or(u64::MAX);
        let bytes = encode(msg);
        let mut g = self.inner.lock().map_err(|e| e.to_string())?;
        g.stdin.write_all(&bytes).map_err(|e| format!("lsp: write: {e}"))?;
        g.stdin.flush().map_err(|e| format!("lsp: flush: {e}"))?;
        loop {
            let body = g.rx.recv_timeout(timeout).map_err(|_| "lsp: request timed out".to_string())??;
            let v: serde_json::Value = serde_json::from_str(&body).map_err(|e| format!("lsp: bad frame: {e}"))?;
            if v.get("method").and_then(|m| m.as_str()) == Some("textDocument/publishDiagnostics") {
                Self::harvest(&mut g, &v);
                continue;
            }
            let id = v.get("id").and_then(|i| i.as_u64()).unwrap_or(u64::MAX);
            if id == want {
                if let Some(err) = v.get("error") {
                    return Err(format!("lsp: server error: {err}"));
                }
                return v.get("result").cloned().ok_or_else(|| "lsp: missing result".to_string());
            }
            // Some other response/notification: ignore (single-flight use).
        }
    }

    fn harvest(g: &mut ServerInner, v: &serde_json::Value) {
        let uri = v.get("params").and_then(|p| p.get("uri")).and_then(|u| u.as_str()).unwrap_or("").to_string();
        if uri.is_empty() {
            return;
        }
        let mut out = Vec::new();
        if let Some(arr) = v.get("params").and_then(|p| p.get("diagnostics")).and_then(|d| d.as_array()) {
            for d in arr {
                let line_1 = d.get("range").and_then(|r| r.get("start")).and_then(|s| s.get("line")).and_then(|l| l.as_u64()).unwrap_or(0) + 1;
                let col_0 = d.get("range").and_then(|r| r.get("start")).and_then(|s| s.get("character")).and_then(|c| c.as_u64()).unwrap_or(0);
                let sev = match d.get("severity").and_then(|s| s.as_u64()).unwrap_or(1) {
                    1 => "error",
                    2 => "warning",
                    3 => "info",
                    _ => "hint",
                };
                let msg_txt = d.get("message").and_then(|m| m.as_str()).unwrap_or("").to_string();
                out.push(CachedDiag { line_1, col_1: col_0 + 1, severity: sev.into(), message: msg_txt });
            }
        }
        g.diags.insert(uri, out);
    }

    fn next_id(&self) -> u64 {
        self.inner.lock().map(|mut g| {
            let id = g.next_id;
            g.next_id += 1;
            id
        }).unwrap_or(1)
    }

    /// Open the file if new, or didChange when the disk content moved on.
    /// Returns the file uri.
    fn sync_open(&self, abs: &Path, lang: Lang, text: &str) -> Result<String, String> {
        let uri = uri_of(abs);
        let (known, same) = self.inner.lock().map(|g| {
            (g.opened.contains_key(&uri), g.opened.get(&uri).map(|(_, t)| t == text).unwrap_or(false))
        }).map_err(|e| e.to_string())?;
        if known && same {
            return Ok(uri);
        }
        if known {
            let version = self.inner.lock().map(|mut g| {
                let e = g.opened.get_mut(&uri).unwrap();
                e.0 += 1;
                e.1 = text.to_string();
                e.0
            }).map_err(|e| e.to_string())?;
            let params = serde_json::json!({
                "textDocument": { "uri": uri.clone(), "version": version },
                "contentChanges": [{ "text": text }],
            });
            self.notify(&serde_json::json!({
                "jsonrpc": "2.0",
                "method": "textDocument/didChange",
                "params": params,
            }))?;
        } else {
            self.inner.lock().map(|mut g| {
                g.opened.insert(uri.clone(), (1, text.to_string()));
            }).map_err(|e| e.to_string())?;
            let params = serde_json::json!({
                "textDocument": {
                    "uri": uri.clone(),
                    "languageId": lang.id(),
                    "version": 1,
                    "text": text,
                },
            });
            self.notify(&serde_json::json!({
                "jsonrpc": "2.0",
                "method": "textDocument/didOpen",
                "params": params,
            }))?;
            // Give push-diagnostic servers a beat to answer didOpen.
            self.drain_diags(&uri);
        }
        Ok(uri)
    }

    /// Short blocking drain harvesting any pending push diagnostics.
    /// Holds no lock across iterations; concurrent requests match by id
    /// and harvest into the same cache, so nothing is lost.
    fn drain_diags(&self, want_uri: &str) {
        let deadline = std::time::Instant::now() + DIAG_DRAIN;
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                break;
            }
            let body = {
                let g = match self.inner.lock() {
                    Ok(g) => g,
                    Err(_) => break,
                };
                match g.rx.recv_timeout(left) {
                    Ok(Ok(body)) => body,
                    _ => break,
                }
            };
            let v: serde_json::Value = match serde_json::from_str(&body) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if v.get("method").and_then(|m| m.as_str()) != Some("textDocument/publishDiagnostics") {
                continue; // progress noise; the request path harvests the rest
            }
            let hit = v.get("params").and_then(|p| p.get("uri")).and_then(|u| u.as_str()) == Some(want_uri);
            if let Ok(mut g) = self.inner.lock() {
                Self::harvest(&mut g, &v);
            }
            if hit {
                break;
            }
        }
    }

    fn position(&self, line_1: u64, col_1: u64, text: &str) -> serde_json::Value {
        let line_0 = line_1.saturating_sub(1);
        let line_text = text.lines().nth(line_0 as usize).unwrap_or("");
        serde_json::json!({"line": line_0, "character": col_to_utf16(line_text, col_1 as usize)})
    }

    fn request(&self, method: &str, params: serde_json::Value) -> Result<serde_json::Value, String> {
        let id = self.next_id();
        self.request_raw(&serde_json::json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}), REQ_TIMEOUT)
    }

    /// Normalize Location | Location[] | LocationLink[] → (path, line_1, col_1).
    fn locations(v: &serde_json::Value) -> Vec<(String, u64, u64)> {
        let mut out = Vec::new();
        let arr: Vec<&serde_json::Value> = match v {
            serde_json::Value::Array(a) => a.iter().collect(),
            o if o.is_object() => vec![o],
            _ => vec![],
        };
        for loc in arr {
            let (uri, range) = if loc.get("targetUri").is_some() {
                (
                    loc.get("targetUri").and_then(|u| u.as_str()).unwrap_or(""),
                    loc.get("targetSelectionRange").or_else(|| loc.get("targetRange")),
                )
            } else {
                (loc.get("uri").and_then(|u| u.as_str()).unwrap_or(""), loc.get("range"))
            };
            let path = uri.strip_prefix("file://").unwrap_or(uri).to_string();
            let line_1 = range.and_then(|r| r.get("start")).and_then(|s| s.get("line")).and_then(|l| l.as_u64()).unwrap_or(0) + 1;
            let col_0 = range.and_then(|r| r.get("start")).and_then(|s| s.get("character")).and_then(|c| c.as_u64()).unwrap_or(0);
            if !path.is_empty() {
                out.push((path, line_1, col_0 + 1));
            }
        }
        out
    }

    fn symbol_kind_name(n: u64) -> &'static str {
        match n {
            1 => "file", 2 => "module", 3 => "namespace", 4 => "package", 5 => "class",
            6 => "method", 7 => "property", 8 => "field", 9 => "constructor", 10 => "enum",
            11 => "interface", 12 => "function", 13 => "variable", 14 => "constant",
            15 => "string", 16 => "number", 17 => "boolean", 18 => "array", 19 => "object",
            20 => "key", 21 => "null", 22 => "enum-member", 23 => "struct", 24 => "event",
            25 => "operator", 26 => "type-param", _ => "symbol",
        }
    }
}

pub struct Manager {
    cfg: LspConfig,
    servers: Mutex<HashMap<String, Arc<Server>>>,
}

impl Manager {
    pub fn new(cfg: LspConfig) -> Self {
        Self { cfg, servers: Mutex::new(HashMap::new()) }
    }

    fn command_for(&self, lang: Lang) -> String {
        self.cfg.commands.get(lang.id()).cloned().unwrap_or_else(|| lang.default_cmd().into())
    }

    /// High-level query used by the `lsp` tool. `op` is one of
    /// `symbols | definition | references | diagnostics`; `line`/`character`
    /// are 1-based (0/0 = start of file).
    pub fn query(&self, workspace: &Path, op: &str, rel: &str, line: u64, character: u64, filter: &str) -> Result<String, String> {
        if self.cfg.disabled {
            return Err("lsp: disabled by `[lsp] disabled = true`".into());
        }
        let op = op.trim().to_lowercase();
        if !matches!(op.as_str(), "symbols" | "definition" | "references" | "diagnostics" | "") {
            return Err(format!("lsp: unknown op `{op}` (want symbols|definition|references|diagnostics)"));
        }
        let op = if op.is_empty() { "symbols" } else { op.as_str() };
        let rel = rel.trim();
        if rel.is_empty() {
            return Err("lsp: `path` is required (workspace-relative file)".into());
        }
        // Defensive fence (the tool layer enforces the workspace fence first).
        let abs = workspace.join(rel);
        let abs = clean_path(&abs);
        if !abs.starts_with(workspace) {
            return Err("lsp: path escapes the workspace".into());
        }
        let lang = Lang::of(&abs).ok_or_else(|| format!("lsp: unsupported extension for `{rel}` (supported: {})", Lang::supported()))?;
        let text = std::fs::read_to_string(&abs).map_err(|e| format!("lsp: cannot read `{rel}`: {e}"))?;
        let root = root_for(&abs, lang);
        let cmd = self.command_for(lang);
        let key = format!("{}|{}", root.display(), lang.id());
        // Get-or-start under one lock: a second thread with the same key
        // waits out the (one-time) initialize instead of spawning a twin.
        let server: Arc<Server> = {
            let mut servers = self.servers.lock().map_err(|e| e.to_string())?;
            if let Some(s) = servers.get(&key) {
                s.clone()
            } else {
                let s = Arc::new(Server::spawn(&root, &cmd).map_err(|e| {
                    if e.contains("not found") {
                        format!("{e} — install it ({}) or set `[lsp.commands]`", lang.install_hint())
                    } else {
                        e
                    }
                })?);
                servers.insert(key, s.clone());
                s
            }
        };
        let uri = server.sync_open(&abs, lang, &text)?;
        match op {
            "symbols" => {
                let res = server.request("textDocument/documentSymbol", serde_json::json!({"textDocument": {"uri": uri}}))?;
                Ok(render_symbols(&res, rel, filter))
            }
            "definition" => {
                let res = server.request(
                    "textDocument/definition",
                    serde_json::json!({"textDocument": {"uri": uri}, "position": server.position(line.max(1), character.max(1), &text)}),
                )?;
                Ok(render_locations(&res, "definition", MAX_ITEMS))
            }
            "references" => {
                let res = server.request(
                    "textDocument/references",
                    serde_json::json!({"textDocument": {"uri": uri},
                        "position": server.position(line.max(1), character.max(1), &text),
                        "context": {"includeDeclaration": false}}),
                )?;
                Ok(render_locations(&res, "reference", 30))
            }
            "diagnostics" => Ok(server.diagnostics(&uri)?),
            _ => unreachable!(),
        }
    }
}

impl Server {
    fn diagnostics(&self, uri: &str) -> Result<String, String> {
        let pull = self.inner.lock().map(|g| g.pull_supported).map_err(|e| e.to_string())?;
        if pull {
            let res = self.request("textDocument/diagnostic", serde_json::json!({"textDocument": {"uri": uri}}))?;
            // Full pull: relatedDocuments or items.
            let items = res.get("items").and_then(|i| i.as_array()).cloned().unwrap_or_default();
            if items.is_empty() {
                return Ok("lsp: no diagnostics".into());
            }
            return Ok(render_diags(&items, uri));
        }
        self.drain_diags(uri);
        let cached = self.inner.lock().map(|g| g.diags.get(uri).cloned().unwrap_or_default()).map_err(|e| e.to_string())?;
        if cached.is_empty() {
            return Ok("lsp: no diagnostics (server reported none)".into());
        }
        let path = uri_path(uri);
        let mut out = String::new();
        for (i, d) in cached.iter().take(30).enumerate() {
            out.push_str(&format!("{}. {path}:{}:{} [{}] {}\n", i + 1, d.line_1, d.col_1, d.severity, d.message.lines().next().unwrap_or("")));
        }
        Ok(out)
    }
}

fn uri_path(uri: &str) -> String {
    uri.strip_prefix("file://").unwrap_or(uri).to_string()
}

/// Lexically clean a joined path (no filesystem touch, so symlinks stay).
fn clean_path(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in p.components() {
        use std::path::Component::*;
        match comp {
            Prefix(p) => out.push(p.as_os_str()),
            RootDir => out.push("/"),
            CurDir => {}
            ParentDir => {
                out.pop();
            }
            Normal(c) => out.push(c),
        }
    }
    out
}

fn render_symbols(v: &serde_json::Value, rel: &str, filter: &str) -> String {
    // Flatten hierarchical DocumentSymbol[] and legacy SymbolInformation[].
    let mut rows: Vec<(String, String, u64, u64)> = Vec::new(); // kind, name, line_1, col_1
    fn walk(node: &serde_json::Value, prefix: &str, rows: &mut Vec<(String, String, u64, u64)>) {
        let arr = match node {
            serde_json::Value::Array(a) => a,
            _ => return,
        };
        for s in arr {
            let name = s.get("name").and_then(|n| n.as_str()).unwrap_or("?");
            let kind = s.get("kind").and_then(|k| k.as_u64()).unwrap_or(0);
            // DocumentSymbol: selectionRange; SymbolInformation: location.range.
            let range = s.get("selectionRange").or_else(|| {
                s.get("location").and_then(|l| l.get("range"))
            }).or_else(|| s.get("range"));
            let line_1 = range.and_then(|r| r.get("start")).and_then(|st| st.get("line")).and_then(|l| l.as_u64()).unwrap_or(0) + 1;
            let col_0 = range.and_then(|r| r.get("start")).and_then(|st| st.get("character")).and_then(|c| c.as_u64()).unwrap_or(0);
            let full = if prefix.is_empty() { name.to_string() } else { format!("{prefix}::{name}") };
            rows.push((Server::symbol_kind_name(kind).into(), full.clone(), line_1, col_0 + 1));
            if let Some(children) = s.get("children") {
                walk(children, &full, rows);
            }
        }
    }
    walk(v, "", &mut rows);
    let f = filter.trim().to_lowercase();
    if !f.is_empty() {
        rows.retain(|(_, n, _, _)| n.to_lowercase().contains(&f));
    }
    if rows.is_empty() {
        return format!("lsp: no symbols in `{rel}`");
    }
    let mut out = String::new();
    for (i, (kind, name, line, col)) in rows.iter().take(MAX_ITEMS).enumerate() {
        out.push_str(&format!("{}. [{kind}] {name}  {rel}:{line}:{col}\n", i + 1));
    }
    if rows.len() > MAX_ITEMS {
        out.push_str(&format!("… {} more (narrow with `query`)\n", rows.len() - MAX_ITEMS));
    }
    out
}

fn render_locations(v: &serde_json::Value, what: &str, cap: usize) -> String {
    let mut locs = Server::locations(v);
    if locs.is_empty() {
        return format!("lsp: no {what} found");
    }
    locs.truncate(cap);
    let mut out = String::new();
    for (i, (path, line, col)) in locs.iter().enumerate() {
        // One-line preview keeps cross-file jumps readable without re-reads.
        let preview = std::fs::read_to_string(path)
            .ok()
            .and_then(|t| t.lines().nth((line.saturating_sub(1)) as usize).map(|s| s.trim().to_string()))
            .unwrap_or_default();
        let preview = preview.chars().take(120).collect::<String>();
        out.push_str(&format!("{}. {path}:{line}:{col}  {preview}\n", i + 1));
    }
    out
}

fn render_diags(items: &[serde_json::Value], uri: &str) -> String {
    let path = uri_path(uri);
    let mut out = String::new();
    for (i, d) in items.iter().take(30).enumerate() {
        let line_1 = d.get("range").and_then(|r| r.get("start")).and_then(|s| s.get("line")).and_then(|l| l.as_u64()).unwrap_or(0) + 1;
        let col_0 = d.get("range").and_then(|r| r.get("start")).and_then(|s| s.get("character")).and_then(|c| c.as_u64()).unwrap_or(0);
        let sev = match d.get("severity").and_then(|s| s.as_u64()).unwrap_or(1) {
            1 => "error",
            2 => "warning",
            3 => "info",
            _ => "hint",
        };
        let msg_txt = d.get("message").and_then(|m| m.as_str()).unwrap_or("");
        out.push_str(&format!("{}. {path}:{line_1}:{} [{sev}] {}\n", i + 1, col_0 + 1, msg_txt.lines().next().unwrap_or("")));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lang_detection() {
        assert_eq!(Lang::of(Path::new("a.rs")), Some(Lang::Rust));
        assert_eq!(Lang::of(Path::new("a.tsx")), Some(Lang::TypeScript));
        assert_eq!(Lang::of(Path::new("a.py")), Some(Lang::Python));
        assert_eq!(Lang::of(Path::new("a.go")), Some(Lang::Go));
        assert_eq!(Lang::of(Path::new("a.md")), None);
    }

    #[test]
    fn root_prefers_manifest_over_git() {
        let d = tempfile::tempdir().unwrap();
        let ws = d.path();
        std::fs::create_dir_all(ws.join("sub")).unwrap();
        std::fs::write(ws.join("Cargo.toml"), "[package]").unwrap();
        std::fs::create_dir_all(ws.join(".git")).unwrap();
        let f = ws.join("sub").join("main.rs");
        assert_eq!(root_for(&f, Lang::Rust), ws.to_path_buf());
        // Lone file with no markers and no .git above: parent dir.
        let d2 = tempfile::tempdir().unwrap();
        let lone = d2.path().join("x.py");
        std::fs::write(&lone, "").unwrap();
        assert_eq!(root_for(&lone, Lang::Python), d2.path().to_path_buf());
    }

    #[test]
    fn uri_encodes_spaces() {
        assert_eq!(uri_of(Path::new("/a b/c.rs")), "file:///a%20b/c.rs");
    }

    #[test]
    fn utf16_columns_roundtrip() {
        // "a😀b": a=1 unit, emoji=2 units, b=1 unit.
        let line = "a\u{1F600}b";
        assert_eq!(col_to_utf16(line, 1), 0);
        assert_eq!(col_to_utf16(line, 2), 1);
        assert_eq!(col_to_utf16(line, 3), 3);
        assert_eq!(col_to_utf16(line, 4), 4);
        assert_eq!(col_from_utf16(line, 0), 1);
        assert_eq!(col_from_utf16(line, 1), 2);
        assert_eq!(col_from_utf16(line, 3), 3);
        assert_eq!(col_from_utf16(line, 4), 4);
    }

    #[test]
    fn locations_normalize_all_shapes() {
        let v = serde_json::json!([
            {"uri": "file:///a.rs", "range": {"start": {"line": 4, "character": 6}}},
            {"targetUri": "file:///b.rs", "targetSelectionRange": {"start": {"line": 0, "character": 0}}},
        ]);
        let locs = Server::locations(&v);
        assert_eq!(locs, vec![("/a.rs".to_string(), 5, 7), ("/b.rs".to_string(), 1, 1)]);
    }

    #[test]
    fn symbols_flatten_and_filter() {
        let v = serde_json::json!([
            {"name": "S", "kind": 23, "selectionRange": {"start": {"line": 1, "character": 0}},
             "children": [{"name": "m", "kind": 6, "selectionRange": {"start": {"line": 2, "character": 4}}}]}
        ]);
        let out = render_symbols(&v, "a.rs", "");
        assert!(out.contains("[struct] S"), "{out}");
        assert!(out.contains("S::m"), "{out}");
        let out = render_symbols(&v, "a.rs", "zzz");
        assert!(out.contains("no symbols"), "{out}");
    }

    #[test]
    fn missing_binary_fails_with_install_hint() {
        let m = Manager::new(LspConfig { disabled: false, commands: [("rust".to_string(), "definitely-not-a-server-xyz".to_string())].into_iter().collect() });
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("main.rs"), "fn main() {}\n").unwrap();
        // Root falls back to the file's parent dir (no markers).
        let err = Manager::query(&m, d.path(), "definition", "main.rs", 1, 1, "").unwrap_err();
        assert!(err.contains("not found"), "{err}");
        assert!(err.contains("rustup"), "{err}");
    }

    #[test]
    fn disabled_config_refuses() {
        let m = Manager::new(LspConfig { disabled: true, commands: HashMap::new() });
        let d = tempfile::tempdir().unwrap();
        assert!(Manager::query(&m, d.path(), "symbols", "a.rs", 0, 0, "").is_err());
    }
}

#[cfg(test)]
mod live_tests {
    use super::*;

    /// Minimal fake LSP server: Content-Length JSON-RPC with canned
    /// initialize / documentSymbol / definition / references, plus a push
    /// `publishDiagnostics` after every didOpen.
    const FAKE_LSP_PY: &str = r#"import sys, json
def read_frame():
    length = None
    while True:
        line = sys.stdin.buffer.readline().decode()
        if not line or line.strip() == "":
            break
        if line.lower().startswith("content-length:"):
            length = int(line.split(":")[1].strip())
    if length is None:
        return None
    return json.loads(sys.stdin.buffer.read(length).decode())
def send(obj):
    body = json.dumps(obj).encode()
    sys.stdout.buffer.write(b"Content-Length: %d\r\n\r\n" % len(body) + body)
    sys.stdout.buffer.flush()
while True:
    req = read_frame()
    if req is None:
        break
    m = req.get("method")
    rid = req.get("id")
    if m == "initialize":
        send({"jsonrpc": "2.0", "id": rid, "result": {"capabilities": {}}})
    elif m == "initialized" or m.startswith("textDocument/did"):
        if "didOpen" in m:
            p = req["params"]["textDocument"]
            send({"jsonrpc": "2.0", "method": "textDocument/publishDiagnostics",
                  "params": {"uri": p["uri"], "diagnostics": [
                      {"range": {"start": {"line": 0, "character": 0}}, "severity": 1, "message": "fake boom"}]}})
    elif m == "textDocument/documentSymbol":
        send({"jsonrpc": "2.0", "id": rid, "result": [
            {"name": "main", "kind": 12, "selectionRange": {"start": {"line": 0, "character": 3}}}]})
    elif m == "textDocument/definition":
        send({"jsonrpc": "2.0", "id": rid, "result": {"uri": "file:///fake.rs", "range": {"start": {"line": 9, "character": 2}}}})
    elif m == "textDocument/references":
        send({"jsonrpc": "2.0", "id": rid, "result": [
            {"uri": "file:///fake.rs", "range": {"start": {"line": 1, "character": 0}}},
            {"uri": "file:///fake.rs", "range": {"start": {"line": 5, "character": 1}}}]})
    elif m == "shutdown":
        send({"jsonrpc": "2.0", "id": rid, "result": None})
    elif m == "exit":
        break
"#;

    /// Minimal fake LSP server: Content-Length JSON-RPC with canned
    /// initialize / documentSymbol / definition / references, plus a push
    /// `publishDiagnostics` after every didOpen.
    fn fake_server_py() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("lumina-lsp-test-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join("fake_lsp.py");
        std::fs::write(&p, FAKE_LSP_PY).unwrap();
        p
    }

    fn manager_with_fake() -> (Manager, tempfile::TempDir) {
        let py = fake_server_py();
        let cmd = format!("python3 {}", py.display());
        let mut commands = HashMap::new();
        commands.insert("rust".to_string(), cmd);
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("main.rs"), "fn main() {}\n").unwrap();
        (Manager::new(LspConfig { disabled: false, commands }), d)
    }

    #[test]
    fn live_symbols_definition_references() {
        let (m, d) = manager_with_fake();
        let out = Manager::query(&m, d.path(), "symbols", "main.rs", 0, 0, "").unwrap();
        assert!(out.contains("[function] main"), "{out}");
        let out = Manager::query(&m, d.path(), "definition", "main.rs", 1, 4, "").unwrap();
        assert!(out.contains("/fake.rs:10:3"), "{out}");
        let out = Manager::query(&m, d.path(), "references", "main.rs", 1, 4, "").unwrap();
        assert!(out.contains("/fake.rs:2:1") && out.contains("/fake.rs:6:2"), "{out}");
    }

    #[test]
    fn live_push_diagnostics_cached() {
        let (m, d) = manager_with_fake();
        // didOpen triggers the fake push; the diagnostics op drains it.
        let out = Manager::query(&m, d.path(), "diagnostics", "main.rs", 0, 0, "").unwrap();
        assert!(out.contains("fake boom"), "{out}");
    }

    /// Real rust-analyzer end-to-end (skips unless RUST_ANALYZER_BIN points
    /// at a binary and `cargo` is on PATH). Uses a real mini cargo project:
    /// detached files get syntax-only service (no cross-ref resolution).
    /// Semantic queries retry briefly — the first request can land while
    /// the server is still loading the workspace.
    #[test]
    fn live_rust_analyzer_end_to_end() {
        let bin = std::env::var("RUST_ANALYZER_BIN").unwrap_or_default();
        if bin.is_empty() || !std::path::Path::new(&bin).exists() {
            eprintln!("skip: set RUST_ANALYZER_BIN to a rust-analyzer binary");
            return;
        }
        if std::process::Command::new("cargo").arg("--version").output().is_err() {
            eprintln!("skip: cargo not on PATH (rust-analyzer needs it for workspace load)");
            return;
        }
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("Cargo.toml"), "[package]\nname = \"lspfixture\"\nversion = \"0.0.0\"\nedition = \"2021\"\n").unwrap();
        std::fs::create_dir_all(d.path().join("src")).unwrap();
        std::fs::write(
            d.path().join("src").join("lib.rs"),
            "pub struct Widget { pub name: String }\n\npub fn make_widget(n: String) -> Widget {\n    Widget { name: n }\n}\n",
        )
        .unwrap();
        let mut commands = HashMap::new();
        commands.insert("rust".to_string(), bin);
        let m = Manager::new(LspConfig { disabled: false, commands });
        let syms = Manager::query(&m, d.path(), "symbols", "src/lib.rs", 0, 0, "").unwrap();
        assert!(syms.contains("Widget") && syms.contains("make_widget"), "{syms}");
        // `n` used at line 4 -> defined at line 3 (the `n: String` param).
        let mut defs = String::new();
        for _ in 0..30 {
            defs = Manager::query(&m, d.path(), "definition", "src/lib.rs", 4, 20, "").unwrap_or_default();
            if defs.contains("lib.rs:3:") {
                break;
            }
            std::thread::sleep(std::time::Duration::from_secs(2));
        }
        assert!(defs.contains("lib.rs:3:"), "{defs}");
        // Syntax diagnostics need no workspace load: a parse error shows fast.
        std::fs::write(
            d.path().join("src").join("lib.rs"),
            "pub struct Widget { pub name: String }\n\npub fn broken( -> i32 {\n}\n",
        )
        .unwrap();
        let mut diags = String::new();
        for _ in 0..15 {
            diags = Manager::query(&m, d.path(), "diagnostics", "src/lib.rs", 0, 0, "").unwrap_or_default();
            if diags.contains("[error]") {
                break;
            }
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
        assert!(diags.contains("[error]"), "{diags}");
    }

    /// Real server end-to-end (skips when typescript-language-server is not
    /// installed: `npm i -g typescript typescript-language-server`).
    /// Exercises spawn → initialize → didOpen → documentSymbol / definition /
    /// push diagnostics against a fixture with a genuine type error.
    #[test]
    fn live_typescript_server_end_to_end() {
        let bin = "/tmp/opencode/lsp-ts/node_modules/.bin/typescript-language-server";
        if !std::path::Path::new(bin).exists() {
            eprintln!("skip: no typescript-language-server at {bin}");
            return;
        }
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("tsconfig.json"), r#"{"compilerOptions": {"strict": true}}"#).unwrap();
        std::fs::write(
            d.path().join("app.ts"),
            "export function add(a: number, b: number): number {\n  return a + b;\n}\n\nexport const wrong: number = \"oops\";\n",
        )
        .unwrap();
        let mut commands = HashMap::new();
        commands.insert("typescript".to_string(), format!("{bin} --stdio"));
        let m = Manager::new(LspConfig { disabled: false, commands });
        let syms = Manager::query(&m, d.path(), "symbols", "app.ts", 0, 0, "").unwrap();
        assert!(syms.contains("add"), "{syms}");
        let defs = Manager::query(&m, d.path(), "definition", "app.ts", 2, 10, "").unwrap();
        assert!(defs.contains("app.ts:1:"), "{defs}");
        // Diagnostics need the server to finish loading: retry briefly.
        let mut diags = String::new();
        for _ in 0..15 {
            diags = Manager::query(&m, d.path(), "diagnostics", "app.ts", 0, 0, "").unwrap_or_default();
            if diags.contains("[error]") {
                break;
            }
            std::thread::sleep(std::time::Duration::from_secs(2));
        }
        assert!(diags.contains("[error]") && diags.contains(":5:"), "{diags}");
    }
}
