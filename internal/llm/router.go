package llm

import (
	"context"
	"fmt"
	"strings"

	"github.com/8nite/lumina/internal/config"
)

// Router maps tiers (fast | executor | planner | improver) to providers.
type Router struct {
	cfg       *config.Config
	providers map[string]Provider
	secrets   map[string]string // provider key -> resolved api key
}

// NewRouter builds providers from config (no network at construction).
func NewRouter(cfg *config.Config) (*Router, error) {
	r := &Router{cfg: cfg, providers: map[string]Provider{}, secrets: map[string]string{}}
	for name, p := range cfg.Providers {
		if err := config.CheckNoGoogle(p.BaseURL); err != nil {
			return nil, fmt.Errorf("provider %q: %w", name, err)
		}
		key, err := config.ResolveSecret(p)
		if err != nil {
			// A failing api_key_cmd (e.g. `gh` not installed or logged
			// out) must not take down every provider: build with an empty
			// key and let device auth or the token store supply it later
			// (/connect then /model with no restart). True config errors
			// (Google URL, unknown protocol) stay fatal below.
			key = ""
		}
		if key == "" {
			key, _ = config.ResolveSecret(config.ProviderConfig{APIKeyEnv: strings.ToUpper(name) + "_API_KEY"})
		}
		r.secrets[name] = key
		switch p.Protocol {
		case "", "openai-compatible":
			r.providers[name] = &OpenAICompat{BaseURL: orLocal(p.BaseURL, "http://127.0.0.1:11434/v1"), APIKey: key}
		case "anthropic-compatible":
			r.providers[name] = &Anthropic{BaseURL: orLocal(p.BaseURL, "https://api.anthropic.com"), APIKey: key}
		case "ollama":
			r.providers[name] = &Ollama{BaseURL: orLocal(p.BaseURL, "http://127.0.0.1:11434")}
		case "copilot":
			r.providers[name] = &Copilot{
				BaseURL: orLocal(p.BaseURL, ""), TokenURL: p.TokenURL,
				GitHubToken: key, ProviderName: name,
			}
		case "openai-responses", "responses":
			r.providers[name] = &Responses{BaseURL: orLocal(p.BaseURL, "http://127.0.0.1:11434/v1"), APIKey: key}
		default:
			return nil, fmt.Errorf("provider %q: unknown protocol %q", name, p.Protocol)
		}
	}
	// Always ensure a local ollama fallback exists for fast-tier distillation.
	if _, ok := r.providers["local_ollama"]; !ok {
		r.providers["local_ollama"] = &Ollama{BaseURL: "http://127.0.0.1:11434"}
	}
	return r, nil
}

func orLocal(v, d string) string {
	if v == "" {
		return d
	}
	return v
}

// ModelFor resolves the model for a tier; planner falls back to executor,
// matching plan.md §8.2. agentMode may force the planner tier in plan mode.
func (r *Router) ModelFor(role, agentMode string) string {
	if agentMode == "plan" && role == "executor" {
		if m := r.cfg.ModelForRole("planner"); m != "" {
			return m
		}
	}
	if m := r.cfg.ModelForRole(role); m != "" {
		return m
	}
	return r.cfg.ModelForRole("executor")
}

// ProviderFor resolves "providerKey/model" against registered providers.
func (r *Router) ProviderFor(model string) (Provider, string) {
	key, id := splitModel(model)
	if key != "" {
		if p, ok := r.providers[key]; ok {
			return p, id
		}
	}
	// Bare model id: prefer executor's provider, else first openai-compat.
	if m := r.cfg.ModelForRole("executor"); m != "" {
		if k, mid := splitModel(m); k != "" {
			if p, ok := r.providers[k]; ok {
				return p, mid
			}
		}
	}
	for _, p := range r.providers {
		return p, model
	}
	return nil, model
}

// Provider returns one registered provider by key.
func (r *Router) Provider(name string) (Provider, bool) {
	p, ok := r.providers[name]
	return p, ok
}

// ContextWindow returns the provider-advertised context window for a
// fully-qualified "provider/model", or 0 when the provider does not report
// one. Metadata failures never propagate — unknown just means the gauge
// shows used tokens without a window.
func (r *Router) ContextWindow(ctx context.Context, model string) int {
	if r == nil || model == "" {
		return 0
	}
	p, mid := r.ProviderFor(model)
	if p == nil {
		return 0
	}
	lens, ok := p.(ContextLens)
	if !ok {
		return 0
	}
	n, err := lens.ContextWindow(ctx, mid)
	if err != nil || n <= 0 {
		return 0
	}
	return n
}

// RefreshCopilotKey installs a freshly device-authed token into the live
// provider (if present) so /model and chat work without a daemon restart.
// Unknown or non-copilot providers are ignored.
func (r *Router) RefreshCopilotKey(provider, key string) {
	if r == nil {
		return
	}
	if p, ok := r.providers[provider]; ok {
		if cp, ok := p.(*Copilot); ok {
			cp.SetKey(key)
		}
	}
}

// Chat routes a request through the tier's provider.
func (r *Router) Chat(ctx context.Context, role, agentMode string, req Request) (<-chan Chunk, error) {
	model := req.Model
	if model == "" {
		model = r.ModelFor(role, agentMode)
	}
	p, mid := r.ProviderFor(model)
	if p == nil {
		return nil, fmt.Errorf("llm: no provider for model %q", model)
	}
	req.Model = mid
	return p.Chat(ctx, req)
}

// SetModel hot-swaps a tier model and persists it (model-persist).
func (r *Router) SetModel(role, model string) error {
	return r.cfg.PersistModel(role, model)
}
