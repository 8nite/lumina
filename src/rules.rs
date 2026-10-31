//! Rule discovery: hierarchical AGENTS.md + instructions/skills/agents/commands.
//! Mirrors `internal/rules/rules.go` (M0: discovery + merge order only).

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

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

pub const IMMUTABLE_SECURITY_CORE: &str = "# Lumina Immutable Security Core (Layer 0)\n- Workspace fence: agents may only touch the session workspace_dir (+ TMPDIR).\n- Credential files (.env, auth.json, lumina.json, *secret*, *credential*, *.pem) are HARD DENY in every mode.\n- Google services (googleapis.com, generativelanguage, gemini, google-workspace) are HARD DENY.\n- plan-mode agents are read-only except plan.md / .lumina/plans/*.md.\n";

/// One discovered skill (only name+description ship in the tool list;
/// the full SKILL.md loads lazily via the `skill` tool).
#[derive(Debug, Clone)]
pub struct SkillMeta {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct AgentDef {
    pub name: String,
    pub description: String,
    pub mode: String,
    pub model: String,
    pub path: String,
    pub body: String,
}

#[derive(Debug, Clone)]
pub struct CommandDef {
    pub name: String,
    pub path: PathBuf,
    pub body: String,
}

/// Merged Layer 0..3 instructions + discovered extensions.
#[derive(Debug, Clone, Default)]
pub struct Bundle {
    pub system_prompt: String,
    pub layers: Vec<String>,
    pub skills: Vec<SkillMeta>,
    pub agents: Vec<AgentDef>,
    pub commands: Vec<CommandDef>,
}

fn home_dir() -> PathBuf {
    std::env::var("HOME").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("/root"))
}

fn read_if_exists(p: &Path) -> String {
    std::fs::read_to_string(p).unwrap_or_default()
}

fn file_exists(p: &Path) -> bool {
    p.is_file()
}

fn glob_files(pattern: &str) -> Vec<PathBuf> {
    glob::glob(pattern).map(|it| it.flatten().collect()).unwrap_or_default()
}

pub fn parse_frontmatter(content: &str) -> (HashMap<String, String>, String) {
    let mut meta = HashMap::new();
    if !content.starts_with("---") {
        return (meta, content.to_string());
    }
    let Some(end) = content[3..].find("\n---") else { return (meta, content.to_string()) };
    let fm = &content[3..3 + end];
    let rest = content[3 + end + 4..].trim_start_matches('\n').to_string();
    for line in fm.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once(':') else { continue };
        meta.insert(k.trim().to_string(), v.trim().trim_matches(|c| c == '"' || c == '\'').to_string());
    }
    (meta, rest)
}

fn discover_skills_from_roots(roots: &[PathBuf]) -> Vec<SkillMeta> {
    let mut out = Vec::new();
    for root in roots {
        let Ok(rd) = std::fs::read_dir(root) else { continue };
        for e in rd.flatten() {
            if !e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let p = e.path().join("SKILL.md");
            let content = read_if_exists(&p);
            if content.is_empty() {
                continue;
            }
            let (meta, _) = parse_frontmatter(&content);
            let name = meta.get("name").cloned().unwrap_or_else(|| e.file_name().to_string_lossy().into_owned());
            out.push(SkillMeta { name, description: meta.get("description").cloned().unwrap_or_default(), path: p });
        }
    }
    out
}

pub fn builtin_agents() -> Vec<AgentDef> {
    vec![
        AgentDef {
            name: "general".into(),
            description: "Handles research and multi-step work with broad tool access, but cannot launch subagents.".into(),
            mode: "subagent".into(),
            model: String::new(),
            path: String::new(),
            body: "You are a general-purpose subagent. Research the workspace, run safe diagnostics, and complete multi-step tasks. Prefer kb_query and grep before broad edits. Summarize what you did with file references.".into(),
        },
        AgentDef {
            name: "explore".into(),
            description: "Searches and reads code or web sources without editing files.".into(),
            mode: "subagent".into(),
            model: String::new(),
            path: String::new(),
            body: "You are an explore subagent. Search and read code (read, glob, grep), memory (kb_query, skill), and the web (webfetch, websearch). Do not edit files. Report findings with file and line references.".into(),
        },
    ]
}

fn discover_agents(workspace: &Path) -> Vec<AgentDef> {
    let mut roots = vec![home_dir().join(".config/lumina/agents")];
    if !workspace.as_os_str().is_empty() {
        roots.push(workspace.join(".lumina/agents"));
    }
    let mut out = Vec::new();
    for root in roots {
        let pat = root.join("*.md");
        for f in glob_files(&pat.to_string_lossy()) {
            let content = read_if_exists(&f);
            let (meta, body) = parse_frontmatter(&content);
            let name = meta.get("name").cloned().unwrap_or_else(|| {
                f.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
            });
            out.push(AgentDef {
                name,
                description: meta.get("description").cloned().unwrap_or_default(),
                mode: meta.get("mode").cloned().unwrap_or_else(|| "subagent".into()),
                model: meta.get("model").cloned().unwrap_or_default(),
                path: f.display().to_string(),
                body,
            });
        }
    }
    out
}

fn discover_commands(workspace: &Path) -> Vec<CommandDef> {
    let mut roots = vec![home_dir().join(".config/lumina/commands")];
    if !workspace.as_os_str().is_empty() {
        roots.push(workspace.join(".lumina/commands"));
    }
    let mut out = Vec::new();
    for root in roots {
        let pat = root.join("*.md");
        for f in glob_files(&pat.to_string_lossy()) {
            out.push(CommandDef {
                name: f.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(),
                path: f.clone(),
                body: read_if_exists(&f),
            });
        }
    }
    out
}

fn dir_chain(root: &Path, dir: &Path) -> Vec<PathBuf> {
    let mut chain = Vec::new();
    let mut cur = dir.to_path_buf();
    loop {
        chain.push(cur.clone());
        if cur == root || cur.parent().is_none() {
            break;
        }
        cur = cur.parent().unwrap().to_path_buf();
    }
    chain.reverse();
    chain
}

impl Bundle {
    /// Merge Layer 0..3 + discover skills/agents/commands (mirrors Go `Load`).
    pub fn load(workspace: &Path, import_global_rules: bool, import_global_skills: bool) -> Self {
        let mut b = Self::default();
        let mut prompt = String::from(IMMUTABLE_SECURITY_CORE);
        prompt.push('\n');
        let mut add_layer = |path: String, content: String| {
            if content.trim().is_empty() {
                return;
            }
            b.layers.push(path.clone());
            prompt.push_str(&format!("\n--- layer: {path} ---\n{content}"));
            if !content.ends_with('\n') {
                prompt.push('\n');
            }
        };

        if import_global_rules {
            let gp = home_dir().join(".config/lumina/AGENTS.md");
            let c = read_if_exists(&gp);
            add_layer(gp.display().to_string(), c);
            for f in glob_files(&home_dir().join(".config/lumina/instructions/*.md").to_string_lossy()) {
                let c = read_if_exists(&f);
                add_layer(f.display().to_string(), c);
            }
            let cl = home_dir().join(".claude/CLAUDE.md");
            let c = read_if_exists(&cl);
            add_layer(cl.display().to_string(), c);
        }

        if !workspace.as_os_str().is_empty() {
            let root = workspace_root(workspace);
            for d in dir_chain(&root, workspace) {
                let a = d.join("AGENTS.md");
                if file_exists(&a) {
                    let c = read_if_exists(&a);
                    add_layer(a.display().to_string(), c);
                } else if d == root {
                    let c = d.join("CLAUDE.md");
                    if file_exists(&c) {
                        let b2 = read_if_exists(&c);
                        add_layer(c.display().to_string(), b2);
                    }
                }
            }
            for f in glob_files(&workspace.join(".lumina/instructions/*.md").to_string_lossy()) {
                let c = read_if_exists(&f);
                add_layer(f.display().to_string(), c);
            }
            if import_global_skills {
                b.skills.extend(discover_skills_from_roots(&[home_dir().join(".config/lumina/skills")]));
            }
            b.skills.extend(discover_skills_from_roots(&[
                workspace.join(".lumina/skills"),
                workspace.join(".lumina/harness/skills"),
            ]));
            let mut agents = discover_agents(workspace);
            agents.extend(builtin_agents());
            b.agents = agents;
            b.commands = discover_commands(workspace);
            // Layer 3 — Harness² overlay (repo root, then workspace when different).
            for base in [root.clone(), workspace.to_path_buf()] {
                if base == root || base == *workspace {
                    for f in ["system_prompt.md", "tool_guidance.md"] {
                        let p = base.join(".lumina/harness").join(f);
                        let c = read_if_exists(&p);
                        add_layer(p.display().to_string(), c);
                    }
                }
            }
        }

        b.system_prompt = prompt;
        b.skills.sort_by(|a, c| a.name.cmp(&c.name));
        b
    }

    /// Compact skill list advertised in the tool list (name+description only).
    pub fn skills_block(&self) -> String {
        if self.skills.is_empty() {
            return String::new();
        }
        let mut s = String::from("\nAvailable skills (load full SKILL.md via the `skill` tool only when needed):\n");
        for sk in &self.skills {
            s.push_str(&format!("- {}: {}\n", sk.name, sk.description));
        }
        s
    }
}
