//! Rule discovery: hierarchical AGENTS.md + instructions/skills/agents/commands.
//! Mirrors `internal/rules/rules.go` (M0: discovery + merge order only).

use std::path::{Path, PathBuf};

/// Merge order layers (lowest wins instructionally; Layer 0 core lives in `security`).
#[derive(Debug, Clone)]
pub struct RuleSet {
    /// (source_path, body) in merge order.
    pub layers: Vec<(PathBuf, String)>,
}

pub fn workspace_root(start: &Path) -> PathBuf {
    let mut cur = start.to_path_buf();
    loop {
        if cur.join(".git").exists() || cur.join("AGENTS.md").exists() {
            return cur;
        }
        match cur.parent() {
            Some(p) => cur = p.to_path_buf(),
            None => return start.to_path_buf(),
        }
    }
}

/// Collect rule files without reading global imports twice; skips missing files.
pub fn discover(workspace: &Path, global: bool) -> RuleSet {
    let mut layers = Vec::new();
    let mut push = |p: PathBuf| {
        if let Ok(body) = std::fs::read_to_string(&p) {
            layers.push((p, body));
        }
    };
    if global {
        if let Ok(home) = std::env::var("HOME") {
            let h = PathBuf::from(home);
            push(h.join(".config/lumina/AGENTS.md"));
            let inst = h.join(".config/lumina/instructions");
            if let Ok(rd) = std::fs::read_dir(&inst) {
                let mut v: Vec<_> = rd.flatten().map(|e| e.path()).collect();
                v.sort();
                for p in v {
                    if p.extension().map(|e| e == "md").unwrap_or(false) {
                        push(p);
                    }
                }
            }
        }
    }
    // Project + directory walk: git root down to workspace dir.
    let root = workspace_root(workspace);
    let mut chain = Vec::new();
    let mut cur = workspace.to_path_buf();
    loop {
        chain.push(cur.clone());
        if cur == root || cur.parent().is_none() {
            break;
        }
        cur = cur.parent().unwrap().to_path_buf();
    }
    chain.reverse();
    for dir in chain {
        let a = dir.join("AGENTS.md");
        if a.exists() {
            push(a);
        } else {
            let c = dir.join("CLAUDE.md"); // fallback when AGENTS.md absent
            if c.exists() {
                push(c);
            }
        }
        let inst = dir.join(".lumina/instructions");
        if let Ok(rd) = std::fs::read_dir(&inst) {
            let mut v: Vec<_> = rd.flatten().map(|e| e.path()).collect();
            v.sort();
            for p in v {
                if p.extension().map(|e| e == "md").unwrap_or(false) {
                    push(p);
                }
            }
        }
    }
    RuleSet { layers }
}
