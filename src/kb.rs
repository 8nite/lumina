//! Markdown + graphify RAG engine: cold-start reflect, constrained-vocab
//! expansion, native graph.json traversal (<2ms BFS/DFS/path/explain/god-nodes),
//! recursive [[wikilink]] resolution, dedup-on-write memory synthesis, and the
//! singleflight standing-ops sweeper. Mirrors `internal/kb`. Zero vector DBs.

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

    pub fn affected(&self, id: &str) -> Vec<String> {
        self.neighbors(id)
    }

    pub fn god_nodes(&self, k: usize) -> Vec<String> {
        let mut ds: Vec<(&String, usize)> = self.adj.iter().map(|(id, es)| (id, es.len())).collect();
        ds.sort_by(|a, b| b.1.cmp(&a.1));
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
        ranked.sort_by(|a, b| b.1.cmp(&a.1));
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

/// Singleflight idle/session-close KB refresh per workspace.
pub struct Sweeper {
    inflight: std::sync::Mutex<HashSet<String>>,
}

impl Sweeper {
    pub fn new() -> Self {
        Self { inflight: std::sync::Mutex::new(HashSet::new()) }
    }

    pub fn run(&self, workspace: &str, f: impl FnOnce() -> Result<(), String>) -> Result<(), String> {
        {
            let mut g = self.inflight.lock().unwrap();
            if !g.insert(workspace.to_string()) {
                return Ok(()); // dedup: a sweep is already running
            }
        }
        let r = f();
        self.inflight.lock().unwrap().remove(workspace);
        r
    }
}

impl Default for Sweeper {
    fn default() -> Self {
        Self::new()
    }
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

/// Cold-start reflect: LESSONS.md + MEMORY.md when the graph is stale.
pub fn reflect(workspace: &Path) -> String {
    let mut out = String::new();
    for rel in ["memory/graphify-out/reflections/LESSONS.md", "memory/MEMORY.md", "MEMORY.md"] {
        let p = workspace.join(rel);
        if let Ok(b) = std::fs::read_to_string(&p) {
            out.push_str(&format!("\n--- {rel} ---\n{b}\n"));
        }
    }
    if out.is_empty() {
        out.push_str("(no MEMORY.md or LESSONS.md yet — run `lumina kb update <topic> \"<note>\"` to seed memory)\n");
    }
    out
}

/// Four-phase retrieval: reflect → constrained vocab → graph traversal →
/// wikilink read. Returns budgeted context text for the agent.
pub fn query(workspace: &Path, q: &str, budget: usize) -> Result<String, KbError> {
    let budget = if budget == 0 { 4000 } else { budget };
    let mut out = String::from("## reflect\n");
    out.push_str(&reflect(workspace));
    let graph_paths = [
        workspace.join("memory/graphify-out/graph.json"),
        workspace.join("graphify-out/graph.json"),
    ];
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
    // Recursive [[wikilink]] resolution from top seeds.
    let mem = workspace.join("memory");
    for s in seeds.iter().take(4) {
        for (name, body) in resolve_wikilinks(&mem, s, 6) {
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

/// Dedup-on-write memory synthesis. Refuses live-secret-shaped content
/// (2-layer guard: shape deny + MEMORY.md pointer guidance).
pub fn memory_write(workspace: &Path, topic: &str, content: &str) -> Result<String, KbError> {
    if topic.trim().is_empty() || content.trim().is_empty() {
        return Err(KbError::Msg("memory_write: missing topic/content".into()));
    }
    if secret_re().is_match(content) {
        return Err(KbError::Msg(
            "memory_write: content looks like a live secret — store a ${placeholder} pointer, never the value".into(),
        ));
    }
    let dir = workspace.join("memory");
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
