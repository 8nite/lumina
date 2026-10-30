package plugins

import (
	"context"
	"os"
	"path/filepath"
	"testing"
	"time"
)

func writePlugin(t *testing.T, dir, name, body string) string {
	t.Helper()
	p := filepath.Join(dir, name)
	if err := os.WriteFile(p, []byte(body), 0o644); err != nil {
		t.Fatal(err)
	}
	return p
}

// rtk-style arg rewriter (factory signature): rewrites bash commands.
const rtkStyle = `
export default async () => ({
  "tool.execute.before": async (input) => {
    if (input.tool === "bash" && input.command && input.command.includes("rtk")) {
      return { patch: { command: input.command.replaceAll("rtk", "rtk --compact") } };
    }
    return {};
  }
});
`

// secure-credentials-guard style blocker (v2 setup signature).
const guardStyle = `
export default {
  id: "secure-credentials-guard",
  async setup(ctx) {
    ctx.tool.hook("execute.before", async (input) => {
      const target = (input.command || input.path || "");
      if (target.includes(".env") || target.includes("lumina.json")) {
        throw new Error("credential access blocked");
      }
      return {};
    });
  }
};
`

func TestOutOfRepoEmbeddedHost(t *testing.T) {
	// TP-05.5: with cwd outside the repo and no override, the embedded
	// host.mjs copy must serve hooks.
	dir := t.TempDir()
	p := writePlugin(t, dir, "echo.mjs", "export default async () => ({\n\"tool.execute.before\": async (i) => ({ patch: { seen: true } }),\n});\n")
	cwd, _ := os.Getwd()
	defer os.Chdir(cwd)
	if err := os.Chdir(dir); err != nil {
		t.Fatal(err)
	}
	t.Setenv("LUMINA_PLUGIN_HOST", "")
	h := NewHost([]string{p})
	defer h.Close()
	defer CleanupEmbedded()
	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Second)
	defer cancel()
	out, err := h.Dispatch(ctx, HookToolBefore, map[string]interface{}{"tool": "read"})
	if err != nil {
		t.Fatalf("embedded host dispatch: %v", err)
	}
	if out["seen"] != true {
		t.Fatalf("patch not applied: %v", out)
	}
}

func TestHookPipeline(t *testing.T) {
	if _, err := os.Stat("/usr/bin/node"); err != nil {
		if _, err2 := os.Stat("/usr/local/bin/node"); err2 != nil {
			// Try PATH lookup via shell instead of skipping blindly.
		}
	}
	dir := t.TempDir()
	t.Setenv("LUMINA_PLUGIN_HOST", filepath.Join(absRepoRoot(), "internal", "plugins", "host.mjs"))
	p1 := writePlugin(t, dir, "rtk.mjs", rtkStyle)
	p2 := writePlugin(t, dir, "guard.mjs", guardStyle)
	h := NewHost([]string{p1, p2})
	defer h.Close()
	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Second)
	defer cancel()

	// Arg rewrite flows through patch.
	out, err := h.Dispatch(ctx, HookToolBefore, map[string]interface{}{"tool": "bash", "command": "npx rtk status"})
	if err != nil {
		t.Fatalf("dispatch rewrite: %v", err)
	}
	if got, _ := out["command"].(string); got != "npx rtk --compact status" {
		t.Fatalf("rewrite failed: %v", out)
	}

	// Guard blocks credential access (surfaced as error).
	if _, err := h.Dispatch(ctx, HookToolBefore, map[string]interface{}{"tool": "bash", "command": "cat .env"}); err == nil {
		t.Fatal("expected credential block")
	}
}

func absRepoRoot() string {
	// tests run from the package dir; walk up to module root.
	d, _ := os.Getwd()
	for i := 0; i < 6; i++ {
		if _, err := os.Stat(filepath.Join(d, "go.mod")); err == nil {
			// package dir is <root>/internal/plugins; go.mod at root.
			return d
		}
		// If we're already inside internal/plugins, d is that dir.
		d = filepath.Dir(d)
	}
	return "."
}
