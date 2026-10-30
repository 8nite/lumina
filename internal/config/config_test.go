package config

import (
	"os"
	"path/filepath"
	"testing"
)

func writeTemp(t *testing.T, body string) string {
	t.Helper()
	p := filepath.Join(t.TempDir(), "config.toml")
	if err := os.WriteFile(p, []byte(body), 0o644); err != nil {
		t.Fatal(err)
	}
	return p
}

func TestDefaultsWhenMissing(t *testing.T) {
	c, err := Load(filepath.Join(t.TempDir(), "does-not-exist.toml"))
	if err != nil {
		t.Fatal(err)
	}
	if c.Server.Port != 7432 || c.Server.Host != "127.0.0.1" {
		t.Fatalf("bad server defaults: %+v", c.Server)
	}
	if c.Defaults.PermissionMode != "ask" {
		t.Fatalf("bad permission default: %q", c.Defaults.PermissionMode)
	}
	if !c.KB.StandingOps || c.KB.IdleDebounceMinutes != 15 {
		t.Fatalf("bad kb defaults: %+v", c.KB)
	}
}

func TestRejectsPlaintextAPIKey(t *testing.T) {
	p := writeTemp(t, "[providers.x]\nprotocol=\"openai-compatible\"\nbase_url=\"http://localhost:8000/v1\"\napi_key=\"sk-live\"\n")
	if _, err := Load(p); err == nil {
		t.Fatal("expected plaintext api_key rejection")
	}
}

func TestRejectsGoogleDomains(t *testing.T) {
	for _, u := range []string{
		"https://generativelanguage.googleapis.com/v1",
		"https://aiplatform.googleapis.com/v1",
		"https://fonts.googleapis.com/css",
	} {
		p := writeTemp(t, "[providers.g]\nprotocol=\"openai-compatible\"\nbase_url=\""+u+"\"\n")
		if _, err := Load(p); err == nil {
			t.Fatalf("expected google block for %s", u)
		}
	}
}

func TestRejectsGoogleCredentialEnv(t *testing.T) {
	p := writeTemp(t, "[providers.g]\nprotocol=\"openai-compatible\"\nbase_url=\"http://localhost:8000/v1\"\napi_key_env=\"GEMINI_API_KEY\"\n")
	if _, err := Load(p); err == nil {
		t.Fatal("expected GEMINI_API_KEY rejection")
	}
}

func TestPermissionsTables(t *testing.T) {
	p := writeTemp(t, "[permissions]\nedit=\"allow\"\n[permissions.bash]\n\"go test*\"=\"allow\"\n\"*\"=\"ask\"\n[router]\nexecutor=\"meta/m\"\n")
	c, err := Load(p)
	if err != nil {
		t.Fatal(err)
	}
	if c.Permissions["edit"] != "allow" {
		t.Fatalf("permissions=%v", c.Permissions)
	}
	if c.BashRules["*"] != "ask" || c.BashRules["go test*"] != "allow" {
		t.Fatalf("bash rules=%v", c.BashRules)
	}
	if c.Router.Executor != "meta/m" {
		t.Fatalf("router=%+v", c.Router)
	}
}

func TestResolveSecretEnv(t *testing.T) {
	t.Setenv("LUMINA_TEST_KEY", "s3cr3t")
	v, err := ResolveSecret(ProviderConfig{APIKeyEnv: "LUMINA_TEST_KEY"})
	if err != nil || v != "s3cr3t" {
		t.Fatalf("got %q err %v", v, err)
	}
}

func TestModelFallbackPlannerToExecutor(t *testing.T) {
	c := Defaults()
	c.Router.Executor = "anthropic/claude-sonnet-4-20250514"
	if got := c.ModelForRole("planner"); got != c.Router.Executor {
		t.Fatalf("planner should fall back to executor, got %q", got)
	}
}

func TestCopilotProtocolAccepted(t *testing.T) {
	p := writeTemp(t, "[providers.cp]\nprotocol=\"copilot\"\nbase_url=\"https://copilot-api.example.test\"\ntoken_url=\"https://example.test/api/v3/copilot_internal/v2/token\"\napi_key_env=\"GH_TOKEN\"\n")
	c, err := Load(p)
	if err != nil {
		t.Fatal(err)
	}
	if c.Providers["cp"].TokenURL == "" {
		t.Fatal("token_url not parsed")
	}
	bad := writeTemp(t, "[providers.g]\nprotocol=\"copilot\"\nbase_url=\"https://api.githubcopilot.com\"\ntoken_url=\"https://generativelanguage.googleapis.com/x\"\n")
	if _, err := Load(bad); err == nil {
		t.Fatal("expected google token_url rejection")
	}
}
