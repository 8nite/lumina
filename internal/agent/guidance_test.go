package agent

import (
	"strings"
	"testing"

	"github.com/8nite/lumina/internal/llm"
	"github.com/8nite/lumina/internal/rules"
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

func TestSkillsBlockAdvertisesNames(t *testing.T) {
	b := &rules.Bundle{Skills: []rules.SkillMeta{
		{Name: "agent-browser", Description: "Browser automation CLI"},
		{Name: "graphify", Description: ""},
	}}
	got := skillsBlock(b)
	for _, want := range []string{"agent-browser", "Browser automation CLI", "graphify"} {
		if !strings.Contains(got, want) {
			t.Fatalf("skills block missing %q: %q", want, got)
		}
	}
	if skillsBlock(nil) != "" {
		t.Fatal("nil bundle must render empty skills block")
	}
	if skillsBlock(&rules.Bundle{}) != "" {
		t.Fatal("empty bundle must render empty skills block")
	}
}

func TestWithSkillListAnnotatesSkillTool(t *testing.T) {
	b := &rules.Bundle{Skills: []rules.SkillMeta{
		{Name: "agent-browser", Description: "Browser automation CLI"},
	}}
	specs := withSkillList(toolSpecs(), b)
	found := false
	for _, sp := range specs {
		if sp.Name == "skill" && strings.Contains(sp.Description, "agent-browser") {
			found = true
		}
	}
	if !found {
		t.Fatal("skill tool description must list available skills")
	}
	// Nil bundle leaves specs untouched.
	plain := withSkillList(toolSpecs(), nil)
	for _, sp := range plain {
		if sp.Name == "skill" && strings.Contains(sp.Description, "Available:") {
			t.Fatal("nil bundle must not annotate skill tool")
		}
	}
}
