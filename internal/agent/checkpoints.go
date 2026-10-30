package agent

import (
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
)

// Checkpoints implement per-turn shadow-Git snapshots on
// refs/lumina/checkpoints/<session>/<turn> using a dedicated GIT_INDEX_FILE,
// so `git status` and the user index are never touched.

// Checkpoint records one snapshot pointer.
type Checkpoint struct {
	SessionID string
	TurnID    string
	Commit    string
}

// clearStaleLock removes a leftover shadow-index lock from a crashed run.
// The shadow index is single-writer (daemon-owned), so a present lock with
// no live git process is always safe to drop.
func clearStaleLock(indexPath string) {
	_ = os.Remove(indexPath + ".lock")
}

// indexFile returns the private index path for a session.
func indexFile(workspaceDir, sessionID string) string {
	return filepath.Join(workspaceDir, ".lumina", "tmp", "checkpoints", sessionID+".index")
}

func gitDir(workspaceDir string) string {
	return filepath.Join(workspaceDir, ".git")
}

func runGit(workspaceDir string, indexPath string, args ...string) (string, error) {
	cmd := exec.Command("git", args...)
	cmd.Dir = workspaceDir
	env := os.Environ()
	if indexPath != "" {
		env = append(env, "GIT_INDEX_FILE="+indexPath)
	}
	// Isolate identity for shadow commits.
	env = append(env, "GIT_AUTHOR_NAME=lumina", "GIT_AUTHOR_EMAIL=lumina@local",
		"GIT_COMMITTER_NAME=lumina", "GIT_COMMITTER_EMAIL=lumina@local")
	cmd.Env = env
	out, err := cmd.CombinedOutput()
	if err != nil {
		return "", fmt.Errorf("git %v: %v: %s", args, err, out)
	}
	return strings.TrimSpace(string(out)), nil
}

// Snapshot stages the workspace tree into the shadow index and points the
// checkpoint ref at the resulting tree commit. Returns the commit SHA.
func Snapshot(workspaceDir, sessionID, turnID string) (*Checkpoint, error) {
	if _, err := os.Stat(gitDir(workspaceDir)); err != nil {
		return nil, fmt.Errorf("checkpoint: not a git repo: %s", workspaceDir)
	}
	idx := indexFile(workspaceDir, sessionID)
	if err := os.MkdirAll(filepath.Dir(idx), 0o755); err != nil {
		return nil, err
	}
	clearStaleLock(idx) // checkout-index may leave idx.lock behind
	// Fresh index from HEAD, then stage everything (untracked included via add -A).
	head, _ := runGit(workspaceDir, "", "rev-parse", "HEAD")
	_ = os.Remove(idx)
	if head != "" {
		// Seed empty index bound to HEAD tree without touching user index.
		if _, err := runGit(workspaceDir, idx, "read-tree", head); err != nil {
			return nil, err
		}
	}
	if _, err := runGit(workspaceDir, idx, "add", "-A"); err != nil {
		return nil, err
	}
	tree, err := runGit(workspaceDir, idx, "write-tree")
	if err != nil {
		return nil, err
	}
	parent := head
	msg := fmt.Sprintf("lumina checkpoint %s/%s", sessionID, turnID)
	var commit string
	if parent != "" {
		commit, err = runGit(workspaceDir, idx, "commit-tree", tree, "-p", parent, "-m", msg)
	} else {
		commit, err = runGit(workspaceDir, idx, "commit-tree", tree, "-m", msg)
	}
	if err != nil {
		return nil, err
	}
	ref := fmt.Sprintf("refs/lumina/checkpoints/%s/%s", sessionID, turnID)
	if _, err := runGit(workspaceDir, "", "update-ref", ref, commit); err != nil {
		return nil, err
	}
	return &Checkpoint{SessionID: sessionID, TurnID: turnID, Commit: commit}, nil
}

// Revert restores the working tree (and shadow index) to a checkpoint ref.
func Revert(workspaceDir, sessionID, turnID string) error {
	ref := fmt.Sprintf("refs/lumina/checkpoints/%s/%s", sessionID, turnID)
	commit, err := runGit(workspaceDir, "", "rev-parse", ref)
	if err != nil {
		return fmt.Errorf("checkpoint %s not found", ref)
	}
	idx := indexFile(workspaceDir, sessionID)
	clearStaleLock(idx)
	if _, err := runGit(workspaceDir, idx, "read-tree", commit); err != nil {
		return err
	}
	// Checkout index into the working tree without moving HEAD.
	// NOTE: checkout-index refreshes stat info and can leave idx.lock
	// behind on some git versions; always clear it afterwards.
	_, err = runGit(workspaceDir, idx, "checkout-index", "-a", "-f")
	clearStaleLock(idx)
	return err
}

// UndoStack lists checkpoint turns for a session (for /redo support the
// caller keeps a parallel redo pointer; refs stay until session delete).
func UndoStack(workspaceDir, sessionID string) ([]string, error) {
	out, err := runGit(workspaceDir, "", "for-each-ref", "--format=%(refname:short)",
		"refs/lumina/checkpoints/"+sessionID+"/")
	if err != nil {
		return nil, err
	}
	var turns []string
	for _, line := range strings.Split(out, "\n") {
		parts := strings.Split(line, "/")
		if len(parts) > 0 && parts[len(parts)-1] != "" {
			turns = append(turns, parts[len(parts)-1])
		}
	}
	return turns, nil
}
