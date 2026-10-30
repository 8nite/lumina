package kb

import (
	"os"
	"path/filepath"
	"sync"
	"sync/atomic"
	"testing"
	"time"
)

const testGraph = `{
  "nodes": [
    {"id": "auth", "label": "auth"},
    {"id": "session", "label": "session"},
    {"id": "router", "label": "router"},
    {"id": "sqlite", "label": "sqlite"}
  ],
  "edges": [
    {"nodes": ["auth", "session"], "relation": "guards", "confidence": "EXTRACTED", "source_file": "security.go"},
    {"nodes": ["session", "sqlite"], "relation": "stored-in", "confidence": "INFERRED"},
    {"nodes": ["session", "router"], "relation": "uses", "confidence": "AMBIGUOUS"},
    {"nodes": ["lonely"], "relation": "bad", "confidence": "WEIRD"}
  ]
}`

func loadTestGraph(t *testing.T) *Graph {
	t.Helper()
	p := filepath.Join(t.TempDir(), "graph.json")
	if err := os.WriteFile(p, []byte(testGraph), 0o644); err != nil {
		t.Fatal(err)
	}
	g, err := LoadGraph(p)
	if err != nil {
		t.Fatal(err)
	}
	return g
}

func TestTraversal(t *testing.T) {
	g := loadTestGraph(t)
	if len(g.Edges) != 3 {
		t.Fatalf("shrink-guard should drop degenerate edge, got %d", len(g.Edges))
	}
	bfs := g.BFS("auth", 10)
	if len(bfs) != 4 {
		t.Fatalf("bfs=%v", bfs)
	}
	if p := g.ShortestPath("auth", "sqlite"); len(p) != 3 {
		t.Fatalf("shortest=%v", p)
	}
	if gods := g.GodNodes(1); len(gods) != 1 || gods[0] != "session" {
		t.Fatalf("god nodes=%v", gods)
	}
	if aff := g.Affected("auth"); len(aff) != 1 || aff[0] != "session" {
		t.Fatalf("affected=%v", aff)
	}
	if ex := g.Explain("auth", "sqlite"); ex == "" {
		t.Fatal("empty explain")
	}
}

func TestConstrainedVocab(t *testing.T) {
	g := loadTestGraph(t)
	toks := g.ExpandVocab("how does the session router work", 12)
	if len(toks) > 12 {
		t.Fatalf("over budget: %v", toks)
	}
	vocab := map[string]bool{}
	for _, v := range g.Vocab() {
		vocab[v] = true
	}
	for _, tok := range toks {
		if !vocab[tok] {
			t.Fatalf("invented token %q not in vocab", tok)
		}
	}
	found := false
	for _, tok := range toks {
		if tok == "session" || tok == "router" {
			found = true
		}
	}
	if !found {
		t.Fatalf("expected session/router in %v", toks)
	}
}

func TestWikilinkCycleSafe(t *testing.T) {
	dir := t.TempDir()
	os.WriteFile(filepath.Join(dir, "a.md"), []byte("see [[b]]"), 0o644)
	os.WriteFile(filepath.Join(dir, "b.md"), []byte("see [[a]] and [[c]]"), 0o644)
	os.WriteFile(filepath.Join(dir, "c.md"), []byte("end"), 0o644)
	out := ResolveWikilinks(dir, "a", 10)
	if len(out) != 3 {
		t.Fatalf("expected 3 notes, got %v", out)
	}
}

func TestSweeperSingleflight(t *testing.T) {
	s := NewSweeper()
	var n int
	var mu sync.Mutex
	release := make(chan struct{})
	started := make(chan struct{})
	var once sync.Once
	done := make(chan struct{})
	go func() {
		defer close(done)
		_ = s.Run("/ws", func() error {
			mu.Lock()
			n++
			mu.Unlock()
			once.Do(func() { close(started) })
			<-release
			return nil
		})
	}()
	<-started // first sweep now occupies the workspace slot
	// Concurrent triggers while occupied must dedup (the 6x hook-race fix).
	var wg sync.WaitGroup
	for i := 0; i < 5; i++ {
		wg.Add(1)
		go func() {
			defer wg.Done()
			_ = s.Run("/ws", func() error {
				mu.Lock()
				n++
				mu.Unlock()
				return nil
			})
		}()
	}
	wg.Wait()
	close(release)
	<-done
	mu.Lock()
	defer mu.Unlock()
	if n != 1 {
		t.Fatalf("singleflight violated: fn ran %d times", n)
	}
}

func TestManifestStamp(t *testing.T) {
	ws := t.TempDir()
	out := filepath.Join(ws, "graphify-out")
	if err := ManifestStamp(ws, out, []string{filepath.Join(ws, "memory", "a.md")}); err != nil {
		t.Fatal(err)
	}
	if _, err := os.Stat(filepath.Join(out, "manifest.json")); err != nil {
		t.Fatal("manifest not written")
	}
}

func TestRealGraphTraversalBudget(t *testing.T) {
	// TP-06.2: full query set against the real workspace graph, <2ms each.
	p := filepath.Join(repoRoot(t), "memory", "graphify-out", "graph.json")
	if _, err := os.Stat(p); err != nil {
		t.Skip("no real graph.json (run graphify first)")
	}
	g, err := LoadGraph(p)
	if err != nil {
		t.Fatal(err)
	}
	if len(g.Nodes) == 0 {
		t.Skip("empty graph")
	}
	seed := g.Nodes[0].ID
	timeTraversal := func(name string, fn func()) {
		t.Helper()
		start := time.Now()
		fn()
		if d := time.Since(start); d > 2*time.Millisecond {
			t.Fatalf("%s took %v (>2ms)", name, d)
		}
	}
	timeTraversal("bfs", func() { _ = g.BFS(seed, 50) })
	timeTraversal("dfs", func() { _ = g.DFS(seed, 50) })
	timeTraversal("godnodes", func() { _ = g.GodNodes(5) })
	timeTraversal("vocab", func() { _ = g.ExpandVocab("session memory router graph", 12) })
	for _, tok := range g.ExpandVocab("session memory router graph", 12) {
		if !containsStr(g.Vocab(), tok) {
			t.Fatalf("invented token %q", tok)
		}
	}
}

func containsStr(xs []string, s string) bool {
	for _, x := range xs {
		if x == s {
			return true
		}
	}
	return false
}

// repoRoot walks up from the test working directory to the module root.
func repoRoot(t *testing.T) string {
	t.Helper()
	d, err := os.Getwd()
	if err != nil {
		t.Fatal(err)
	}
	for i := 0; i < 8; i++ {
		if _, err := os.Stat(filepath.Join(d, "go.mod")); err == nil {
			return d
		}
		parent := filepath.Dir(d)
		if parent == d {
			break
		}
		d = parent
	}
	t.Fatal("go.mod not found")
	return ""
}

func TestSweeperSixConcurrent(t *testing.T) {
	// TP-06.5: 6 goroutines released at once; the sweep body must never
	// overlap (max concurrency 1) and — with the body slower than any
	// scheduling skew — runs exactly once.
	s := NewSweeper()
	var runs, active, maxActive int32
	release := make(chan struct{})
	var wg sync.WaitGroup
	for i := 0; i < 6; i++ {
		wg.Add(1)
		go func() {
			defer wg.Done()
			<-release
			_ = s.Run("/ws6", func() error {
				atomic.AddInt32(&runs, 1)
				cur := atomic.AddInt32(&active, 1)
				for {
					old := atomic.LoadInt32(&maxActive)
					if cur <= old || atomic.CompareAndSwapInt32(&maxActive, old, cur) {
						break
					}
				}
				time.Sleep(50 * time.Millisecond)
				atomic.AddInt32(&active, -1)
				return nil
			})
		}()
	}
	close(release)
	done := make(chan struct{})
	go func() { wg.Wait(); close(done) }()
	select {
	case <-done:
	case <-time.After(15 * time.Second):
		t.Fatal("sweeper test deadlocked")
	}
	if got := atomic.LoadInt32(&maxActive); got != 1 {
		t.Fatalf("overlap: max concurrent runs = %d", got)
	}
	if got := atomic.LoadInt32(&runs); got != 1 {
		t.Fatalf("singleflight violated: fn ran %d times", got)
	}
}
