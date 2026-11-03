//! Markdown + graphify RAG engine: cold-start reflect, constrained-vocab
//! expansion, native graph.json traversal (<2ms BFS/DFS/path/explain/god-nodes),
//! recursive [[wikilink]] resolution, dedup-on-write memory synthesis, and the
//! singleflight standing-ops sweeper. Zero vector DBs.

use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::{Path, PathBuf},
};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum KbError {
    #[error("kb: {0}")]
    Msg(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Node {
    pub id: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub r#type: String,
    #[serde(default)]
    pub file: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Edge {
    #[serde(default)]
    pub nodes: Vec<String>,
    #[serde(default)]
    pub relation: String,
    #[serde(default)]
    pub confidence: String,
    #[serde(default)]
    pub source_file: String,
}

pub struct Graph {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    adj: HashMap<String, Vec<usize>>,
}

impl Graph {
    /// Load + normalize graph.json (hyperedges[].nodes coerced, confidence
    /// defaulted, degenerate <2-node edges shrink-guarded).
    pub fn load(path: &Path) -> Result<Self, KbError> {
        let raw = std::fs::read_to_string(path)?;
        let wire: serde_json::Value = serde_json::from_str(&raw)?;
        let nodes: Vec<Node> = wire
            .get("nodes")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();
        let mut edges = Vec::new();
        for key in ["edges", "hyperedges"] {
            if let Some(arr) = wire.get(key).and_then(|v| v.as_array()) {
                for rm in arr {
                    if let Ok(mut e) = serde_json::from_value::<Edge>(rm.clone()) {
                        if e.nodes.len() < 2 {
                            // Tolerate {from,to} shaped edges.
                            continue;
                        }
                        if e.confidence.is_empty() {
                            e.confidence = "INFERRED".into();
                        }
                        if !matches!(e.confidence.as_str(), "EXTRACTED" | "INFERRED" | "AMBIGUOUS") {
                            e.confidence = "INFERRED".into();
                        }
                        edges.push(e);
                        continue;
                    }
                    let ft = (
                        rm.get("from").and_then(|v| v.as_str()).unwrap_or(""),
                        rm.get("to").and_then(|v| v.as_str()).unwrap_or(""),
                    );
                    if ft.0.is_empty() || ft.1.is_empty() {
                        continue;
                    }
                    edges.push(Edge {
                        nodes: vec![ft.0.into(), ft.1.into()],
                        relation: rm.get("relation").and_then(|v| v.as_str()).unwrap_or("").into(),
                        confidence: "INFERRED".into(),
                        source_file: rm.get("source_file").and_then(|v| v.as_str()).unwrap_or("").into(),
                    });
                }
            }
        }
        let mut g = Self { nodes, edges, adj: HashMap::new() };
        g.rebuild();
        Ok(g)
    }

    fn rebuild(&mut self) {
        self.adj.clear();
        for (i, e) in self.edges.iter().enumerate() {
            for n in &e.nodes {
                self.adj.entry(n.clone()).or_default().push(i);
            }
        }
    }

    pub fn neighbors(&self, id: &str) -> Vec<String> {
        let mut seen = HashSet::from([id.to_string()]);
        let mut out = Vec::new();
        if let Some(idxs) = self.adj.get(id) {
            for &ei in idxs {
                for n in &self.edges[ei].nodes {
                    if seen.insert(n.clone()) {
                        out.push(n.clone());
                    }
                }
            }
        }
        out.sort();
        out
    }

    pub fn bfs(&self, seed: &str, budget: usize) -> Vec<String> {
        let budget = if budget == 0 { 50 } else { budget };
        let mut seen = HashSet::from([seed.to_string()]);
        let mut queue = VecDeque::from([seed.to_string()]);
        let mut out = Vec::new();
        while let Some(cur) = queue.pop_front() {
            if out.len() >= budget {
                break;
            }
            out.push(cur.clone());
            for n in self.neighbors(&cur) {
                if seen.insert(n.clone()) {
                    queue.push_back(n);
                }
            }
        }
        out
    }

    #[allow(dead_code)]
    pub fn dfs(&self, seed: &str, budget: usize) -> Vec<String> {
        let budget = if budget == 0 { 50 } else { budget };
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        fn visit(g: &Graph, id: &str, out: &mut Vec<String>, seen: &mut HashSet<String>, budget: usize) {
            if out.len() >= budget || !seen.insert(id.to_string()) {
                return;
            }
            out.push(id.to_string());
            for n in g.neighbors(id) {
                visit(g, &n, out, seen, budget);
            }
        }
        visit(self, seed, &mut out, &mut seen, budget);
        out
    }

    #[allow(dead_code)]
    #[allow(dead_code)]
    pub fn shortest_path(&self, from: &str, to: &str) -> Option<Vec<String>> {
        if from == to {
            return Some(vec![from.into()]);
        }
        let mut prev: HashMap<String, String> = HashMap::from([(from.into(), String::new())]);
        let mut queue = VecDeque::from([from.to_string()]);
        while let Some(cur) = queue.pop_front() {
            for n in self.neighbors(&cur) {
                if prev.contains_key(&n) {
                    continue;
                }
                prev.insert(n.clone(), cur.clone());
                if n == to {
                    let mut path = vec![to.to_string()];
                    let mut c = to.to_string();
                    while let Some(p) = prev.get(&c) {
                        if p.is_empty() {
                            break;
                        }
                        path.push(p.clone());
                        c = p.clone();
                    }
                    path.reverse();
                    return Some(path);
                }
                queue.push_back(n);
            }
        }
        None
    }

    #[allow(dead_code)]
    #[allow(dead_code)]
    pub fn explain(&self, from: &str, to: &str) -> String {
        let Some(path) = self.shortest_path(from, to) else {
            return format!("no path between {from:?} and {to:?}");
        };
        let mut s = format!("{}\n", path.join(" → "));
        for w in path.windows(2) {
            let (a, b) = (&w[0], &w[1]);
            if let Some(idxs) = self.adj.get(a) {
                for &ei in idxs {
                    let e = &self.edges[ei];
                    if e.nodes.contains(b) {
                        s.push_str(&format!(
                            "  {a} --[{}|{}]--> {b}  (source: {})\n",
                            e.relation, e.confidence, e.source_file
                        ));
                        break;
                    }
                }
            }
        }
        s
    }

    #[allow(dead_code)]
    pub fn affected(&self, id: &str) -> Vec<String> {
        self.neighbors(id)
    }

    #[allow(dead_code)]
    pub fn god_nodes(&self, k: usize) -> Vec<String> {
        let mut ds: Vec<(&String, usize)> = self.adj.iter().map(|(id, es)| (id, es.len())).collect();
        ds.sort_by_key(|&(_, n)| std::cmp::Reverse(n));
        let k = if k == 0 || k > ds.len() { ds.len() } else { k };
        ds.into_iter().take(k).map(|(id, _)| id.clone()).collect()
    }

    pub fn vocab(&self) -> Vec<String> {
        self.nodes
            .iter()
            .map(|n| if n.label.is_empty() { n.id.clone() } else { n.label.clone() })
            .collect()
    }

    /// Map a query onto <=12 tokens strictly present in the graph vocabulary.
    pub fn expand_vocab(&self, query: &str, max_tokens: usize) -> Vec<String> {
        let max_tokens = max_tokens.clamp(1, 12);
        let lq = query.to_lowercase();
        let mut scored: HashMap<String, i32> = HashMap::new();
        for tok in self.vocab() {
            let lt = tok.to_lowercase();
            for w in lq.split_whitespace() {
                if w.is_empty() {
                    continue;
                }
                if lt == w {
                    *scored.entry(tok.clone()).or_default() += 3;
                } else if lt.contains(w) || w.contains(&lt as &str) {
                    *scored.entry(tok.clone()).or_default() += 1;
                }
            }
        }
        let mut ranked: Vec<(String, i32)> = scored.into_iter().collect();
        ranked.sort_by_key(|&(_, v)| std::cmp::Reverse(v));
        ranked.into_iter().take(max_tokens).map(|(k, _)| k).collect()
    }
}

static WIKILINK_RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
fn wikilink_re() -> &'static regex::Regex {
    WIKILINK_RE.get_or_init(|| regex::Regex::new(r"\[\[([^\]]+)\]\]").unwrap())
}

/// Extract [[note]] targets from Markdown.
pub fn wikilinks(md: &str) -> Vec<String> {
    wikilink_re().captures_iter(md).map(|c| c[1].to_string()).collect()
}

/// Follow [[notes]] across mem_dir cycle-safe and budget-capped.
pub fn resolve_wikilinks(mem_dir: &Path, start: &str, budget: usize) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let mut seen = HashSet::new();
    let mut queue = VecDeque::from([start.to_string()]);
    while let Some(cur) = queue.pop_front() {
        if out.len() >= budget || !seen.insert(cur.clone()) {
            continue;
        }
        let mut p = mem_dir.join(&cur);
        if p.extension().is_none() {
            p.set_extension("md");
        }
        let Ok(body) = std::fs::read_to_string(&p) else { continue };
        for next in wikilinks(&body) {
            queue.push_back(next);
        }
        out.insert(cur, body);
    }
    out
}

/// Singleflight idle/session-close KB refresh per workspace, with a
/// per-workspace debounce so idle ticks and rapid turn completions collapse
/// into one refresh (mirrors `kb.idle_debounce_minutes`).
/// Clone shares the same inflight/debounce state (correct under axum's
/// per-request `State` clones).
#[derive(Clone)]
pub struct Sweeper {
    inner: std::sync::Arc<Inner>,
}

#[derive(Default)]
struct Inner {
    inflight: std::sync::Mutex<HashSet<String>>,
    last_run: std::sync::Mutex<HashMap<String, std::time::Instant>>,
}

impl Sweeper {
    pub fn new() -> Self {
        Self { inner: std::sync::Arc::new(Inner::default()) }
    }

    pub fn run(&self, workspace: &str, f: impl FnOnce() -> Result<(), String>) -> Result<(), String> {
        {
            let mut g = self.inner.inflight.lock().unwrap();
            if !g.insert(workspace.to_string()) {
                return Ok(()); // dedup: a sweep is already running
            }
        }
        let r = f();
        self.inner.inflight.lock().unwrap().remove(workspace);
        r
    }

    /// Run at most once per `debounce`; returns true when the pass executed.
    pub fn run_throttled(
        &self,
        workspace: &str,
        debounce: std::time::Duration,
        f: impl FnOnce() -> Result<(), String>,
    ) -> Result<bool, String> {
        {
            let last = self.inner.last_run.lock().unwrap();
            if let Some(t) = last.get(workspace) {
                if t.elapsed() < debounce {
                    return Ok(false);
                }
            }
        }
        let r = self.run(workspace, f);
        if r.is_ok() {
            self.inner.last_run.lock().unwrap().insert(workspace.to_string(), std::time::Instant::now());
        }
        r.map(|_| true)
    }
}

impl Default for Sweeper {
    fn default() -> Self {
        Self::new()
    }
}

/// Per-session KB override: `LUMINA_MEMORY_DIR` (colon-separated, workspace-
/// relative or absolute). Covers "each session might have different path"
/// without touching rule files.
pub fn kb_override() -> Option<String> {
    std::env::var("LUMINA_MEMORY_DIR").ok().filter(|s| !s.trim().is_empty())
}

/// Workspace rule texts (declare this workspace's KB): AGENTS.md /
/// CLAUDE.md / .lumina/rules/*.md. A mention here wins even before the
/// dir exists (`memory_write` creates it) — this is the per-workspace
/// "users config rule md files" source of truth.
fn workspace_rule_texts(workspace: &Path) -> Vec<String> {
    let mut out = Vec::new();
    for p in [
        workspace.join("AGENTS.md"),
        workspace.join("CLAUDE.md"),
    ] {
        if let Ok(b) = std::fs::read_to_string(&p) {
            out.push(b);
        }
    }
    if let Ok(rd) = std::fs::read_dir(workspace.join(".lumina/rules")) {
        for e in rd.flatten() {
            if e.path().extension().map(|x| x == "md").unwrap_or(false) {
                if let Ok(b) = std::fs::read_to_string(e.path()) {
                    out.push(b);
                }
            }
        }
    }
    out
}

/// Global rule texts (cross-device vocabulary only): they never create a
/// per-workspace KB by themselves — a global mention counts only when the
/// resolved dir already exists in the workspace.
fn global_rule_texts() -> Vec<String> {
    let mut out = Vec::new();
    let home = std::env::var("HOME").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("/root"));
    for p in [home.join(".config/lumina/AGENTS.md"), home.join(".claude/CLAUDE.md")] {
        if let Ok(b) = std::fs::read_to_string(&p) {
            out.push(b);
        }
    }
    if let Ok(rd) = std::fs::read_dir(home.join(".config/lumina/rules")) {
        for e in rd.flatten() {
            if e.path().extension().map(|x| x == "md").unwrap_or(false) {
                if let Ok(b) = std::fs::read_to_string(e.path()) {
                    out.push(b);
                }
            }
        }
    }
    out
}

static ASSIGN_RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
fn assign_re() -> &'static regex::Regex {
    ASSIGN_RE.get_or_init(|| {
        regex::Regex::new(r#"(?i)(?:memory_dir|kb_dir|knowledge_dir|kb_path|memory_path|knowledge_base)\s*[:=]\s*[`"']?([./\w\-:]+)[`"']?"#).unwrap()
    })
}

static PATH_RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
fn path_re() -> &'static regex::Regex {
    PATH_RE.get_or_init(|| {
        regex::Regex::new(r#"[`"']?((?:\./)?(?:[\w\-.]+/)+[\w\-.]*(?:\*\.md|\*|[\w\-.]+)?)[`"']?"#).unwrap()
    })
}

/// Extract workspace-relative KB dir candidates from one rule text.
/// Accepts explicit `memory_dir: docs/notes` assignments, keyword-named
/// paths (`./memory/`, `memory/*.md`, `docs/knowledge/`, `kb/`), and
/// notes-file paths on KB-context lines (`memory|knowledge|durable|
/// notes|wikilink|graphify-out|manifest` nearby — covers `docs/notes`-
/// style custom dirs). Source files (`*.rs`, `*.ts`, bare `src/`) are
/// never KB dirs. Returns cleaned dir strings; callers resolve them
/// against the workspace and filter escapes.
pub fn explicit_kb_dirs_from_text(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let push = |s: &str, out: &mut Vec<String>| {
        let mut c = s.trim().trim_matches(|c| c == '`' || c == '"' || c == '\'').trim().to_string();
        if c.is_empty() {
            return;
        }
        // Strip file globs / file names down to the containing dir.
        for suffix in ["/*.md", "/*", "*.md"] {
            if let Some(stripped) = c.strip_suffix(suffix) {
                c = stripped.to_string();
                break;
            }
        }
        if c.ends_with(".md") {
            if let Some(parent) = Path::new(&c).parent() {
                c = parent.to_string_lossy().into_owned();
            }
        }
        // `memory/graphify-out` is the graph dir — the KB dir is its parent.
        if let Some(stripped) = c.strip_suffix("/graphify-out") {
            c = stripped.to_string();
        }
        c = c.trim_start_matches("./").trim_end_matches('/').to_string();
        if c.is_empty() || c == "." {
            return;
        }
        // Safety: no escapes, no absolute-outside-workspace here (resolved later).
        if c.contains("..") || c.starts_with('/') {
            return;
        }
        // Tooling trees are never a workspace KB.
        if c.split('/').any(|seg| {
            seg.starts_with('.') || matches!(seg, "skills" | "plugins" | "scripts" | "harness")
        }) {
            return;
        }
        // Source files are never KB dirs (`src/kb.rs`, `web/app.ts`).
        // Notes dirs either carry a KB keyword or came from a `*.md`
        // path (already stripped to its parent above); a bare `src/`
        // or `web/` does not qualify.
        if let Some(last) = c.rsplit('/').next() {
            if last.contains('.') {
                return;
            }
        }
        if !out.contains(&c) {
            out.push(c);
        }
    };
    for cap in assign_re().captures_iter(text) {
        push(&cap[1], &mut out);
    }
    // Pass 1 (keyword-named paths anywhere): `memory` / `knowledge` /
    // whole-component `kb` in the path itself. `graphify` alone is the
    // graph tool, not a notes dir.
    for cap in path_re().captures_iter(text) {
        let cand = cap[1].trim_start_matches("./").trim_end_matches('/').to_string();
        if cand.split('/').any(|seg| seg.starts_with('.')) {
            continue;
        }
        let lc = cand.to_lowercase();
        let is_kb = lc.split('/').any(|seg| seg.contains("memory") || seg.contains("knowledge") || seg == "kb");
        if is_kb {
            push(&cand, &mut out);
        }
    }
    // Pass 2 (KB-context lines): a path sharing a line with a KB cue
    // counts when it is keyword-named or a notes file (`*.md` — covers
    // `Durable notes live in ./docs/notes/*.md`). Bare source dirs
    // (`src/`, `src/kb.rs`) on cue lines are still rejected. `kb` alone
    // is NOT a cue (it fires on every `kb.rs` / `kb_query` mention).
    const CUES: &[&str] = &[
        "memory", "knowledge", "durable", "notes", "wikilink", "graphify-out", "manifest", "knowledge base",
    ];
    for line in text.lines() {
        let ll = line.to_lowercase();
        if !CUES.iter().any(|c| ll.contains(c)) {
            continue;
        }
        for cap in path_re().captures_iter(line) {
            let raw = cap[1].to_string();
            let cand = raw.trim_start_matches("./").trim_end_matches('/').to_string();
            let lc = cand.to_lowercase();
            let keyword = lc.split('/').any(|seg| {
                seg.contains("memory") || seg.contains("knowledge") || seg.contains("notes") || seg == "kb"
            });
            let notes_file = raw.trim_end().ends_with(".md") || raw.contains("*.md");
            if keyword || notes_file {
                push(&raw, &mut out);
            }
        }
    }
    out
}

fn resolve_override_dirs(workspace: &Path, ov: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for part in ov.split(':') {
        let p = part.trim();
        if p.is_empty() || p.contains("..") {
            continue;
        }
        let cand = if Path::new(p).is_absolute() { PathBuf::from(p) } else { workspace.join(p) };
        if !out.contains(&cand) {
            out.push(cand);
        }
    }
    out
}

/// Resolve the workspace's knowledge-base dirs from (in order):
/// 1. explicit `override_dir` arg or `LUMINA_MEMORY_DIR` env,
/// 2. workspace rule-md mentions (AGENTS.md / CLAUDE.md / .lumina/rules/),
/// 3. global rule-md mentions, but only when the dir already exists,
/// 4. legacy `workspace/memory` when it already holds `.md` notes.
///
/// Returns an empty vec when no KB is configured — callers must then SKIP
/// KB use (never invent a dir, never dump unrelated files).
pub fn resolve_memory_dirs(workspace: &Path) -> Vec<PathBuf> {
    if let Some(ov) = kb_override() {
        let dirs = resolve_override_dirs(workspace, &ov);
        if !dirs.is_empty() {
            return dirs;
        }
    }
    let mut out: Vec<PathBuf> = Vec::new();
    // Workspace declarations win even before the dir exists (writes create it).
    for text in workspace_rule_texts(workspace) {
        for cand in explicit_kb_dirs_from_text(&text) {
            let p = workspace.join(&cand);
            if !out.contains(&p) {
                out.push(p);
            }
        }
    }
    if !out.is_empty() {
        return out;
    }
    // Global mentions are vocabulary, not workspace structure: only honor
    // them when the dir already holds notes here.
    for text in global_rule_texts() {
        for cand in explicit_kb_dirs_from_text(&text) {
            let p = workspace.join(&cand);
            if out.contains(&p) {
                continue;
            }
            if p.is_dir() {
                out.push(p);
            }
        }
    }
    if !out.is_empty() {
        return out;
    }
    // Backwards compat: the lumina repo itself (and long-standing
    // workspaces) declare `./memory/*.md` in AGENTS.md, but a workspace
    // without any rule mention still gets its existing memory/ dir so
    // `lumina kb query` keeps working where notes already live.
    let legacy = workspace.join("memory");
    if let Ok(rd) = std::fs::read_dir(&legacy) {
        let has_md = rd.flatten().any(|e| e.path().extension().map(|x| x == "md").unwrap_or(false));
        if has_md {
            return vec![legacy];
        }
    }
    Vec::new()
}

/// First resolved KB dir, or legacy `workspace/memory` (for writes/CLI).
pub fn primary_memory_dir(workspace: &Path) -> PathBuf {
    resolve_memory_dirs(workspace).into_iter().next().unwrap_or_else(|| workspace.join("memory"))
}

/// True when this workspace declares (or already holds) a knowledge base.
pub fn has_kb(workspace: &Path) -> bool {
    !resolve_memory_dirs(workspace).is_empty()
}

/// Workspace-relative prefix for manifest entries (`memory`, `docs/notes`).
fn mem_rel(workspace: &Path, mem_dir: &Path) -> String {
    match mem_dir.strip_prefix(workspace) {
        Ok(r) if !r.as_os_str().is_empty() => r.to_string_lossy().replace('\\', "/"),
        _ => "memory".to_string(),
    }
}

/// Best-effort deterministic `graphify reflect` for one memory dir:
/// aggregates `graphify-out/memory/` outcomes into
/// `graphify-out/reflections/LESSONS.md`. No LLM, no network; all
/// failures are swallowed (missing binary, missing outcomes dir).
pub fn try_graphify_reflect(mem_dir: &Path) {
    let out = mem_dir.join("graphify-out/reflections/LESSONS.md");
    let mem_outcomes = mem_dir.join("graphify-out/memory");
    if !mem_outcomes.is_dir() {
        return;
    }
    if let Some(d) = out.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let _ = std::process::Command::new("graphify")
        .args(["reflect", "--memory-dir", &mem_outcomes.display().to_string(), "--out", &out.display().to_string()])
        .output();
}

/// Re-stamp every resolved KB dir's `graphify-out/manifest.json` from its
/// current `*.md` notes (cheap freshness anchor for the standing-ops pass).
pub fn refresh_memory_manifest(workspace: &Path) -> Result<bool, KbError> {
    let dirs = resolve_memory_dirs(workspace);
    if dirs.is_empty() {
        return Ok(false);
    }
    for dir in &dirs {
        let Ok(rd) = std::fs::read_dir(dir) else { continue };
        let prefix = mem_rel(workspace, dir);
        let files: Vec<String> = rd
            .flatten()
            .filter(|e| e.path().extension().map(|x| x == "md").unwrap_or(false))
            .map(|e| format!("{prefix}/{}", e.file_name().to_string_lossy()))
            .collect();
        manifest_stamp(workspace, &dir.join("graphify-out"), &files)?;
        try_graphify_reflect(dir);
    }
    Ok(true)
}

/// Write the canonical root-relative manifest.json.
pub fn manifest_stamp(workspace: &Path, out_dir: &Path, files: &[String]) -> Result<(), KbError> {
    let mut rel: Vec<String> = files
        .iter()
        .map(|f| {
            pathdiff_rel(workspace, Path::new(f))
        })
        .collect();
    rel.sort();
    std::fs::create_dir_all(out_dir)?;
    let doc = serde_json::json!({"root": ".", "files": rel});
    std::fs::write(out_dir.join("manifest.json"), serde_json::to_string_pretty(&doc)?)?;
    Ok(())
}

fn pathdiff_rel(base: &Path, p: &Path) -> String {
    match p.strip_prefix(base) {
        Ok(r) => r.to_string_lossy().replace('\\', "/"),
        Err(_) => p.to_string_lossy().into_owned(),
    }
}

/// Cold-start reflect: LESSONS.md + MEMORY.md from each resolved KB dir.
/// Empty when no KB is configured (callers then skip, never invent).
pub fn reflect(workspace: &Path) -> String {
    reflect_dirs(workspace, &resolve_memory_dirs(workspace))
}

/// Same as [`reflect`] but over an explicit dir list (override path).
fn reflect_dirs(workspace: &Path, dirs: &[PathBuf]) -> String {
    let mut out = String::new();
    for dir in dirs {
        let prefix = mem_rel(workspace, dir);
        for rel in ["graphify-out/reflections/LESSONS.md", "MEMORY.md"] {
            let p = dir.join(rel);
            if let Ok(b) = std::fs::read_to_string(&p) {
                out.push_str(&format!("\n--- {prefix}/{rel} ---\n{b}\n"));
            }
        }
    }
    // Legacy root MEMORY.md (pre-resolution workspaces).
    if out.is_empty() {
        let p = workspace.join("MEMORY.md");
        if let Ok(b) = std::fs::read_to_string(&p) {
            out.push_str(&format!("\n--- MEMORY.md ---\n{b}\n"));
        }
    }
    if out.is_empty() {
        out.push_str("(no knowledge base configured for this workspace — no memory/knowledge dir mentioned in AGENTS.md/rules and no memory/*.md found; skipping KB)\n");
    }
    out
}

/// Four-phase retrieval with clarify-before-KB and rule-based dirs:
/// vague queries and KB-less workspaces return guidance instead of a
/// misleading dump. Returns budgeted context text for the agent.
pub fn query(workspace: &Path, q: &str, budget: usize) -> Result<String, KbError> {
    query_in(workspace, q, budget, None)
}

/// Override-aware query: `override_dir` (colon-separated) wins over
/// `LUMINA_MEMORY_DIR` env and rule-md resolution. Empty override falls
/// back to the standard chain.
pub fn query_in(
    workspace: &Path,
    q: &str,
    budget: usize,
    override_dir: Option<&str>,
) -> Result<String, KbError> {
    let budget = if budget == 0 { 4000 } else { budget };
    // Clarify-before-KB: a query with no signal means intent is unknown.
    // Tell the agent to ask first instead of burning a full-corpus scan.
    if q.trim().len() < 3 {
        return Ok("kb: query too vague (empty or <3 chars) — clarify user intent first (use the `question` tool for ambiguous requests), then re-query with 2-6 request keywords, budget 4000.".into());
    }
    let dirs: Vec<PathBuf> = match override_dir.filter(|s| !s.trim().is_empty()) {
        Some(ov) => resolve_override_dirs(workspace, ov),
        None => resolve_memory_dirs(workspace),
    };
    if dirs.is_empty() {
        let mut out = String::from("## reflect\n");
        out.push_str(&reflect_dirs(workspace, &[]));
        out.push_str("\nkb: no knowledge base configured for this workspace (no memory/knowledge dir mentioned in AGENTS.md/rules and no memory/*.md found) — skipping KB; proceed with code/tools, and do NOT call kb_query again this turn (re-querying returns this same skip note).\n");
        return Ok(out);
    }
    let lq_words: Vec<String> = q.to_lowercase().split_whitespace().map(|w| w.to_string()).collect();
    let mut out = String::from("## reflect\n");
    out.push_str(&reflect_dirs(workspace, &dirs));
    let mut graph_paths: Vec<PathBuf> = Vec::new();
    for dir in &dirs {
        graph_paths.push(dir.join("graphify-out/graph.json"));
    }
    // Workspace code graph (AST, 0 LLM tokens) alongside memory graphs.
    graph_paths.push(workspace.join("graphify-out/graph.json"));
    let mut seeds: Vec<String> = Vec::new();
    for gp in &graph_paths {
        if !gp.exists() {
            continue;
        }
        let g = Graph::load(gp)?;
        let toks = g.expand_vocab(q, 12);
        out.push_str(&format!("\n## graph {} — seeds: {}\n", gp.display(), toks.join(", ")));
        for t in &toks {
            // Map label back to node id for traversal.
            let id = g.nodes.iter().find(|n| n.label == *t || n.id == *t).map(|n| n.id.clone()).unwrap_or_else(|| t.clone());
            let mut ctx = format!("### {t}\n");
            for n in g.bfs(&id, 12) {
                ctx.push_str(&format!("- {n}\n"));
            }
            if out.len() + ctx.len() > budget {
                break;
            }
            out.push_str(&ctx);
            seeds.push(t.clone());
        }
        if out.len() >= budget {
            break;
        }
    }
    // Recursive [[wikilink]] resolution from graph seeds, plus fallbacks
    // when the graph is missing or silent (note filenames matching query
    // words + every [[link]] in MEMORY.md — the index is only useful if
    // something follows its pointers). Resolved per-KB-dir, not hardcoded.
    let mut fb: Vec<String> = Vec::new();
    let mut fb_dir: PathBuf = dirs[0].clone();
    if seeds.is_empty() {
        for dir in &dirs {
            if let Ok(rd) = std::fs::read_dir(dir) {
                let mut names: Vec<String> = rd
                    .flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect();
                names.sort();
                for n in names {
                    if !n.ends_with(".md") {
                        continue;
                    }
                    let stem = n.trim_end_matches(".md").to_lowercase();
                    if stem == "memory" || stem.is_empty() {
                        continue;
                    }
                    if lq_words.iter().any(|w| {
                        let w = w.as_str();
                        !w.is_empty() && (stem.contains(w) || w.contains(&stem))
                    }) {
                        fb.push(stem.clone());
                        fb_dir = dir.clone();
                    }
                    if fb.len() >= 6 {
                        break;
                    }
                }
            }
            if fb.len() >= 6 {
                fb_dir = dir.clone();
                break;
            }
            if let Ok(body) = std::fs::read_to_string(dir.join("MEMORY.md")) {
                for link in wikilinks(&body) {
                    if fb.len() >= 8 {
                        break;
                    }
                    if !fb.contains(&link) {
                        fb.push(link);
                    }
                }
            }
            if !fb.is_empty() {
                fb_dir = dir.clone();
                break;
            }
        }
    }
    let all_seeds: Vec<String> = seeds.iter().cloned().chain(fb).take(8).collect();
    // Graph seeds resolve against the first KB dir; filename fallbacks
    // against the dir that produced them.
    let seed_dir = if seeds.is_empty() { fb_dir } else { dirs[0].clone() };
    for s in all_seeds.iter().take(6) {
        for (name, body) in resolve_wikilinks(&seed_dir, s, 6) {
            let chunk = format!("\n### [[{name}]]\n{body}\n");
            if out.len() + chunk.len() > budget + 2000 {
                break;
            }
            out.push_str(&chunk);
        }
        if out.len() > budget + 2000 {
            break;
        }
    }
    Ok(out)
}

static SECRET_RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
fn secret_re() -> &'static regex::Regex {
    SECRET_RE.get_or_init(|| {
        regex::Regex::new(r"sk-[A-Za-z0-9_-]{8,}|AKIA[0-9A-Z]{16}|ghp_[A-Za-z0-9]{8,}|xox[bap]-[A-Za-z0-9-]+").unwrap()
    })
}

/// Dedup-on-write memory synthesis into the rule-declared KB dir
/// (2-layer guard: shape deny + MEMORY.md pointer guidance).
/// Writes are post-completion only (see system prompt); mid-turn
/// speculation is a caller bug. Refuses live-secret-shaped content.
pub fn memory_write(workspace: &Path, topic: &str, content: &str) -> Result<String, KbError> {
    memory_write_in(workspace, topic, content, None)
}

/// Override-aware write: `override_dir` wins over env + rule resolution.
/// Explicit CLI use falls back to `workspace/memory` so `kb update` works
/// even before any rule declares a KB; autonomous agent writes should only
/// run when `has_kb` (the system prompt enforces this).
pub fn memory_write_in(
    workspace: &Path,
    topic: &str,
    content: &str,
    override_dir: Option<&str>,
) -> Result<String, KbError> {
    if topic.trim().is_empty() || content.trim().is_empty() {
        return Err(KbError::Msg("memory_write: missing topic/content".into()));
    }
    if secret_re().is_match(content) {
        return Err(KbError::Msg(
            "memory_write: content looks like a live secret — store a ${placeholder} pointer, never the value".into(),
        ));
    }
    let dir = match override_dir.filter(|s| !s.trim().is_empty()) {
        Some(ov) => resolve_override_dirs(workspace, ov).into_iter().next().unwrap_or_else(|| workspace.join("memory")),
        None => primary_memory_dir(workspace),
    };
    std::fs::create_dir_all(&dir)?;
    // Base prevents traversal; memory writes stay inside ./memory.
    let mut name = match PathBuf::from(topic).file_name() {
        Some(n) => PathBuf::from(n),
        None => return Err(KbError::Msg("memory_write: bad topic".into())),
    };
    if name.as_os_str().is_empty() {
        return Err(KbError::Msg("memory_write: bad topic".into()));
    }
    name.set_extension("md");
    let p = dir.join(&name);
    let existing = std::fs::read_to_string(&p).unwrap_or_default();
    if existing.contains(content) {
        return Ok(format!("dedup: already present in {}", p.display()));
    }
    use std::fmt::Write as _;
    let mut f = String::new();
    if !existing.is_empty() {
        f.push_str(&existing);
        if !f.ends_with('\n') {
            f.push('\n');
        }
    } else {
        let _ = write!(
            f,
            "---\ndate_modified: {}\ntags: [lumina-memory]\n---\n\n",
            chrono::Utc::now().format("%Y-%m-%d")
        );
    }
    f.push_str(content);
    if !content.ends_with('\n') {
        f.push('\n');
    }
    std::fs::write(&p, f)?;
    // Keep MEMORY.md index pointing at the topic.
    let idx = dir.join("MEMORY.md");
    let entry = format!("- [[{}]]", name.to_string_lossy().trim_end_matches(".md"));
    let cur = std::fs::read_to_string(&idx).unwrap_or_default();
    if !cur.contains(&entry) {
        let mut c = cur;
        if !c.is_empty() && !c.ends_with('\n') {
            c.push('\n');
        }
        c.push_str(&entry);
        c.push('\n');
        std::fs::write(&idx, c)?;
    }
    // Keep the graphify manifest fresh so kb queries never go stale,
    // then best-effort deterministic reflect (LESSONS.md aggregation).
    if let Ok(rd) = std::fs::read_dir(&dir) {
        let prefix = mem_rel(workspace, &dir);
        let files: Vec<String> = rd
            .flatten()
            .filter(|e| e.path().extension().map(|x| x == "md").unwrap_or(false))
            .map(|e| format!("{prefix}/{}", e.file_name().to_string_lossy()))
            .collect();
        let _ = manifest_stamp(workspace, &dir.join("graphify-out"), &files);
    }
    try_graphify_reflect(&dir);
    Ok(format!("wrote {}", p.display()))
}



#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Graph {
        let mut g = Graph {
            nodes: vec![
                Node { id: "a".into(), label: "auth".into(), r#type: String::new(), file: String::new() },
                Node { id: "b".into(), label: "broker".into(), r#type: String::new(), file: String::new() },
                Node { id: "c".into(), label: "cache".into(), r#type: String::new(), file: String::new() },
            ],
            edges: vec![
                Edge { nodes: vec!["a".into(), "b".into()], relation: "uses".into(), confidence: "EXTRACTED".into(), source_file: "x".into() },
                Edge { nodes: vec!["b".into(), "c".into()], relation: "fills".into(), confidence: String::new(), source_file: String::new() },
            ],
            adj: HashMap::new(),
        };
        g.rebuild();
        // Normalize the empty confidence like load() does.
        for e in &mut g.edges {
            if e.confidence.is_empty() {
                e.confidence = "INFERRED".into();
            }
        }
        g
    }

    #[test]
    fn traversal_shapes() {
        let g = sample();
        assert_eq!(g.bfs("a", 10), vec!["a", "b", "c"]);
        assert_eq!(g.dfs("a", 10), vec!["a", "b", "c"]);
        assert_eq!(g.shortest_path("a", "c"), Some(vec!["a".into(), "b".into(), "c".into()]));
        assert!(g.explain("a", "c").contains("broker") || g.explain("a", "c").contains('b'));
        assert_eq!(g.affected("b").len(), 2);
        assert_eq!(g.god_nodes(1), vec!["b".to_string()]);
        assert!(g.expand_vocab("auth broker question", 12).contains(&"auth".to_string()));
    }

    #[test]
    fn wikilinks_cycle_safe() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.md"), "see [[b]]").unwrap();
        std::fs::write(d.path().join("b.md"), "see [[a]]").unwrap();
        let out = resolve_wikilinks(d.path(), "a", 10);
        assert_eq!(out.len(), 2);
        assert_eq!(wikilinks("x [[y]] z"), vec!["y".to_string()]);
    }

    #[test]
    fn memory_write_dedups_and_blocks_secrets() {
        let d = tempfile::tempdir().unwrap();
        let ok = memory_write(d.path(), "topic", "durable fact [[other]]").unwrap();
        assert!(ok.starts_with("wrote"));
        let dup = memory_write(d.path(), "topic", "durable fact [[other]]").unwrap();
        assert!(dup.starts_with("dedup"));
        assert!(memory_write(d.path(), "t", "key sk-abc123xyz789zzz").is_err());
        // Traversal stays inside memory/.
        assert!(!d.path().join("..").join("escaped.md").exists());
        assert!(memory_write(d.path(), "../escaped", "x").unwrap().contains("memory"));
    }
}

#[cfg(test)]
mod sweeper_tests {
    use super::*;

    #[test]
    fn singleflight_dedups() {
        let s = Sweeper::new();
        let ran = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let r2 = ran.clone();
        // Second Run while the first is inflight is a no-op.
        let s2 = Sweeper::new();
        s2.inner.inflight.lock().unwrap().insert("ws".to_string());
        assert!(s2.run("ws", || { r2.fetch_add(1, std::sync::atomic::Ordering::SeqCst); Ok(()) }).is_ok());
        assert_eq!(r2.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert!(s.run("ws", || { ran.fetch_add(1, std::sync::atomic::Ordering::SeqCst); Ok(()) }).is_ok());
        assert_eq!(ran.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
}

#[cfg(test)]
mod fallback_tests {
    use super::*;

    #[test]
    fn no_graph_still_reads_notes() {
        let d = tempfile::tempdir().unwrap();
        let ws = d.path();
        std::fs::create_dir_all(ws.join("memory")).unwrap();
        std::fs::write(ws.join("memory/MEMORY.md"), "# idx\n\n- [[fruit]]\n").unwrap();
        std::fs::write(ws.join("memory/fruit.md"), "the magic fruit is kiwi\n").unwrap();
        // No graphify-out anywhere: seeds must come from MEMORY.md links.
        let out = query(ws, "magic fruit", 4000).unwrap();
        assert!(out.contains("kiwi"), "{out}");
        assert!(out.contains("[[fruit]]"), "{out}");
    }

    #[test]
    fn vague_query_asks_to_clarify_first() {
        let d = tempfile::tempdir().unwrap();
        for q in ["", "  ", "hi"] {
            let out = query(d.path(), q, 4000).unwrap();
            assert!(out.contains("clarify"), "{q} -> {out}");
        }
    }

    #[test]
    fn no_kb_dir_skips_with_guidance() {
        let d = tempfile::tempdir().unwrap();
        // No AGENTS.md mention and no memory/*.md: KB-less workspace.
        let out = query(d.path(), "how does auth work", 4000).unwrap();
        assert!(out.contains("no knowledge base configured"), "{out}");
        assert!(out.contains("do NOT call kb_query again"), "{out}");
        assert!(!has_kb(d.path()));
    }

    #[test]
    fn rule_declared_custom_dir_is_used() {
        let d = tempfile::tempdir().unwrap();
        let ws = d.path();
        std::fs::write(ws.join("AGENTS.md"), "# rules\n\nDurable notes live in ./docs/notes/*.md\n").unwrap();
        std::fs::create_dir_all(ws.join("docs/notes")).unwrap();
        std::fs::write(ws.join("docs/notes/MEMORY.md"), "# idx\n\n- [[apple]]\n").unwrap();
        std::fs::write(ws.join("docs/notes/apple.md"), "the custom fruit is mango\n").unwrap();
        assert!(has_kb(ws));
        assert_eq!(resolve_memory_dirs(ws), vec![ws.join("docs/notes")]);
        let out = query(ws, "custom fruit mango", 4000).unwrap();
        assert!(out.contains("mango"), "{out}");
        let w = memory_write(ws, "banana", "yellow [[apple]]").unwrap();
        assert!(w.contains("docs/notes/banana.md"), "{w}");
        assert!(ws.join("docs/notes/banana.md").exists());
        // Manifest is workspace-relative to the custom dir.
        let man = std::fs::read_to_string(ws.join("docs/notes/graphify-out/manifest.json")).unwrap();
        assert!(man.contains("docs/notes/banana.md"), "{man}");
    }

    #[test]
    fn override_dir_wins_over_rules() {
        let d = tempfile::tempdir().unwrap();
        let ws = d.path();
        std::fs::write(ws.join("AGENTS.md"), "notes in ./memory/*.md\n").unwrap();
        std::fs::create_dir_all(ws.join("custom")).unwrap();
        std::fs::write(ws.join("custom/MEMORY.md"), "- [[z]]\n").unwrap();
        let out = query_in(ws, "zebra zone", 4000, Some("custom")).unwrap();
        assert!(!out.contains("no knowledge base"), "{out}");
        let w = memory_write_in(ws, "t", "body [[z]]", Some("custom")).unwrap();
        assert!(w.contains("custom/t.md"), "{w}");
    }

    #[test]
    fn explicit_dirs_parse() {
        assert_eq!(explicit_kb_dirs_from_text("memory_dir: docs/notes"), vec!["docs/notes".to_string()]);
        let v = explicit_kb_dirs_from_text("Durable notes live in ./memory/*.md");
        assert!(v.contains(&"memory".to_string()), "{v:?}");
        let v2 = explicit_kb_dirs_from_text("knowledge base: ./kb/");
        assert!(v2.iter().any(|s| s.contains("kb")), "{v2:?}");
        // Escapes are dropped.
        assert!(explicit_kb_dirs_from_text("see ../outside/memory/x").is_empty());
    }
}
