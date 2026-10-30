package agent

import (
	"strings"
	"testing"

	"github.com/8nite/lumina/internal/llm"
)

func TestEstimateContextTokens(t *testing.T) {
	// "system"(6) + "hello"(5) = 11 chars, no specs: 11/4+1 = 3.
	got := EstimateContextTokens([]llm.Message{{Role: "system", Content: "hello"}}, nil)
	if got != 3 {
		t.Fatalf("estimate=%d want 3", got)
	}
	// Growth is monotonic as the agentic loop appends rounds.
	base := EstimateContextTokens([]llm.Message{{Role: "user", Content: "hi"}}, toolSpecs())
	grown := EstimateContextTokens([]llm.Message{
		{Role: "user", Content: "hi"},
		{Role: "assistant", Content: "working"},
		{Role: "user", Content: "Tool results:\nread: ok\nContinue"},
	}, toolSpecs())
	if grown <= base {
		t.Fatalf("estimate must grow with rounds: base=%d grown=%d", base, grown)
	}
}

func TestTurnSummaryIncludesContext(t *testing.T) {
	s := TurnStats{Model: "prov/m", DurationMs: 2000, OutputTokens: 100, ContextTokens: 12000, MaxTokens: 128000}
	out := s.Summary()
	for _, want := range []string{"prov/m", "50 tok/s", "ctx ~12.0k/128.0k"} {
		if !strings.Contains(out, want) {
			t.Fatalf("summary %q missing %q", out, want)
		}
	}
	// Unknown window (provider reports none) shows used-only, never a
	// hardcoded guess.
	usedOnly := TurnStats{Model: "prov/m", ContextTokens: 12000}.Summary()
	if usedOnly != "prov/m · ctx ~12.0k" {
		t.Fatalf("used-only summary wrong: %q", usedOnly)
	}
	// No estimate at all stays omitted.
	plain := TurnStats{Model: "prov/m"}.Summary()
	if strings.Contains(plain, "ctx") {
		t.Fatalf("ctx must be omitted when unknown: %q", plain)
	}
}
