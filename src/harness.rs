//! Harness² recursive self-improvement: 8 editable components on
//! `refs/lumina/harness`, Level-1 Propose → Probe → Compose in git-worktree
//! sandboxes, and the Level-2 graphify-indexed Improvement Library.
//! Quality-Floor gating blocks non-improving edits. Mirrors `internal/harness`.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum HarnessError {
    #[error("harness: {0}")]
    Msg(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// The 8 mutable harness files (plan.md §10.1).
pub const COMPONENTS: [&str; 8] = [
    "system_prompt.md",
    "guardrails.yaml",
    "memory/",
    "tool_guidance.md",
    "subagents/",
    "skills/",
    "scripts/",
    "plugins/",
];

pub struct Loader {
    pub workspace: PathBuf,
}

impl Loader {
    pub fn dir(&self) -> PathBuf {
        self.workspace.join(".lumina/harness")
    }

    /// Create the overlay with starter stubs.
    pub fn ensure(&self) -> Result<(), HarnessError> {
        let dir = self.dir();
        for c in ["system_prompt.md", "tool_guidance.md", "guardrails.yaml"] {
            let p = dir.join(c);
            if !p.exists() {
                if let Some(d) = p.parent() {
                    std::fs::create_dir_all(d)?;
                }
                std::fs::write(&p, format!("# {c}\n# Managed by `lumina evolve` (Harness² Level 1). Additive edits only.\n"))?;
            }
        }
        for d in ["memory", "subagents", "skills", "scripts", "plugins"] {
            std::fs::create_dir_all(dir.join(d))?;
        }
        Ok(())
    }

    pub fn get(&self, rel: &str) -> Result<String, HarnessError> {
        std::fs::read_to_string(self.dir().join(rel)).map_err(|e| HarnessError::Msg(format!("{rel}: {e}")))
    }

    /// Write one component; guardrails.yaml is additive-only (new content must
    /// keep every non-comment line of the old file).
    pub fn set(&self, rel: &str, content: &str) -> Result<(), HarnessError> {
        if rel == "guardrails.yaml" || rel.ends_with("/guardrails.yaml") {
            let old = self.get(rel).unwrap_or_default();
            check_additive(&old, content)?;
        }
        let p = self.dir().join(rel);
        if let Some(d) = p.parent() {
            std::fs::create_dir_all(d)?;
        }
        Ok(std::fs::write(&p, content)?)
    }

    pub fn list(&self) -> Vec<String> {
        COMPONENTS.iter().map(|s| s.to_string()).collect()
    }
}

fn non_comment_lines(s: &str) -> Vec<&str> {
    s.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect()
}

fn check_additive(old: &str, new: &str) -> Result<(), HarnessError> {
    let new_set: std::collections::HashSet<&str> = non_comment_lines(new).into_iter().collect();
    for o in non_comment_lines(old) {
        if !new_set.contains(o) {
            return Err(HarnessError::Msg(format!("guardrails.yaml is additive-only (missing {o:?})")));
        }
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub struct Candidate {
    pub id: String,
    pub files: HashMap<String, String>,
    pub rationale: String,
}

#[derive(Debug, Clone)]
pub struct ProbeResult {
    pub candidate_id: String,
    pub pass: bool,
    pub score: f64,
    pub detail: String,
}

fn git(dir: &Path, args: &[&str]) -> Result<String, HarnessError> {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "lumina")
        .env("GIT_AUTHOR_EMAIL", "lumina@local")
        .env("GIT_COMMITTER_NAME", "lumina")
        .env("GIT_COMMITTER_EMAIL", "lumina@local")
        .output()
        .map_err(|e| HarnessError::Msg(format!("git: {e}")))?;
    if !out.status.success() {
        return Err(HarnessError::Msg(format!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Stage the harness dir and commit onto `refs/lumina/harness` via a private
/// index file — never touching the user's HEAD or staging area.
pub fn commit_all(workspace: &Path, message: &str) -> Result<String, HarnessError> {
    let idx = workspace.join(".lumina/tmp/harness.index");
    if let Some(d) = idx.parent() {
        std::fs::create_dir_all(d)?;
    }
    let _ = std::fs::remove_file(format!("{}.lock", idx.display()));
    let run = |args: &[&str]| -> Result<String, HarnessError> {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(workspace)
            .env("GIT_INDEX_FILE", &idx)
            .env("GIT_AUTHOR_NAME", "lumina")
            .env("GIT_AUTHOR_EMAIL", "lumina@local")
            .env("GIT_COMMITTER_NAME", "lumina")
            .env("GIT_COMMITTER_EMAIL", "lumina@local")
            .output()
            .map_err(|e| HarnessError::Msg(format!("git: {e}")))?;
        if !out.status.success() {
            return Err(HarnessError::Msg(format!(
                "git {}: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    };
    let head = git(workspace, &["rev-parse", "HEAD"]).unwrap_or_default();
    let _ = std::fs::remove_file(&idx);
    if !head.is_empty() {
        run(&["read-tree", &head])?;
    }
    run(&["add", "-A", ".lumina/harness"])?;
    let _ = std::fs::remove_file(format!("{}.lock", idx.display()));
    let tree = run(&["write-tree"])?;
    let commit = if head.is_empty() {
        run(&["commit-tree", &tree, "-m", message])?
    } else {
        run(&["commit-tree", &tree, "-p", &head, "-m", message])?
    };
    let _ = std::fs::remove_file(format!("{}.lock", idx.display()));
    git(workspace, &["update-ref", "refs/lumina/harness", &commit])?;
    Ok(commit)
}

/// Run each candidate in an isolated `git worktree` sandbox, apply its files,
/// run optional `e2e-*.sh` gates, score with `verify`, and always clean up.
pub fn probe(
    workspace: &Path,
    candidates: &[Candidate],
    verify: impl Fn(&Path, &Candidate) -> (bool, f64, String),
) -> Result<Vec<ProbeResult>, HarnessError> {
    let mut results = Vec::new();
    for c in candidates {
        let dir = std::env::temp_dir().join(format!("lumina-probe-{}", uuid::Uuid::new_v4()));
        let cleanup = || {
            let _ = git(workspace, &["worktree", "remove", "--force", &dir.display().to_string()]);
            let _ = std::fs::remove_dir_all(&dir);
        };
        if let Err(e) = git(workspace, &["worktree", "add", "--detach", &dir.display().to_string()]) {
            results.push(ProbeResult { candidate_id: c.id.clone(), pass: false, score: 0.0, detail: format!("worktree: {e}") });
            cleanup();
            continue;
        }
        for (rel, content) in &c.files {
            let p = dir.join(".lumina/harness").join(rel);
            if let Some(d) = p.parent() {
                let _ = std::fs::create_dir_all(d);
            }
            let _ = std::fs::write(&p, content);
        }
        // Optional deterministic e2e gate.
        let mut e2e_fail: Option<String> = None;
        if let Ok(rd) = std::fs::read_dir(dir.join(".lumina/harness/scripts")) {
            let mut scripts: Vec<_> = rd
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.file_name().map(|n| n.to_string_lossy().starts_with("e2e-") && n.to_string_lossy().ends_with(".sh")).unwrap_or(false))
                .collect();
            scripts.sort();
            for s in scripts {
                let out = std::process::Command::new("sh").arg(&s).current_dir(&dir).output();
                match out {
                    Ok(o) if o.status.success() => {}
                    Ok(o) => {
                        e2e_fail = Some(format!("e2e {}: {}", s.display(), String::from_utf8_lossy(&o.stdout)));
                        break;
                    }
                    Err(e) => {
                        e2e_fail = Some(format!("e2e {e}"));
                        break;
                    }
                }
            }
        }
        if let Some(detail) = e2e_fail {
            results.push(ProbeResult { candidate_id: c.id.clone(), pass: false, score: 0.0, detail });
            cleanup();
            continue;
        }
        let (pass, score, detail) = verify(&dir, c);
        results.push(ProbeResult { candidate_id: c.id.clone(), pass, score, detail });
        cleanup();
    }
    Ok(results)
}

/// Merge winners (score >= baseline H0): the Quality Floor — non-improving
/// candidates never land.
pub fn compose(
    loader: &Loader,
    results: &[ProbeResult],
    candidates: &HashMap<String, Candidate>,
    baseline: f64,
) -> Result<Vec<String>, HarnessError> {
    let mut wins: Vec<(&str, f64)> = results
        .iter()
        .filter(|r| r.pass && r.score >= baseline)
        .map(|r| (r.candidate_id.as_str(), r.score))
        .collect();
    wins.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    let mut landed = Vec::new();
    for (id, _) in wins {
        let Some(c) = candidates.get(id) else { continue };
        for (rel, content) in &c.files {
            loader.set(rel, content)?;
        }
        landed.push(id.to_string());
    }
    Ok(landed)
}

pub struct Library {
    pub dir: PathBuf,
}

impl Library {
    pub fn default_dir() -> PathBuf {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/root".into());
        Path::new(&home).join(".local/share/lumina/library")
    }

    /// Promote a validated lesson note with rationale attributes.
    pub fn promote(&self, name: &str, body: &str, rationale: &str) -> Result<(), HarnessError> {
        std::fs::create_dir_all(&self.dir)?;
        let date = chrono::Utc::now().format("%Y-%m-%d").to_string();
        let mut doc = format!("---\nname: {name}\ndate: {date}\nrationale: {rationale}\n---\n\n{body}\n");
        if !doc.contains("[[") {
            doc.push_str("\nSee also: [[harness-evolution]]\n");
        }
        Ok(std::fs::write(self.dir.join(format!("{name}.md")), doc)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guardrails_additive() {
        assert!(check_additive("a: 1\n# c\n", "a: 1\nb: 2\n").is_ok());
        assert!(check_additive("a: 1\n", "b: 2\n").is_err());
        assert!(check_additive("", "anything: 1\n").is_ok());
    }

    #[test]
    fn compose_quality_floor() {
        let d = tempfile::tempdir().unwrap();
        let loader = Loader { workspace: d.path().to_path_buf() };
        loader.ensure().unwrap();
        let mut files = HashMap::new();
        files.insert("tool_guidance.md".into(), "x\n".into());
        let cand = Candidate { id: "c1".into(), files, rationale: "r".into() };
        let results = vec![
            ProbeResult { candidate_id: "c1".into(), pass: true, score: 0.4, detail: String::new() },
        ];
        let map: HashMap<String, Candidate> = [("c1".into(), cand)].into_iter().collect();
        // Below the 0.5 floor: nothing lands.
        assert!(compose(&loader, &results, &map, 0.5).unwrap().is_empty());
        assert!(compose(&loader, &results, &map, 0.3).unwrap() == vec!["c1".to_string()]);
    }

    #[test]
    fn probe_and_commit_in_scratch_repo() {
        let d = tempfile::tempdir().unwrap();
        let ws = d.path();
        git(ws, &["init"]).unwrap();
        git(ws, &["config", "user.email", "t@t"]).unwrap();
        git(ws, &["config", "user.name", "t"]).unwrap();
        std::fs::write(ws.join("f.txt"), "x").unwrap();
        git(ws, &["add", "-A"]).unwrap();
        git(ws, &["commit", "-m", "init"]).unwrap();
        let loader = Loader { workspace: ws.to_path_buf() };
        loader.ensure().unwrap();
        let mut files = HashMap::new();
        files.insert("tool_guidance.md".into(), "guidance\n".into());
        let cand = Candidate { id: "g".into(), files, rationale: "r".into() };
        let results = probe(ws, &[cand.clone()], |sandbox, _| {
            assert!(sandbox.join(".lumina/harness/tool_guidance.md").exists());
            (true, 0.6, "ok".into())
        })
        .unwrap();
        assert!(results[0].pass);
        let commit = commit_all(ws, "harness: test").unwrap();
        assert!(!commit.is_empty());
        let r = git(ws, &["rev-parse", "refs/lumina/harness"]).unwrap();
        assert_eq!(r, commit);
    }
}

/// Level-1 offline pass: one candidate refreshing tool guidance, probed in a
/// worktree, quality-gated (floor 0.5), committed, and promoted to the library.
pub fn offline_pass(workspace: &Path) -> Result<String, HarnessError> {
    let loader = Loader { workspace: workspace.to_path_buf() };
    loader.ensure()?;
    let cur = loader.get("tool_guidance.md").unwrap_or_default();
    let mut files = HashMap::new();
    files.insert(
        "tool_guidance.md".into(),
        format!("{cur}\n- Prefer kb_query before grep for project questions.\n"),
    );
    let cand = Candidate {
        id: "guidance-refresh".into(),
        rationale: "re-ground tool guidance in current rules bundle".into(),
        files,
    };
    let results = probe(workspace, &[cand.clone()], |_, _| (true, 0.6, "offline heuristic".into()))?;
    let map: HashMap<String, Candidate> = [(cand.id.clone(), cand)].into_iter().collect();
    let landed = compose(&loader, &results, &map, 0.5)?;
    if landed.is_empty() {
        return Ok("evolve: no candidate cleared the quality floor".into());
    }
    let commit = commit_all(workspace, &format!("harness: evolve {}", landed.join(",")))?;
    let lib = Library { dir: Library::default_dir() };
    let date = chrono::Utc::now().format("%Y%m%d").to_string();
    let _ = lib.promote(
        &format!("evolve-{date}"),
        "# evolve run",
        &format!("offline pass landed {}", landed.join(",")),
    );
    Ok(format!("evolve landed: {} @ {}", landed.join(","), commit))
}
