package llm

import (
	"context"
	"fmt"
	"net/http"
	"net/http/httptest"
	"sync/atomic"
	"testing"

	"github.com/8nite/lumina/internal/config"
)

// TestOpenAICompatContextWindow proves the window comes from the gateway's
// /models document (OpenRouter-style context_length), is cached, and is
// unknown (0) when the gateway reports nothing.
func TestOpenAICompatContextWindow(t *testing.T) {
	var hits int64
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		atomic.AddInt64(&hits, 1)
		w.Header().Set("Content-Type", "application/json")
		fmt.Fprint(w, `{"data":[
			{"id":"big","name":"Big","context_length":128000},
			{"id":"plain","name":"Plain"}
		]}`)
	}))
	defer srv.Close()
	p := &OpenAICompat{BaseURL: srv.URL}
	if got, err := p.ContextWindow(context.Background(), "big"); err != nil || got != 128000 {
		t.Fatalf("window=%d err=%v", got, err)
	}
	if got, err := p.ContextWindow(context.Background(), "plain"); err != nil || got != 0 {
		t.Fatalf("unreported window must be 0, got %d err=%v", got, err)
	}
	if got, err := p.ContextWindow(context.Background(), "big"); err != nil || got != 128000 {
		t.Fatalf("cached window=%d err=%v", got, err)
	}
	if n := atomic.LoadInt64(&hits); n != 1 {
		t.Fatalf("/models fetched %d times, want 1 (cached)", n)
	}
}

// TestOllamaContextWindow proves the window comes from /api/show
// model_info and is cached per model.
func TestOllamaContextWindow(t *testing.T) {
	var hits int64
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		atomic.AddInt64(&hits, 1)
		w.Header().Set("Content-Type", "application/json")
		fmt.Fprint(w, `{"model_info":{"llama.context_length":8192.0,"other":1}}`)
	}))
	defer srv.Close()
	p := &Ollama{BaseURL: srv.URL}
	if got, err := p.ContextWindow(context.Background(), "llama3"); err != nil || got != 8192 {
		t.Fatalf("window=%d err=%v", got, err)
	}
	if _, err := p.ContextWindow(context.Background(), "llama3"); err != nil {
		t.Fatal(err)
	}
	if n := atomic.LoadInt64(&hits); n != 1 {
		t.Fatalf("/api/show fetched %d times, want 1 (cached)", n)
	}
}

// TestRouterContextWindowUnknown proves unreachable gateways and unknown
// providers resolve to 0 without failing (metadata failures never fail a turn).
func TestRouterContextWindowUnknown(t *testing.T) {
	r, err := NewRouter(testConfig())
	if err != nil {
		t.Fatal(err)
	}
	// openrouter gateway is unreachable in tests; metadata failure → 0.
	if got := r.ContextWindow(context.Background(), "openrouter/deepseek/deepseek-r1"); got != 0 {
		t.Fatalf("unreachable gateway must yield 0, got %d", got)
	}
	c := testConfig()
	c.Providers["a"] = config.ProviderConfig{Protocol: "anthropic-compatible", BaseURL: "https://api.anthropic.com"}
	c.Router.Executor = "a/claude-x"
	r2, err := NewRouter(c)
	if err != nil {
		t.Fatal(err)
	}
	if got := r2.ContextWindow(context.Background(), "a/claude-x"); got != 0 {
		t.Fatalf("unreachable anthropic endpoint must yield 0, got %d", got)
	}
	if got := r2.ContextWindow(context.Background(), "nope/nothing"); got != 0 {
		t.Fatalf("unknown provider must yield 0, got %d", got)
	}
}
