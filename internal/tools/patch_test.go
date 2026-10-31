package tools

import (
	"context"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"github.com/8nite/lumina/internal/security"
)

func patchCtx(ws string) SecCtx {
	return SecCtx{
		Agent: security.AgentBuild, Permission: security.PermAutoEdit,
		WorkspaceDir: ws, BashRules: map[string]string{"*": "ask"},
	}
}

func TestPatchAddUpdateDelete(t *testing.T) {
	ws := t.TempDir()
	r := NewRegistry()
	os.WriteFile(filepath.Join(ws, "old.go"), []byte("package a\n\nfunc f() {\n\tprintln(1)\n}\n"), 0o644)
	os.WriteFile(filepath.Join(ws, "gone.txt"), []byte("bye\n"), 0o644)
	patch := "*** Begin Patch\n" +
		"*** Add File: new.txt\n" +
		"+hello\n" +
		"*** Update File: old.go\n" +
		"@@ func f() {\n" +
		"-	println(1)\n" +
		"+	println(2)\n" +
		"*** Delete File: gone.txt\n" +
		"*** End Patch"
	res, err := r.Call(context.Background(), patchCtx(ws), "patch", map[string]interface{}{"patchText": patch})
	if err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(res.Output, "A new.txt") || !strings.Contains(res.Output, "M old.go") || !strings.Contains(res.Output, "D gone.txt") {
		t.Fatalf("patch summary wrong: %q", res.Output)
	}
	if b, _ := os.ReadFile(filepath.Join(ws, "new.txt")); string(b) != "hello\n" {
		t.Fatalf("add wrong: %q", b)
	}
	if b, _ := os.ReadFile(filepath.Join(ws, "old.go")); !strings.Contains(string(b), "println(2)") {
		t.Fatalf("update wrong: %q", b)
	}
	if _, err := os.Stat(filepath.Join(ws, "gone.txt")); !os.IsNotExist(err) {
		t.Fatal("delete did not remove file")
	}
}

func TestPatchRejects(t *testing.T) {
	ws := t.TempDir()
	r := NewRegistry()
	ctx := patchCtx(ws)
	for name, p := range map[string]string{
		"no envelope":   "hello",
		"bad hunk":      "*** Begin Patch\n*** Frobnicate: x\n*** End Patch",
		"add no plus":   "*** Begin Patch\n*** Add File: a.txt\noops\n*** End Patch",
		"delete body":   "*** Begin Patch\n*** Delete File: a.txt\nbody\n*** End Patch",
		"empty update":  "*** Begin Patch\n*** Update File: a.txt\n*** End Patch",
		"bad context":   "*** Begin Patch\n*** Update File: a.txt\n@@\n-zzz-no-match\n+new\n*** End Patch",
		"escape":        "*** Begin Patch\n*** Add File: /var/empty/lumina-evil.txt\n+x\n*** End Patch",
		"credential":    "*** Begin Patch\n*** Add File: .env\n+x\n*** End Patch",
		"missing field": "*** Begin Patch\n*** End Patch",
	} {
		if p == "*** Begin Patch\n*** End Patch" {
			if _, err := r.Call(context.Background(), ctx, "patch", map[string]interface{}{"patchText": p}); err == nil {
				t.Fatalf("%s: expected error", name)
			}
			continue
		}
		if _, err := r.Call(context.Background(), ctx, "patch", map[string]interface{}{"patchText": p}); err == nil {
			t.Fatalf("%s: expected error", name)
		}
	}
	// plan fence: src patch denied, plan.md patch allowed
	os.WriteFile(filepath.Join(ws, "plan.md"), []byte("old\n"), 0o644)
	planCtx := SecCtx{Agent: security.AgentPlan, Permission: security.PermBypass, WorkspaceDir: ws}
	planPatch := "*** Begin Patch\n*** Update File: plan.md\n@@\n-old\n+new\n*** End Patch\n"
	if _, err := r.Call(context.Background(), planCtx, "patch", map[string]interface{}{"patchText": planPatch}); err != nil {
		t.Fatalf("plan.md patch must pass: %v", err)
	}
	srcPatch := "*** Begin Patch\n*** Add File: src/a.go\n+x\n*** End Patch"
	if _, err := r.Call(context.Background(), planCtx, "patch", map[string]interface{}{"patchText": srcPatch}); err == nil {
		t.Fatal("plan src patch must be denied")
	}
	// partial progress reported on failure
	os.WriteFile(filepath.Join(ws, "ok.txt"), []byte("v\n"), 0o644)
	mixed := "*** Begin Patch\n*** Add File: first.txt\n+one\n*** Delete File: nope-missing.txt\n*** End Patch"
	_, err := r.Call(context.Background(), ctx, "patch", map[string]interface{}{"patchText": mixed})
	if err == nil || !strings.Contains(err.Error(), "first.txt") {
		t.Fatalf("expected partial progress, got %v", err)
	}
}
