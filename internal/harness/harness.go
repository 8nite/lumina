// Package harness implements Harness² recursive self-improvement:
// 8 editable components on refs/lumina/harness, Level-1 Propose → Probe →
// Compose in git-worktree sandboxes, and the Level-2 graphify-indexed
// Improvement Library. Quality-Floor gating blocks non-improving edits.
package harness

import (
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"sort"
	"strings"
	"time"
)

// Components lists the 8 mutable harness files (plan.md §10.1).
var Components = []string{
	"system_prompt.md",
	"guardrails.yaml",
	"memory/",
	"tool_guidance.md",
	"subagents/",
	"skills/",
	"scripts/",
	"plugins/",
}

// Loader reads the 8 components from .lumina/harness.
type Loader struct {
	WorkspaceDir string
}

// Dir returns the harness overlay root.
func (l *Loader) Dir() string { return filepath.Join(l.WorkspaceDir, ".lumina", "harness") }

// Ensure creates the overlay with starter stubs.
func (l *Loader) Ensure() error {
	dir := l.Dir()
	for _, c := range []string{"system_prompt.md", "tool_guidance.md", "guardrails.yaml"} {
		p := filepath.Join(dir, c)
		if _, err := os.Stat(p); os.IsNotExist(err) {
			if err := os.MkdirAll(filepath.Dir(p), 0o755); err != nil {
				return err
			}
			stub := "# " + c + "\n# Managed by `lumina evolve` (Harness² Level 1). Additive edits only.\n"
			if err := os.WriteFile(p, []byte(stub), 0o644); err != nil {
				return err
			}
		}
	}
	for _, d := range []string{"memory", "subagents", "skills", "scripts", "plugins"} {
		if err := os.MkdirAll(filepath.Join(dir, d), 0o755); err != nil {
			return err
		}
	}
	return nil
}

// Get reads one component file (relative to the harness dir).
func (l *Loader) Get(rel string) (string, error) {
	b, err := os.ReadFile(filepath.Join(l.Dir(), rel))
	if err != nil {
		return "", err
	}
	return string(b), nil
}

// Set writes one component file, blocking guardrail loosening at the Go
// layer (additive-only: new content must keep every non-comment line of the
// old guardrails.yaml).
func (l *Loader) Set(rel, content string) error {
	if rel == "guardrails.yaml" || strings.HasSuffix(rel, "/guardrails.yaml") {
		old, _ := l.Get(rel)
		if err := checkAdditive(old, content); err != nil {
			return err
		}
	}
	p := filepath.Join(l.Dir(), rel)
	if err := os.MkdirAll(filepath.Dir(p), 0o755); err != nil {
		return err
	}
	return os.WriteFile(p, []byte(content), 0o644)
}

func checkAdditive(old, new string) error {
	olds := nonCommentLines(old)
	lines := map[string]bool{}
	for _, l := range nonCommentLines(new) {
		lines[l] = true
	}
	for _, o := range olds {
		if !lines[o] {
			return fmt.Errorf("harness: guardrails.yaml is additive-only (missing %q)", o)
		}
	}
	return nil
}

func nonCommentLines(s string) []string {
	var out []string
	for _, l := range strings.Split(s, "\n") {
		t := strings.TrimSpace(l)
		if t == "" || strings.HasPrefix(t, "#") {
			continue
		}
		out = append(out, t)
	}
	return out
}

// Candidate is one proposed harness patch batch.
type Candidate struct {
	ID        string
	Files     map[string]string // rel path -> new content
	Rationale string
}

// ProbeResult scores one candidate in its sandbox worktree.
type ProbeResult struct {
	CandidateID string
	Pass        bool
	Score       float64
	Detail      string
	Worktree    string
}

// git runs a git command in dir.
func git(dir string, args ...string) (string, error) {
	cmd := exec.Command("git", args...)
	cmd.Dir = dir
	cmd.Env = append(os.Environ(),
		"GIT_AUTHOR_NAME=lumina", "GIT_AUTHOR_EMAIL=lumina@local",
		"GIT_COMMITTER_NAME=lumina", "GIT_COMMITTER_EMAIL=lumina@local")
	out, err := cmd.CombinedOutput()
	if err != nil {
		return "", fmt.Errorf("git %v: %v: %s", args, err, out)
	}
	return strings.TrimSpace(string(out)), nil
}

// CommitAll stages the harness dir and commits onto refs/lumina/harness
// without touching the user's HEAD or index.
func (l *Loader) CommitAll(message string) (string, error) {
	idx := filepath.Join(l.WorkspaceDir, ".lumina", "tmp", "harness.index")
	if err := os.MkdirAll(filepath.Dir(idx), 0o755); err != nil {
		return "", err
	}
	_ = os.Remove(idx + ".lock")
	env := func(c *exec.Cmd) {
		c.Env = append(os.Environ(), "GIT_INDEX_FILE="+idx,
			"GIT_AUTHOR_NAME=lumina", "GIT_AUTHOR_EMAIL=lumina@local",
			"GIT_COMMITTER_NAME=lumina", "GIT_COMMITTER_EMAIL=lumina@local")
		c.Dir = l.WorkspaceDir
	}
	run := func(args ...string) (string, error) {
		cmd := exec.Command("git", args...)
		env(cmd)
		out, err := cmd.CombinedOutput()
		if err != nil {
			return "", fmt.Errorf("git %v: %v: %s", args, err, out)
		}
		return strings.TrimSpace(string(out)), nil
	}
	head, _ := git(l.WorkspaceDir, "rev-parse", "HEAD")
	_ = os.Remove(idx)
	if head != "" {
		if _, err := run("read-tree", head); err != nil {
			return "", err
		}
	}
	if _, err := run("add", "-A", ".lumina/harness"); err != nil {
		return "", err
	}
	_ = os.Remove(idx + ".lock")
	tree, err := run("write-tree")
	if err != nil {
		return "", err
	}
	var commit string
	if head != "" {
		commit, err = run("commit-tree", tree, "-p", head, "-m", message)
	} else {
		commit, err = run("commit-tree", tree, "-m", message)
	}
	if err != nil {
		return "", err
	}
	_ = os.Remove(idx + ".lock")
	_, err = git(l.WorkspaceDir, "update-ref", "refs/lumina/harness", commit)
	return commit, err
}

// Probe runs each candidate in an isolated `git worktree` sandbox under
// /tmp/lumina-probe-*, evaluates verify(script?) and returns scored results.
// Worktrees are always cleaned up. verify receives the sandbox path and
// returns (pass, score[0..1], detail).
func Probe(workspaceDir string, candidates []Candidate, verify func(sandbox string, c Candidate) (bool, float64, string)) ([]ProbeResult, error) {
	var results []ProbeResult
	for _, c := range candidates {
		dir, err := os.MkdirTemp("", "lumina-probe-*")
		if err != nil {
			return results, err
		}
		cleanup := func() {
			_, _ = git(workspaceDir, "worktree", "remove", "--force", dir)
			_ = os.RemoveAll(dir)
		}
		// Detached worktree at HEAD keeps probes off all branches.
		if _, err := git(workspaceDir, "worktree", "add", "--detach", dir); err != nil {
			cleanup()
			results = append(results, ProbeResult{CandidateID: c.ID, Detail: "worktree: " + err.Error()})
			continue
		}
		// Apply candidate files into the sandbox harness overlay.
		for rel, content := range c.Files {
			p := filepath.Join(dir, ".lumina", "harness", rel)
			_ = os.MkdirAll(filepath.Dir(p), 0o755)
			_ = os.WriteFile(p, []byte(content), 0o644)
		}
		// Optional deterministic e2e gate: scripts/e2e-*.sh inside the sandbox.
		e2ePass := true
		if matches, _ := filepath.Glob(filepath.Join(dir, ".lumina", "harness", "scripts", "e2e-*.sh")); len(matches) > 0 {
			for _, s := range matches {
				cmd := exec.Command("sh", s)
				cmd.Dir = dir
				if out, err := cmd.CombinedOutput(); err != nil {
					e2ePass = false
					results = append(results, ProbeResult{CandidateID: c.ID, Detail: "e2e " + s + ": " + string(out), Worktree: dir})
					break
				}
			}
		}
		if !e2ePass {
			cleanup()
			continue
		}
		pass, score, detail := verify(dir, c)
		results = append(results, ProbeResult{CandidateID: c.ID, Pass: pass, Score: score, Detail: detail, Worktree: dir})
		cleanup()
	}
	return results, nil
}

// Compose merges winners (score >= baseline H0) into the live harness dir,
// enforcing the Quality Floor: non-improving candidates never land.
func (l *Loader) Compose(results []ProbeResult, candidates map[string]Candidate, baseline float64) ([]string, error) {
	type win struct {
		id    string
		score float64
	}
	var wins []win
	for _, r := range results {
		if r.Pass && r.Score >= baseline {
			wins = append(wins, win{r.CandidateID, r.Score})
		}
	}
	sort.Slice(wins, func(i, j int) bool { return wins[i].score > wins[j].score })
	var landed []string
	for _, w := range wins {
		c, ok := candidates[w.id]
		if !ok {
			continue
		}
		for rel, content := range c.Files {
			if err := l.Set(rel, content); err != nil {
				return landed, err
			}
		}
		landed = append(landed, w.id)
	}
	return landed, nil
}

// Library promotes validated lessons into ~/.local/share/lumina/library.
type Library struct {
	Dir string
}

// DefaultLibraryDir is ~/.local/share/lumina/library.
func DefaultLibraryDir() string {
	home, _ := os.UserHomeDir()
	return filepath.Join(home, ".local", "share", "lumina", "library")
}

// Promote writes a lesson note + SKILL.md playbook with rationale attributes.
func (l *Library) Promote(name, body, rationale string) error {
	if err := os.MkdirAll(l.Dir, 0o755); err != nil {
		return err
	}
	ts := time.Now().UTC().Format("2006-01-02")
	doc := "---\nname: " + name + "\ndate: " + ts + "\nrationale: " + rationale + "\n---\n\n" + body + "\n"
	if !strings.Contains(doc, "[[") {
		doc += "\nSee also: [[harness-evolution]]\n"
	}
	return os.WriteFile(filepath.Join(l.Dir, name+".md"), []byte(doc), 0o644)
}
