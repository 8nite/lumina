package tools

import (
	"context"
	"strings"
	"testing"
	"time"

	"github.com/8nite/lumina/internal/security"
)

func bypassCtx(ws string) SecCtx {
	return SecCtx{
		Agent: security.AgentBuild, Permission: security.PermBypass,
		WorkspaceDir: ws, BashRules: map[string]string{"*": "ask"},
	}
}

func TestWebfetchRejectsNonHTTP(t *testing.T) {
	ws := t.TempDir()
	r := NewRegistry()
	if _, err := r.Call(context.Background(), bypassCtx(ws), "webfetch", map[string]interface{}{"url": "ftp://example.com/x"}); err == nil {
		t.Fatal("expected non-http reject")
	}
	if _, err := r.Call(context.Background(), bypassCtx(ws), "webfetch", map[string]interface{}{}); err == nil {
		t.Fatal("expected missing url error")
	}
}

func TestWebsearchDisabled(t *testing.T) {
	ws := t.TempDir()
	r := NewRegistry()
	r.WebSearchDisabled = true
	if _, err := r.Call(context.Background(), bypassCtx(ws), "websearch", map[string]interface{}{"query": "x"}); err == nil {
		t.Fatal("expected disabled error")
	} else if !strings.Contains(err.Error(), "disabled") {
		t.Fatalf("wrong error: %v", err)
	}
}

func TestWebsearchNeedsProvider(t *testing.T) {
	ws := t.TempDir()
	r := NewRegistry()
	r.WebSearchProvider = "tavily"
	t.Setenv("TAVILY_API_KEY", "")
	t.Setenv("EXA_API_KEY", "")
	t.Setenv("FIRECRAWL_API_KEY", "")
	t.Setenv("PARALLEL_API_KEY", "")
	t.Setenv("TINYFISH_API_KEY", "")
	if _, err := r.Call(context.Background(), bypassCtx(ws), "websearch", map[string]interface{}{"query": "x"}); err == nil {
		t.Fatal("expected missing-key error")
	}
}

func TestBashBackgroundLifecycle(t *testing.T) {
	ws := t.TempDir()
	r := NewRegistry()
	res, err := r.Call(context.Background(), bypassCtx(ws), "bash", map[string]interface{}{"command": "echo hi", "background": true})
	if err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(res.Output, "task-") {
		t.Fatalf("expected task id, got %q", res.Output)
	}
	id := strings.TrimSpace(strings.TrimPrefix(strings.Split(res.Output, "(poll")[0], "started background task"))
	deadline := time.Now().Add(10 * time.Second)
	for {
		out, err := r.Call(context.Background(), bypassCtx(ws), "task_output", map[string]interface{}{"task_id": id})
		if err != nil {
			t.Fatal(err)
		}
		if !strings.Contains(out.Output, "still running") {
			if !strings.Contains(out.Output, "hi") {
				t.Fatalf("expected task output hi, got %q", out.Output)
			}
			return
		}
		if time.Now().After(deadline) {
			t.Fatal("background task never finished")
		}
		time.Sleep(50 * time.Millisecond)
	}
}

func TestSubagentNeedsWiring(t *testing.T) {
	ws := t.TempDir()
	r := NewRegistry()
	if _, err := r.Call(context.Background(), bypassCtx(ws), "subagent", map[string]interface{}{"agent": "explore", "prompt": "x"}); err == nil {
		t.Fatal("expected not-wired error")
	}
}

func TestSubagentDepthDenied(t *testing.T) {
	ws := t.TempDir()
	r := NewRegistry()
	r.Subagent = func(context.Context, SecCtx, string, string, string) (string, error) {
		return "ok", nil
	}
	sc := bypassCtx(ws)
	sc.SubagentDepth = 1
	if _, err := r.Call(context.Background(), sc, "subagent", map[string]interface{}{"agent": "explore", "prompt": "x"}); err == nil {
		t.Fatal("expected depth deny")
	} else if !strings.Contains(err.Error(), "depth") {
		t.Fatalf("wrong error: %v", err)
	}
}

func TestBackgroundSubagentPoll(t *testing.T) {
	ws := t.TempDir()
	r := NewRegistry()
	r.Subagent = func(context.Context, SecCtx, string, string, string) (string, error) {
		return "child result", nil
	}
	res, err := r.Call(context.Background(), bypassCtx(ws), "subagent", map[string]interface{}{
		"agent": "explore", "prompt": "x", "background": true,
	})
	if err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(res.Output, "task-") {
		t.Fatalf("expected task id, got %q", res.Output)
	}
	id := strings.TrimSpace(strings.TrimPrefix(strings.Split(res.Output, "(poll")[0], "started background subagent task"))
	deadline := time.Now().Add(10 * time.Second)
	for {
		out, err := r.Call(context.Background(), bypassCtx(ws), "task_output", map[string]interface{}{"task_id": id})
		if err != nil {
			t.Fatal(err)
		}
		if !strings.Contains(out.Output, "still running") {
			if !strings.Contains(out.Output, "child result") {
				t.Fatalf("expected child result, got %q", out.Output)
			}
			return
		}
		if time.Now().After(deadline) {
			t.Fatal("background subagent never finished")
		}
		time.Sleep(20 * time.Millisecond)
	}
}
