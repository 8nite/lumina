package tools

import (
	"context"
	"fmt"
	"net/http"
	"net/http/httptest"
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
	if err != nil || res.Output != "1: hello" {
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
	// Non-whitelisted absolute escapes must always deny (workspace fence).
	// Note: ../escape.txt would land under /tmp (whitelisted for tmp),
	// so escape tests must use a path outside home/tmp/system.
	escapeAbs := "/var/empty/lumina-escape-test.txt"
	for _, alias := range []string{"path", "file"} {
		for _, tool := range []string{"read", "write", "edit"} {
			args := map[string]interface{}{alias: escapeAbs, "content": "x", "old_string": "x", "new_string": "y"}
			if _, err := r.Call(context.Background(), ctx, tool, args); err == nil {
				t.Fatalf("expected escape deny for %s via %s", tool, alias)
			} else if !strings.Contains(err.Error(), "escapes workspace") && !strings.Contains(err.Error(), "deny") && !strings.Contains(err.Error(), "ask") {
				// Any denial (fence or broker) is acceptable; allow must not happen.
				t.Fatalf("expected fence deny for %s via %s, got %v", tool, alias, err)
			}
		}
	}
}

func TestWhitelistAllowsTmpReadsWritesIncludingDotfiles(t *testing.T) {
	ws := t.TempDir()
	r := NewRegistry()
	ctx := testCtx(ws)
	// Normal tmp file: write then read must pass (whitelisted).
	tmpFile := filepath.Join(os.TempDir(), "lumina-whitelist-test.txt")
	_ = os.Remove(tmpFile)
	if _, err := r.Call(context.Background(), ctx, "write", map[string]interface{}{"path": tmpFile, "content": "hello"}); err != nil {
		t.Fatalf("whitelisted /tmp write must pass: %v", err)
	}
	res, err := r.Call(context.Background(), ctx, "read", map[string]interface{}{"path": tmpFile})
	if err != nil || !strings.Contains(res.Output, "hello") {
		t.Fatalf("whitelisted /tmp read must pass: %q err=%v", res.Output, err)
	}
	_ = os.Remove(tmpFile)
	// Per explicit user override: external dotfiles incl. secrets ARE allowed.
	// Workspace-internal secrets stay denied (see TestCredentialDeny).
	secretTmp := filepath.Join(os.TempDir(), "lumina-whitelist-test.env")
	_ = os.WriteFile(secretTmp, []byte("x"), 0o644)
	defer os.Remove(secretTmp)
	if _, err := r.Call(context.Background(), ctx, "read", map[string]interface{}{"path": secretTmp}); err != nil {
		t.Fatalf("external .env in /tmp must allow per override, got %v", err)
	}
	// Workspace-internal .env must still deny.
	if _, err := r.Call(context.Background(), testCtx(ws), "read", map[string]interface{}{"path": ".env"}); err == nil {
		// .env may not exist; ensure fence denies when it does exist.
		_ = os.WriteFile(filepath.Join(ws, ".env"), []byte("s"), 0o644)
		if _, err2 := r.Call(context.Background(), testCtx(ws), "read", map[string]interface{}{"path": ".env"}); err2 == nil {
			t.Fatalf("workspace .env must still deny")
		}
	}
	// System read-only: /etc/hosts readable, but writes denied at fence.
	if _, err := os.Stat("/etc/hosts"); err == nil {
		if _, err := r.Call(context.Background(), ctx, "read", map[string]interface{}{"path": "/etc/hosts"}); err != nil {
			t.Fatalf("/etc/hosts read must pass: %v", err)
		}
		if _, err := r.Call(context.Background(), ctx, "write", map[string]interface{}{"path": "/etc/lumina-test", "content": "x"}); err == nil {
			t.Fatalf("/etc write must deny (read-only system)")
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

func TestReadLineNumbersAndDir(t *testing.T) {
	ws := t.TempDir()
	r := NewRegistry()
	ctx := testCtx(ws)
	os.WriteFile(filepath.Join(ws, "b.txt"), []byte("l1\nl2\nl3"), 0o644)
	res, err := r.Call(context.Background(), ctx, "read", map[string]interface{}{"path": "b.txt", "offset": 2, "limit": 1})
	if err != nil || res.Output != "2: l2" {
		t.Fatalf("read window=%q err=%v", res.Output, err)
	}
	dres, err := r.Call(context.Background(), ctx, "read", map[string]interface{}{"path": "."})
	if err != nil || !strings.Contains(dres.Output, "b.txt") {
		t.Fatalf("dir read=%q err=%v", dres.Output, err)
	}
	os.WriteFile(filepath.Join(ws, "bin.dat"), []byte{0, 1, 2}, 0o644)
	if _, err := r.Call(context.Background(), ctx, "read", map[string]interface{}{"path": "bin.dat"}); err == nil {
		t.Fatal("expected binary reject")
	}
	os.WriteFile(filepath.Join(ws, "c.txt"), []byte("a a"), 0o644)
	if _, err := r.Call(context.Background(), ctx, "edit", map[string]interface{}{"path": "c.txt", "oldString": "a", "newString": "b", "replaceAll": true}); err != nil {
		t.Fatalf("replaceAll edit: %v", err)
	}
}

func TestGrepOptions(t *testing.T) {
	ws := t.TempDir()
	r := NewRegistry()
	ctx := testCtx(ws)
	os.WriteFile(filepath.Join(ws, "a.go"), []byte("FooBar\nfoobar\n"), 0o644)
	os.WriteFile(filepath.Join(ws, "a.txt"), []byte("foobar\n"), 0o644)
	res, _ := r.Call(context.Background(), ctx, "grep", map[string]interface{}{"pattern": "Foo.*"})
	if !strings.Contains(res.Output, "a.go:1:") || strings.Contains(res.Output, "a.txt") {
		t.Fatalf("regex grep wrong: %q", res.Output)
	}
	res, _ = r.Call(context.Background(), ctx, "grep", map[string]interface{}{"pattern": "foobar", "literal": true, "caseSensitive": false, "include": "*.txt"})
	if !strings.Contains(res.Output, "a.txt:1:") || strings.Contains(res.Output, "a.go") {
		t.Fatalf("literal grep wrong: %q", res.Output)
	}
	if _, err := r.Call(context.Background(), ctx, "grep", map[string]interface{}{"pattern": "([bad"}); err == nil {
		t.Fatal("expected invalid regex error")
	}
}

func TestGlobStarStar(t *testing.T) {
	ws := t.TempDir()
	r := NewRegistry()
	ctx := testCtx(ws)
	os.MkdirAll(filepath.Join(ws, "src", "deep"), 0o755)
	os.WriteFile(filepath.Join(ws, "src", "a.ts"), []byte("x"), 0o644)
	os.WriteFile(filepath.Join(ws, "src", "deep", "b.ts"), []byte("x"), 0o644)
	res, err := r.Call(context.Background(), ctx, "glob", map[string]interface{}{"pattern": "**/*.ts"})
	if err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(res.Output, "src/a.ts") || !strings.Contains(res.Output, "src/deep/b.ts") {
		t.Fatalf("** glob missed: %q", res.Output)
	}
}

func TestBashWorkdir(t *testing.T) {
	ws := t.TempDir()
	r := NewRegistry()
	ctx := bypassCtx(ws)
	os.MkdirAll(filepath.Join(ws, "sub"), 0o755)
	res, err := r.Call(context.Background(), ctx, "bash", map[string]interface{}{"command": "pwd", "workdir": "sub"})
	if err != nil || !strings.HasSuffix(strings.TrimSpace(res.Output), "sub") {
		t.Fatalf("workdir pwd=%q err=%v", res.Output, err)
	}
	if _, err := r.Call(context.Background(), ctx, "bash", map[string]interface{}{"command": "pwd", "workdir": "../escape"}); err == nil {
		t.Fatal("expected workdir escape deny")
	}
}

func TestTodoWriteAndQuestion(t *testing.T) {
	ws := t.TempDir()
	r := NewRegistry()
	ctx := testCtx(ws)
	if _, err := r.Call(context.Background(), ctx, "todowrite", map[string]interface{}{"todos": []interface{}{map[string]interface{}{"content": "x", "status": "bogus"}}}); err == nil {
		t.Fatal("expected bad status error")
	}
	res, err := r.Call(context.Background(), ctx, "todowrite", map[string]interface{}{"todos": []interface{}{map[string]interface{}{"content": "do it", "status": "in_progress"}}})
	if err != nil || !strings.Contains(res.Output, "0/1 completed") || !strings.Contains(res.Output, "do it") {
		t.Fatalf("todowrite=%q err=%v", res.Output, err)
	}
	qres, err := r.Call(context.Background(), ctx, "question", map[string]interface{}{"questions": []interface{}{"q1"}})
	if err != nil || !strings.Contains(qres.Output, "not wired") {
		t.Fatalf("question unwired=%q err=%v", qres.Output, err)
	}
	r.AskQuestion = func(context.Context, SecCtx, []Question) ([][]string, error) {
		return [][]string{{"opt-a"}}, nil
	}
	wired, err := r.Call(context.Background(), ctx, "question", map[string]interface{}{"questions": []interface{}{map[string]interface{}{
		"question": "pick one", "header": "Pick", "options": []interface{}{map[string]interface{}{"label": "opt-a", "description": "first"}},
	}}})
	if err != nil || !strings.Contains(wired.Output, `"pick one"="opt-a"`) {
		t.Fatalf("question wired=%q err=%v", wired.Output, err)
	}
	if _, err := r.Call(context.Background(), ctx, "question", map[string]interface{}{"questions": []interface{}{map[string]interface{}{"header": "no-question"}}}); err == nil {
		t.Fatal("expected missing question error")
	}
}

func TestSubagentSessionIDRejected(t *testing.T) {
	ws := t.TempDir()
	r := NewRegistry()
	r.Subagent = func(context.Context, SecCtx, SubagentRequest) (string, error) {
		return "ok", nil
	}
	_, err := r.Call(context.Background(), bypassCtx(ws), "subagent", map[string]interface{}{"agent": "explore", "prompt": "x", "sessionID": "ses-1"})
	if err == nil || !strings.Contains(err.Error(), "sessionID") {
		t.Fatalf("expected sessionID error, got %v", err)
	}
}

func TestWebFetchStripsHTML(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "text/html")
		fmt.Fprint(w, "<html><body><h1>Hi</h1><script>evil()</script></body></html>")
	}))
	defer srv.Close()
	r := NewRegistry()
	res, err := r.Call(context.Background(), bypassCtx(t.TempDir()), "webfetch", map[string]interface{}{"url": srv.URL})
	if err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(res.Output, "Hi") || strings.Contains(res.Output, "evil") || strings.Contains(res.Output, "<h1>") {
		t.Fatalf("html strip wrong: %q", res.Output)
	}
}
