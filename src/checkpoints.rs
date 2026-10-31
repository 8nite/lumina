//! Shadow-Git checkpoints (/undo /redo): per-turn workspace snapshots on a
//! private index file, committed to `refs/lumina/checkpoints/<session>/<turn>`
//! without touching the user's HEAD, index, or `git log`.

use std::path::Path;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum CheckpointError {
    #[error("checkpoint: {0}")]
    Msg(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

fn git(workspace: &Path, args: &[&str], index: Option<&Path>) -> Result<String, CheckpointError> {
    let mut cmd = std::process::Command::new("git");
    cmd.args(args).current_dir(workspace);
    if let Some(idx) = index {
        cmd.env("GIT_INDEX_FILE", idx);
    }
    cmd.env("GIT_AUTHOR_NAME", "lumina")
        .env("GIT_AUTHOR_EMAIL", "lumina@local")
        .env("GIT_COMMITTER_NAME", "lumina")
        .env("GIT_COMMITTER_EMAIL", "lumina@local");
    let out = cmd.output().map_err(|e| CheckpointError::Msg(format!("git: {e}")))?;
    if !out.status.success() {
        return Err(CheckpointError::Msg(format!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn index_file(workspace: &Path, session: &str) -> std::path::PathBuf {
    workspace.join(".lumina/tmp").join(format!("checkpoints-{session}.index"))
}

/// Snapshot the workspace tree; returns the commit sha. Best-effort: callers
/// (the turn loop) warn and continue when the workspace isn't a git repo.
pub fn snapshot(workspace: &Path, session: &str, turn: &str) -> Result<String, CheckpointError> {
    // Not a repo → no checkpoints (plain directories still run fine).
    if git(workspace, &["rev-parse", "--git-dir"], None).is_err() {
        return Err(CheckpointError::Msg("not a git workspace".into()));
    }
    let idx = index_file(workspace, session);
    if let Some(d) = idx.parent() {
        std::fs::create_dir_all(d)?;
    }
    let _ = std::fs::remove_file(&idx);
    let head = git(workspace, &["rev-parse", "HEAD"], None).unwrap_or_default();
    if !head.is_empty() {
        git(workspace, &["read-tree", &head], Some(&idx))?;
    }
    git(workspace, &["add", "-A", "."], Some(&idx))?;
    let tree = git(workspace, &["write-tree"], Some(&idx))?;
    let _ = std::fs::remove_file(format!("{}.lock", idx.display()));
    let commit = if head.is_empty() {
        git(workspace, &["commit-tree", &tree, "-m", &format!("lumina checkpoint {session}/{turn}")], None)?
    } else {
        git(
            workspace,
            &["commit-tree", &tree, "-p", &head, "-m", &format!("lumina checkpoint {session}/{turn}")],
            None,
        )?
    };
    // Per-turn ref for inspection; creation order lives in the traces table.
    let _ = git(
        workspace,
        &["update-ref", &format!("refs/lumina/checkpoints/{session}/{turn}"), &commit],
        None,
    );
    Ok(commit)
}

/// Restore tracked files from a checkpoint commit. Untracked files are left
/// alone (documented limitation vs full worktree swap).
pub fn restore(workspace: &Path, commit: &str) -> Result<String, CheckpointError> {
    git(workspace, &["rev-parse", "--git-dir"], None)?; // fail fast outside repos
    git(workspace, &["checkout", commit, "--", "."], None)
}


#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        let ws = d.path();
        git(ws, &["init"], None).unwrap();
        git(ws, &["config", "user.email", "t@t"], None).unwrap();
        git(ws, &["config", "user.name", "t"], None).unwrap();
        std::fs::write(ws.join("a.txt"), "v1").unwrap();
        git(ws, &["add", "-A"], None).unwrap();
        git(ws, &["commit", "-m", "init"], None).unwrap();
        d
    }

    #[test]
    fn snapshot_restore_roundtrip() {
        let d = repo();
        let ws = d.path();
        let c1 = snapshot(ws, "s", "t1").unwrap();
        std::fs::write(ws.join("a.txt"), "v2").unwrap();
        restore(ws, &c1).unwrap();
        assert_eq!(std::fs::read_to_string(ws.join("a.txt")).unwrap(), "v1");
    }

    #[test]
    fn non_repo_is_soft_error() {
        let d = tempfile::tempdir().unwrap();
        assert!(snapshot(d.path(), "s", "t1").is_err());
    }
}
