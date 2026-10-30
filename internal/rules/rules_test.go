package rules

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func TestPrecedenceGlobalThenProject(t *testing.T) {
	home := t.TempDir()
	t.Setenv("HOME", home)
	ws := t.TempDir()

	os.MkdirAll(filepath.Join(home, ".config", "lumina"), 0o755)
	os.WriteFile(filepath.Join(home, ".config", "lumina", "AGENTS.md"), []byte("GLOBAL-RULES"), 0o644)
	os.WriteFile(filepath.Join(ws, "AGENTS.md"), []byte("PROJECT-RULES"), 0o644)

	b := Load(Options{WorkspaceDir: ws, ImportGlobalRules: true})
	if len(b.Layers) < 2 {
		t.Fatalf("expected >=2 layers, got %v", b.Layers)
	}
	g, p := -1, -1
	for i, l := range b.Layers {
		if contains(l, ".config/lumina/AGENTS.md") {
			g = i
		}
		if l == filepath.Join(ws, "AGENTS.md") {
			p = i
		}
	}
	if g < 0 || p < 0 || g > p {
		t.Fatalf("global must precede project: layers=%v", b.Layers)
	}
}

func TestClaudeFallback(t *testing.T) {
	home := t.TempDir()
	t.Setenv("HOME", home)
	ws := t.TempDir()
	os.WriteFile(filepath.Join(ws, "CLAUDE.md"), []byte("CLAUDE-FALLBACK"), 0o644)
	// Fake git root at ws so dirChain == [ws].
	os.MkdirAll(filepath.Join(ws, ".git"), 0o755)

	b := Load(Options{WorkspaceDir: ws, ImportGlobalRules: false})
	found := false
	for _, l := range b.Layers {
		if l == filepath.Join(ws, "CLAUDE.md") {
			found = true
		}
	}
	if !found {
		t.Fatalf("expected CLAUDE.md fallback, layers=%v", b.Layers)
	}
}

func TestSkillDiscovery(t *testing.T) {
	home := t.TempDir()
	t.Setenv("HOME", home)
	ws := t.TempDir()
	os.MkdirAll(filepath.Join(ws, ".lumina", "skills", "review"), 0o755)
	os.WriteFile(filepath.Join(ws, ".lumina", "skills", "review", "SKILL.md"),
		[]byte("---\nname: review\ndescription: Review code\n---\n# Review\n"), 0o644)

	b := Load(Options{WorkspaceDir: ws})
	if len(b.Skills) != 1 || b.Skills[0].Name != "review" {
		t.Fatalf("skill discovery failed: %+v", b.Skills)
	}
}

func contains(s, sub string) bool {
	return len(s) >= len(sub) && (func() bool {
		for i := 0; i+len(sub) <= len(s); i++ {
			if s[i:i+len(sub)] == sub {
				return true
			}
		}
		return false
	})()
}

func TestLayerOrder(t *testing.T) {
	// TP-05.1: Layer 0 core, global, repo root, subdir, harness overlay.
	home := t.TempDir()
	t.Setenv("HOME", home)
	ws := t.TempDir()
	os.MkdirAll(filepath.Join(home, ".config", "lumina"), 0o755)
	os.WriteFile(filepath.Join(home, ".config", "lumina", "AGENTS.md"), []byte("GLOBAL"), 0o644)
	os.WriteFile(filepath.Join(ws, "AGENTS.md"), []byte("ROOT"), 0o644)
	os.MkdirAll(filepath.Join(ws, ".git"), 0o755)
	os.WriteFile(filepath.Join(ws, ".git", "HEAD"), []byte("ref: refs/heads/main\n"), 0o644)
	sub := filepath.Join(ws, "sub")
	os.MkdirAll(sub, 0o755)
	os.WriteFile(filepath.Join(sub, "AGENTS.md"), []byte("SUB"), 0o644)
	os.MkdirAll(filepath.Join(ws, ".lumina", "harness"), 0o755)
	os.WriteFile(filepath.Join(ws, ".lumina", "harness", "system_prompt.md"), []byte("HARNESS"), 0o644)

	b := Load(Options{WorkspaceDir: sub, ImportGlobalRules: true})
	idx := map[string]int{}
	for i, l := range b.Layers {
		idx[l] = i
	}
	glob := filepath.Join(home, ".config", "lumina", "AGENTS.md")
	root := filepath.Join(ws, "AGENTS.md")
	subf := filepath.Join(sub, "AGENTS.md")
	harn := filepath.Join(ws, ".lumina", "harness", "system_prompt.md")
	for _, f := range []string{glob, root, subf, harn} {
		if _, ok := idx[f]; !ok {
			t.Fatalf("layer missing: %s (got %v)", f, b.Layers)
		}
	}
	if !(idx[glob] < idx[root] && idx[root] < idx[subf] && idx[subf] < idx[harn]) {
		t.Fatalf("layer order wrong: %v", b.Layers)
	}
	if !strings.Contains(b.SystemPrompt, "Immutable Security Core") {
		t.Fatal("Layer 0 core missing from system prompt")
	}
}
