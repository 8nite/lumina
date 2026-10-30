package agent

import (
	"context"
	"strings"
	"testing"

	"github.com/8nite/lumina/internal/config"
	"github.com/8nite/lumina/internal/llm"
	"github.com/8nite/lumina/internal/tools"
)

func TestToolSpecsAdvertiseWebAndSubagents(t *testing.T) {
	seen := map[string]bool{}
	for _, s := range toolSpecs() {
		seen[s.Name] = true
	}
	for _, want := range []string{"webfetch", "websearch", "subagent", "task_output", "bash", "question", "todowrite", "patch"} {
		if !seen[want] {
			t.Fatalf("toolSpecs missing %q", want)
		}
	}
}

func TestExploreSpecsAreReadOnly(t *testing.T) {
	seen := map[string]bool{}
	for _, s := range subagentSpecs("explore") {
		seen[s.Name] = true
	}
	for _, banned := range []string{"edit", "write", "memory_write", "subagent", "patch", "todowrite"} {
		if seen[banned] {
			t.Fatalf("explore must not include %q", banned)
		}
	}
	for _, want := range []string{"read", "grep", "webfetch", "websearch", "bash"} {
		if !seen[want] {
			t.Fatalf("explore must include %q", want)
		}
	}
}

func TestGeneralSpecsCannotDelegate(t *testing.T) {
	for _, s := range subagentSpecs("general") {
		if s.Name == "subagent" {
			t.Fatal("general must not delegate")
		}
	}
}

func TestRunSubagentTextRejectsUnknownAgent(t *testing.T) {
	st, err := Open(t.TempDir() + "/s.db")
	if err != nil {
		t.Fatal(err)
	}
	defer st.Close()
	cfg := config.Defaults()
	router, err := llm.NewRouter(cfg)
	if err != nil {
		t.Fatal(err)
	}
	reg := tools.NewRegistry()
	r := NewRunner(st, router, reg)
	sess, err := st.CreateSession(t.TempDir(), "build", "ask", "", "")
	if err != nil {
		t.Fatal(err)
	}
	sc := tools.SecCtx{WorkspaceDir: sess.WorkspaceDir, SessionID: sess.ID}
	if _, err := r.runSubagentText(context.Background(), sc, tools.SubagentRequest{Agent: "no-such-agent", Prompt: "do x"}); err == nil {
		t.Fatal("expected unknown agent error")
	} else if !strings.Contains(err.Error(), "unknown agent") {
		t.Fatalf("wrong error: %v", err)
	}
}
