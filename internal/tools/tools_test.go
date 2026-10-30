package tools

import (
	"context"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"github.com/8nite/lumina/internal/security"
)

func testCtx(ws string) SecCtx {
	return SecCtx{
		Agent: security.AgentBuild, Permission: security.PermAutoEdit,
		WorkspaceDir: ws, BashRules: map[string]string{"*": "ask"},
	}
}

func TestReadEditWriteRoundtrip(t *testing.T) {
	ws := t.TempDir()
	r := NewRegistry()
	if _, err := r.Call(context.Background(), testCtx(ws), "write", map[string]interface{}{"path": "a.txt", "content": "hello"}); err != nil {
		t.Fatal(err)
	}
	res, err := r.Call(context.Background(), testCtx(ws), "read", map[string]interface{}{"path": "a.txt"})
	if err != nil || res.Output != "hello" {
		t.Fatalf("read=%q err=%v", res.Output, err)
	}
	if _, err := r.Call(context.Background(), testCtx(ws), "edit", map[string]interface{}{"path": "a.txt", "old_string": "hello", "new_string": "world"}); err != nil {
		t.Fatal(err)
	}
	b, _ := os.ReadFile(filepath.Join(ws, "a.txt"))
	if string(b) != "world" {
		t.Fatalf("edit result=%q", b)
	}
}

func TestCredentialDeny(t *testing.T) {
	ws := t.TempDir()
	r := NewRegistry()
	ctx := testCtx(ws)
	ctx.Permission = security.PermBypass
	if _, err := r.Call(context.Background(), ctx, "read", map[string]interface{}{"path": ".env"}); err == nil {
		t.Fatal("expected credential deny")
	}
}

func TestPlanFenceViaRegistry(t *testing.T) {
	ws := t.TempDir()
	r := NewRegistry()
	ctx := testCtx(ws)
	ctx.Agent = security.AgentPlan
	ctx.Permission = security.PermBypass
	if _, err := r.Call(context.Background(), ctx, "edit", map[string]interface{}{"path": "a.txt", "old_string": "x", "new_string": "y"}); err == nil {
		t.Fatal("expected plan fence deny")
	}
	if _, err := r.Call(context.Background(), ctx, "write", map[string]interface{}{"path": "plan.md", "content": "# plan"}); err != nil {
		t.Fatalf("plan.md write must pass: %v", err)
	}
	if _, err := r.Call(context.Background(), ctx, "write", map[string]interface{}{"path": "../escape-plan.md", "content": "x"}); err == nil {
		t.Fatal("expected workspace escape deny")
	}
}

func TestFileAliasEscapeBlocked(t *testing.T) {
	ws := t.TempDir()
	r := NewRegistry()
	ctx := testCtx(ws)
	for _, alias := range []string{"path", "file"} {
		for _, tool := range []string{"read", "write", "edit"} {
			args := map[string]interface{}{alias: "../escape.txt", "content": "x", "old_string": "x", "new_string": "y"}
			if _, err := r.Call(context.Background(), ctx, tool, args); err == nil {
				t.Fatalf("expected escape deny for %s via %s", tool, alias)
			}
		}
	}
}

func TestGrepSkipsCredentials(t *testing.T) {
	ws := t.TempDir()
	r := NewRegistry()
	ctx := testCtx(ws)
	os.WriteFile(filepath.Join(ws, ".env"), []byte("SECRET=topsecret\n"), 0o644)
	os.WriteFile(filepath.Join(ws, "main.go"), []byte("plain\n"), 0o644)
	// Direct grep tool must deny credential targets even in auto-edit.
	if _, err := r.Call(context.Background(), ctx, "grep", map[string]interface{}{"pattern": "SECRET", "path": ".env"}); err == nil {
		t.Fatal("expected grep credential deny")
	}
	// Recursive walk must skip .env contents.
	res, err := r.Call(context.Background(), ctx, "grep", map[string]interface{}{"pattern": "SECRET"})
	if err != nil {
		t.Fatal(err)
	}
	if strings.Contains(res.Output, "topsecret") {
		t.Fatalf("grep walk leaked credential file: %q", res.Output)
	}
}

func TestPluginHooksFireAroundCalls(t *testing.T) {
	ws := t.TempDir()
	r := NewRegistry()
	var before, after []string
	r.BeforeCall = func(_ context.Context, _ SecCtx, tool string, args map[string]interface{}) (map[string]interface{}, error) {
		before = append(before, tool)
		if tool == "bash" {
			return nil, fmt.Errorf("plugin blocked bash with guidance")
		}
		if tool == "write" {
			args["content"] = "rewritten"
		}
		return args, nil
	}
	r.AfterCall = func(_ context.Context, _ SecCtx, tool string, _ map[string]interface{}, _ Result, _ error) {
		after = append(after, tool)
	}
	ctx := testCtx(ws)
	if _, err := r.Call(context.Background(), ctx, "bash", map[string]interface{}{"command": "ls"}); err == nil {
		t.Fatal("expected plugin block")
	}
	res, err := r.Call(context.Background(), ctx, "write", map[string]interface{}{"path": "h.txt", "content": "orig"})
	if err != nil || res.Output == "" {
		t.Fatalf("write failed: %v", res)
	}
	b, _ := os.ReadFile(filepath.Join(ws, "h.txt"))
	if string(b) != "rewritten" {
		t.Fatalf("before-hook rewrite not applied: %q", b)
	}
	if len(before) != 1 || before[0] != "write" || len(after) != 1 || after[0] != "write" {
		t.Fatalf("hook order wrong: before=%v after=%v", before, after)
	}
	// BeforeCall error blocks an otherwise-allowed call.
	r.BeforeCall = func(_ context.Context, _ SecCtx, tool string, args map[string]interface{}) (map[string]interface{}, error) {
		return nil, fmt.Errorf("plugin blocked %s with guidance", tool)
	}
	if _, err := r.Call(context.Background(), ctx, "read", map[string]interface{}{"path": "h.txt"}); err == nil {
		t.Fatal("expected plugin block on allowed call")
	}
}
