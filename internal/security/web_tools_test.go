package security

import "testing"

// Plan mode may research the web but must never delegate (a build child
// could edit while the parent stays in plan, bypassing the read-only fence).
func TestPlanAllowsWebResearchButDeniesSubagent(t *testing.T) {
	for _, tool := range []string{"webfetch", "websearch", "task_output"} {
		if got := Authorize(AgentPlan, PermBypass, tool, "https://example.com/x", ws, nil, false); got != Allow {
			t.Fatalf("plan+bypass must allow %s, got %s", tool, got)
		}
	}
	if got := Authorize(AgentPlan, PermBypass, "subagent", "explore", ws, nil, false); got != Deny {
		t.Fatalf("plan must deny subagent delegation, got %s", got)
	}
}

func TestBuildWebToolsAllowSubagentGated(t *testing.T) {
	rules := map[string]string{"*": "ask"}
	if got := Authorize(AgentBuild, PermAsk, "webfetch", "https://example.com/x", ws, rules, false); got != Allow {
		t.Fatalf("webfetch should allow, got %s", got)
	}
	if got := Authorize(AgentBuild, PermAsk, "websearch", "latest release", ws, rules, false); got != Allow {
		t.Fatalf("websearch should allow, got %s", got)
	}
	if got := Authorize(AgentBuild, PermAsk, "task_output", "task:task-1", ws, rules, false); got != Allow {
		t.Fatalf("task_output should allow, got %s", got)
	}
	if got := Authorize(AgentBuild, PermAsk, "subagent", "explore", ws, rules, false); got != Ask {
		t.Fatalf("subagent should ask, got %s", got)
	}
	if got := Authorize(AgentBuild, PermBypass, "subagent", "explore", ws, rules, false); got != Allow {
		t.Fatalf("bypass should allow subagent, got %s", got)
	}
	// Google hard-deny covers web tools too, even in bypass.
	if got := Authorize(AgentBuild, PermBypass, "webfetch", "https://generativelanguage.googleapis.com/v1", ws, rules, false); got != Deny {
		t.Fatalf("webfetch of google must deny, got %s", got)
	}
}
