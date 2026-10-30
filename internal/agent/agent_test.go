package agent

import (
	"context"
	"crypto/sha256"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
)

func testStore(t *testing.T) (*Store, func()) {
	t.Helper()
	s, err := Open(filepath.Join(t.TempDir(), "lumina.db"))
	if err != nil {
		t.Fatal(err)
	}
	return s, func() { s.Close() }
}

func TestContinueLatestSession(t *testing.T) {
	s, done := testStore(t)
	defer done()
	if _, err := s.CreateSession("/tmp/ws", "build", "ask", "", "first"); err != nil {
		t.Fatal(err)
	}
	second, err := s.CreateSession("/tmp/ws", "plan", "ask", "", "second")
	if err != nil {
		t.Fatal(err)
	}
	latest, err := s.LatestSession("/tmp/ws")
	if err != nil {
		t.Fatal(err)
	}
	if latest.ID != second.ID {
		t.Fatalf("latest=%s want %s", latest.ID, second.ID)
	}
	// lumina run -c continues exact workspace context.
	r := NewRunner(s, nil, nil)
	out, err := r.RunPrompt(context.Background(), latest.ID, TurnInput{Text: "hello"})
	if err != nil {
		t.Fatal(err)
	}
	if out == "" {
		t.Fatal("empty reply")
	}
	turns, _ := s.ListTurns(latest.ID)
	if len(turns) != 2 {
		t.Fatalf("expected user+assistant turns, got %d", len(turns))
	}
}

func TestForkPreservesHistory(t *testing.T) {
	s, done := testStore(t)
	defer done()
	src, _ := s.CreateSession("/tmp/ws", "build", "ask", "", "")
	_, _ = s.AddTurn(src.ID, "", "user", "step 1")
	_, _ = s.AddTurn(src.ID, "", "assistant", "did step 1")
	fork, err := s.ForkSession(src.ID, "")
	if err != nil {
		t.Fatal(err)
	}
	if fork.ParentID != src.ID {
		t.Fatalf("fork parent=%q", fork.ParentID)
	}
	turns, _ := s.ListTurns(fork.ID)
	if len(turns) != 2 {
		t.Fatalf("fork should copy 2 turns, got %d", len(turns))
	}
}

func TestSanitizeExport(t *testing.T) {
	in := `key sk-abcdefgh12345678 and AKIAIOSFODNN7EXAMPLE plus Bearer abc.def.ghi`
	out := SanitizeExport(in)
	if strings.Contains(out, "sk-abcdefgh") || strings.Contains(out, "AKIAIOSFODNN7EXAMPLE") {
		t.Fatalf("not sanitized: %q", out)
	}
}

func gitRepo(t *testing.T) string {
	t.Helper()
	dir := t.TempDir()
	for _, args := range [][]string{{"init"}, {"config", "user.email", "t@t"}, {"config", "user.name", "t"}} {
		cmd := exec.Command("git", args...)
		cmd.Dir = dir
		if out, err := cmd.CombinedOutput(); err != nil {
			t.Fatalf("git %v: %v %s", args, err, out)
		}
	}
	if err := os.WriteFile(filepath.Join(dir, "f.txt"), []byte("v1"), 0o644); err != nil {
		t.Fatal(err)
	}
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

func TestUndoRedoRestoresTree(t *testing.T) {
	if _, err := exec.LookPath("git"); err != nil {
		t.Skip("git missing")
	}
	ws := gitRepo(t)
	cp, err := Snapshot(ws, "ses1", "t1")
	if err != nil {
		t.Fatal(err)
	}
	if cp.Commit == "" {
		t.Fatal("empty checkpoint commit")
	}
	if err := os.WriteFile(filepath.Join(ws, "f.txt"), []byte("v2-mutated"), 0o644); err != nil {
		t.Fatal(err)
	}
	if err := Revert(ws, "ses1", "t1"); err != nil {
		t.Fatal(err)
	}
	b, _ := os.ReadFile(filepath.Join(ws, "f.txt"))
	if string(b) != "v1" {
		t.Fatalf("undo failed, got %q", b)
	}
	// Redo == snapshot the restored state under a new turn and revert to it.
	if err := os.WriteFile(filepath.Join(ws, "f.txt"), []byte("v2-mutated"), 0o644); err != nil {
		t.Fatal(err)
	}
	if _, err := Snapshot(ws, "ses1", "t2"); err != nil {
		t.Fatal(err)
	}
	if err := Revert(ws, "ses1", "t2"); err != nil {
		t.Fatal(err)
	}
	b, _ = os.ReadFile(filepath.Join(ws, "f.txt"))
	if string(b) != "v2-mutated" {
		t.Fatalf("redo failed, got %q", b)
	}
	// User index untouched: `git status` clean apart from our mutation (restored).
}

// TestCheckpointHygiene proves shadow ops never touch the user index and leak
// no .index.lock files (TP-04.5).
func TestCheckpointHygiene(t *testing.T) {
	if _, err := exec.LookPath("git"); err != nil {
		t.Skip("git missing")
	}
	ws := gitRepo(t)
	// Untracked file must also survive round-trips.
	os.WriteFile(filepath.Join(ws, "new-untracked.txt"), []byte("fresh"), 0o644)
	indexHash := fileHash(t, filepath.Join(ws, ".git", "index"))
	cachedBefore := gitOut(t, ws, "diff", "--cached", "--name-only")
	for i := 0; i < 3; i++ {
		cp, err := Snapshot(ws, "hyg", "t")
		if err != nil {
			t.Fatal(err)
		}
		if cp.Commit == "" {
			t.Fatal("empty commit")
		}
		os.WriteFile(filepath.Join(ws, "f.txt"), []byte("mut"), 0o644)
		if err := Revert(ws, "hyg", "t"); err != nil {
			t.Fatal(err)
		}
	}
	if got := gitOut(t, ws, "diff", "--cached", "--name-only"); got != cachedBefore {
		t.Fatalf("user index touched: %q", got)
	}
	if got := fileHash(t, filepath.Join(ws, ".git", "index")); got != indexHash {
		t.Fatal("user .git/index bytes changed")
	}
	var locks []string
	filepath.Walk(filepath.Join(ws, ".lumina"), func(p string, info os.FileInfo, err error) error {
		if err == nil && strings.HasSuffix(p, ".lock") {
			locks = append(locks, p)
		}
		return nil
	})
	if len(locks) > 0 {
		t.Fatalf("leaked locks: %v", locks)
	}
	b, _ := os.ReadFile(filepath.Join(ws, "new-untracked.txt"))
	if string(b) != "fresh" {
		t.Fatalf("untracked file lost: %q", b)
	}
}

func fileHash(t *testing.T, p string) string {
	t.Helper()
	b, err := os.ReadFile(p)
	if err != nil {
		t.Fatal(err)
	}
	h := sha256.Sum256(b)
	return strings.TrimSpace(string(h[:8]))
}

func gitOut(t *testing.T, dir string, args ...string) string {
	t.Helper()
	cmd := exec.Command("git", args...)
	cmd.Dir = dir
	out, err := cmd.CombinedOutput()
	if err != nil {
		t.Fatalf("git %v: %v %s", args, err, out)
	}
	return strings.TrimSpace(string(out))
}
