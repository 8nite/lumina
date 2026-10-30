package agent

import (
	"strings"
	"testing"

	"github.com/8nite/lumina/internal/llm"
)

func specNames(names ...string) []llm.ToolSpec {
	var out []llm.ToolSpec
	for _, n := range names {
		out = append(out, llm.ToolSpec{Name: n})
	}
	return out
}

func TestRenderGuidanceConditional(t *testing.T) {
	full := renderToolGuidance(toolSpecs())
	for _, want := range []string{"old_string", "webfetch", "kb_query", "patch"} {
		if !strings.Contains(full, want) {
			t.Fatalf("full guidance missing %q", want)
		}
	}
	// Explore-like envelope: no edit/write/delegation lines.
	ro := renderToolGuidance(specNames("read", "grep", "glob", "bash", "webfetch", "task_output"))
	if strings.Contains(ro, "old_string") || strings.Contains(ro, "Delegation:") || strings.Contains(ro, "subagent` launches") {
		t.Fatalf("read-only guidance leaks edit/delegation: %q", ro)
	}
	if !strings.Contains(ro, "webfetch") {
		t.Fatalf("read-only guidance must keep webfetch: %q", ro)
	}
}

func TestEnvBlock(t *testing.T) {
	ws := t.TempDir()
	b := envBlock(ws)
	for _, want := range []string{"Today's date:", "<env>", "Working directory: " + ws, "Platform: "} {
		if !strings.Contains(b, want) {
			t.Fatalf("env block missing %q: %q", want, b)
		}
	}
}
