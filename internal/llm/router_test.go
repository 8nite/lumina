package llm

import (
	"testing"

	"github.com/8nite/lumina/internal/config"
)

// A failing api_key_cmd (e.g. `gh` not installed or logged out) must not
// take down the whole router: the provider builds with an empty key so a
// later device auth (/connect) can supply it via the token store.
func TestRouterToleratesFailingSecretCmd(t *testing.T) {
	c := config.Defaults()
	c.Providers = map[string]config.ProviderConfig{
		"copilot": {Protocol: "copilot", APIKeyCmd: "exit 3"},
		"open":    {Protocol: "openai-compatible", BaseURL: "http://127.0.0.1:11434/v1", APIKeyCmd: "exit 3"},
	}
	r, err := NewRouter(c)
	if err != nil {
		t.Fatalf("router must tolerate failing api_key_cmd: %v", err)
	}
	if _, ok := r.Provider("copilot"); !ok {
		t.Fatal("copilot provider missing after tolerated secret failure")
	}
	if _, ok := r.Provider("open"); !ok {
		t.Fatal("open provider missing after tolerated secret failure")
	}
}

// True config errors stay fatal.
func TestRouterStillRejectsBadConfig(t *testing.T) {
	c := config.Defaults()
	c.Providers = map[string]config.ProviderConfig{
		"weird": {Protocol: "nope"},
	}
	if _, err := NewRouter(c); err == nil {
		t.Fatal("unknown protocol must still fail")
	}
}
