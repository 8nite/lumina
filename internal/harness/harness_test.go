package harness

import (
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
)

func splitLines(s string) []string { return strings.Split(s, "\n") }

func gitInit(t *testing.T) string {
	t.Helper()
	dir := t.TempDir()
	for _, args := range [][]string{{"init"}, {"config", "user.email", "t@t"}, {"config", "user.name", "t"}} {
		cmd := exec.Command("git", args...)
		cmd.Dir = dir
		if out, err := cmd.CombinedOutput(); err != nil {
			t.Fatalf("git %v: %v %s", args, err, out)
		}
	}
	os.WriteFile(filepath.Join(dir, "f.txt"), []byte("v1"), 0o644)
	cmd := exec.Command("git", "add", "-A")
	cmd.Dir = dir
	_ = cmd.Run()
	cmd = exec.Command("git", "commit", "-m", "init")
	cmd.Dir = dir
	if out, err := cmd.CombinedOutput(); err != nil {
		t.Fatalf("commit: %v %s", err, out)
	}
	return dir
}

func TestEnsureAndAdditiveGuardrails(t *testing.T) {
	ws := gitInit(t)
	l := &Loader{WorkspaceDir: ws}
	if err := l.Ensure(); err != nil {
		t.Fatal(err)
	}
	if err := l.Set("guardrails.yaml", "checks:\n  - lint\n"); err != nil {
		t.Fatal(err)
	}
	// Loosening (dropping a rule) must be blocked at the Go layer.
	if err := l.Set("guardrails.yaml", "checks:\n  - nothing\n"); err == nil {
		t.Fatal("expected additive-only rejection")
	}
}

func TestProbeComposeQualityFloor(t *testing.T) {
	if _, err := exec.LookPath("git"); err != nil {
		t.Skip("git missing")
	}
	ws := gitInit(t)
	l := &Loader{WorkspaceDir: ws}
	if err := l.Ensure(); err != nil {
		t.Fatal(err)
	}
	cands := []Candidate{
		{ID: "good", Files: map[string]string{"tool_guidance.md": "# guidance v2\nPrefer kb_query first.\n"}, Rationale: "retrieval first"},
		{ID: "bad", Files: map[string]string{"tool_guidance.md": "# guidance v0\nAlways grep everything.\n"}, Rationale: "worse"},
	}
	results, err := Probe(ws, cands, func(sandbox string, c Candidate) (bool, float64, string) {
		// Deterministic verifier: "good" beats baseline H0=0.5.
		if c.ID == "good" {
			return true, 0.9, "improves"
		}
		return true, 0.2, "regresses"
	})
	if err != nil {
		t.Fatal(err)
	}
	if len(results) != 2 {
		t.Fatalf("expected 2 probe results, got %d", len(results))
	}
	byID := map[string]Candidate{"good": cands[0], "bad": cands[1]}
	landed, err := l.Compose(results, byID, 0.5)
	if err != nil {
		t.Fatal(err)
	}
	if len(landed) != 1 || landed[0] != "good" {
		t.Fatalf("quality floor violated: landed=%v", landed)
	}
	// Probe sandboxes cleaned up: only the main worktree must remain.
	out, _ := git(ws, "worktree", "list", "--porcelain")
	count := 0
	for _, line := range splitLines(out) {
		if len(line) > 9 && line[:9] == "worktree " {
			count++
		}
	}
	if count != 1 {
		t.Fatalf("leaked worktrees:\n%s", out)
	}
	if _, err := l.CommitAll("harness: land good"); err != nil {
		t.Fatal(err)
	}
}

func TestLibraryPromote(t *testing.T) {
	lib := &Library{Dir: filepath.Join(t.TempDir(), "library")}
	if err := lib.Promote("fast-kb-first", "# playbook", "saves tool turns"); err != nil {
		t.Fatal(err)
	}
	if _, err := os.Stat(filepath.Join(lib.Dir, "fast-kb-first.md")); err != nil {
		t.Fatal("lesson not written")
	}
}

func TestProbeE2EGate(t *testing.T) {
	// TP-07.2: a candidate shipping a failing e2e-*.sh is rejected even
	// with a passing verifier; no worktrees leak.
	if _, err := exec.LookPath("git"); err != nil {
		t.Skip("git missing")
	}
	ws := gitInit(t)
	l := &Loader{WorkspaceDir: ws}
	if err := l.Ensure(); err != nil {
		t.Fatal(err)
	}
	cands := []Candidate{{
		ID: "e2e-fail",
		Files: map[string]string{
			"tool_guidance.md":     "# guidance v2\n",
			"scripts/e2e-check.sh": "#!/bin/sh\necho boom >&2\nexit 1\n",
		},
		Rationale: "high score but broken gate",
	}}
	results, err := Probe(ws, cands, func(sandbox string, c Candidate) (bool, float64, string) {
		return true, 0.99, "verifier loves it"
	})
	if err != nil {
		t.Fatal(err)
	}
	if len(results) != 1 || results[0].Pass {
		t.Fatalf("e2e gate should reject: %+v", results)
	}
	out, _ := git(ws, "worktree", "list", "--porcelain")
	count := 0
	for _, line := range splitLines(out) {
		if len(line) > 9 && line[:9] == "worktree " {
			count++
		}
	}
	if count != 1 {
		t.Fatalf("leaked worktrees:\n%s", out)
	}
	landed, err := l.Compose(results, map[string]Candidate{cands[0].ID: cands[0]}, 0.5)
	if err != nil {
		t.Fatal(err)
	}
	if len(landed) != 0 {
		t.Fatalf("rejected candidate landed: %v", landed)
	}
}
