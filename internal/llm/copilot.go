package llm

import (
	"context"
	"encoding/json"
	"fmt"
	"net/http"
	"strings"
	"sync"
	"time"
)

// Copilot speaks GitHub Copilot's OpenAI-compatible chat API with the native
// two-token flow: a long-lived GitHub token (ghu_/PAT/env/`gh auth token`)
// is exchanged for a short-lived Copilot session token, cached in memory and
// refreshed ahead of expiry. Enterprise routing follows the exchange
// response (`endpoints.api`, `proxy-ep`); an explicit BaseURL always wins.
type Copilot struct {
	// BaseURL overrides the exchanged endpoint (GHES / proxies).
	BaseURL string
	// TokenURL overrides the exchange endpoint (GHES).
	TokenURL string
	// GitHubToken is the long-lived user token (never the session token).
	GitHubToken string
	// ProviderName selects the daemon token-store file fallback.
	ProviderName string

	mu     sync.Mutex
	cached *copilotSession

	ctxMu      sync.Mutex
	ctxCache   map[string]int // model id → advertised window (0 = none)
	ctxFetched bool
}

type copilotSession struct {
	token   string
	expires int64 // unix seconds
	baseURL string
}

const (
	defaultCopilotTokenURL = "https://api.github.com/copilot_internal/v2/token"
	fallbackCopilotBase    = "https://api.githubcopilot.com"
	copilotUserAgent       = "GitHubCopilotChat/0.35.0"
	copilotEditorVersion   = "vscode/1.107.0"
	copilotPluginVersion   = "copilot-chat/0.35.0"
	copilotIntegrationID   = "vscode-chat"
	// refreshMargin refreshes the session token this far ahead of expiry.
	refreshMargin = int64(300)
)

func (p *Copilot) Name() string { return "copilot" }

func copilotHeaders() map[string]string {
	return map[string]string{
		"User-Agent":             copilotUserAgent,
		"Editor-Version":         copilotEditorVersion,
		"Editor-Plugin-Version":  copilotPluginVersion,
		"Copilot-Integration-Id": copilotIntegrationID,
		"Accept":                 "application/json",
	}
}

func (p *Copilot) tokenURL() string {
	if p.TokenURL != "" {
		return p.TokenURL
	}
	return defaultCopilotTokenURL
}

// session returns a valid exchanged session, refreshing when needed.
func (p *Copilot) session(ctx context.Context) (*copilotSession, error) {
	p.mu.Lock()
	defer p.mu.Unlock()
	if p.cached != nil && time.Now().Unix()+refreshMargin < p.cached.expires {
		return p.cached, nil
	}
	sess, err := p.exchange(ctx)
	if err != nil {
		return nil, err
	}
	p.cached = sess
	return sess, nil
}

// exchange trades the GitHub token for a short-lived Copilot session token.
func (p *Copilot) exchange(ctx context.Context) (*copilotSession, error) {
	key := p.GitHubToken
	tokenURL := p.TokenURL
	baseOverride := p.BaseURL
	var doc ProviderDoc
	var haveDoc bool
	if (key == "" || tokenURL == "" || baseOverride == "") && p.ProviderName != "" {
		if d, ok := ReadProviderDoc(p.ProviderName); ok {
			doc, haveDoc = d, true
			if key == "" {
				key = d.GitHubToken
			}
			if tokenURL == "" {
				tokenURL = d.TokenURL
			}
			if baseOverride == "" {
				baseOverride = d.BaseURL
			}
		}
	}
	sess, err := p.doExchange(ctx, tokenURL, baseOverride, key)
	if err == nil {
		return sess, nil
	}
	// A construction-time key can be valid for the API yet rejected by the
	// Copilot exchange (classic PATs 404). Fall back to the device-authed
	// store token when it differs, so /connect keeps working without
	// editing config or restarting.
	if haveDoc && doc.GitHubToken != "" && doc.GitHubToken != key {
		if sess2, err2 := p.doExchange(ctx, tokenURL, baseOverride, doc.GitHubToken); err2 == nil {
			return sess2, nil
		}
	}
	return nil, err
}

func (p *Copilot) doExchange(ctx context.Context, tokenURL, baseOverride, key string) (*copilotSession, error) {
	if key == "" {
		return nil, fmt.Errorf("copilot: no github token (api_key_env/api_key_cmd, device auth, or token store)")
	}
	if tokenURL == "" {
		tokenURL = defaultCopilotTokenURL
	}
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, tokenURL, nil)
	if err != nil {
		return nil, err
	}
	req.Header.Set("Authorization", "Bearer "+key)
	req.Header.Set("Accept", "application/json")
	for k, v := range copilotHeaders() {
		req.Header.Set(k, v)
	}
	resp, err := newClient().Do(req)
	if err != nil {
		return nil, err
	}
	defer resp.Body.Close()
	if resp.StatusCode >= 400 {
		var eb strings.Builder
		buf := make([]byte, 1024)
		for {
			n, rerr := resp.Body.Read(buf)
			if n > 0 {
				eb.Write(buf[:n])
			}
			if rerr != nil || eb.Len() > 1024 {
				break
			}
		}
		return nil, fmt.Errorf("copilot token exchange: status %d: %s", resp.StatusCode, eb.String())
	}
	var tr struct {
		Token     string `json:"token"`
		ExpiresAt int64  `json:"expires_at"`
		ExpiresIn int64  `json:"expires_in"`
		Endpoints *struct {
			API string `json:"api"`
		} `json:"endpoints"`
		ProxyEP string `json:"proxy-ep"`
	}
	if err := json.NewDecoder(resp.Body).Decode(&tr); err != nil {
		return nil, err
	}
	if tr.Token == "" {
		return nil, fmt.Errorf("copilot token exchange: empty token")
	}
	exp := tr.ExpiresAt
	if exp == 0 && tr.ExpiresIn > 0 {
		exp = time.Now().Unix() + tr.ExpiresIn
	}
	if exp == 0 {
		exp = time.Now().Unix() + 1500 // documented ~25-30min session
	}
	base := fallbackCopilotBase
	switch {
	case baseOverride != "":
		base = strings.TrimSuffix(baseOverride, "/")
	case tr.ProxyEP != "":
		base = "https://" + strings.TrimSuffix(tr.ProxyEP, "/")
	case tr.Endpoints != nil && tr.Endpoints.API != "":
		base = strings.TrimSuffix(tr.Endpoints.API, "/")
	}
	return &copilotSession{token: tr.Token, expires: exp, baseURL: base}, nil
}

// Chat delegates to the OpenAI-compatible SSE parser once exchanged.
func (p *Copilot) Chat(ctx context.Context, req Request) (<-chan Chunk, error) {
	sess, err := p.session(ctx)
	if err != nil {
		return nil, err
	}
	inner := &OpenAICompat{
		BaseURL: sess.baseURL, APIKey: sess.token,
		Headers: map[string]string{
			"User-Agent":             copilotUserAgent,
			"Editor-Version":         copilotEditorVersion,
			"Editor-Plugin-Version":  copilotPluginVersion,
			"Copilot-Integration-Id": copilotIntegrationID,
			"Accept":                 "text/event-stream",
			"Openai-Intent":          "conversation-edits",
			"X-Initiator":            "agent",
		},
	}
	return inner.Chat(ctx, req)
}

// SetKey installs a freshly authed GitHub token, dropping any cached
// session so the next call re-exchanges immediately (no daemon restart).
func (p *Copilot) SetKey(key string) {
	p.mu.Lock()
	defer p.mu.Unlock()
	p.GitHubToken = key
	p.cached = nil
}

// ListModels returns the account's enabled Copilot models.
func (p *Copilot) ListModels(ctx context.Context) ([]ModelMeta, error) {
	sess, err := p.session(ctx)
	if err != nil {
		return nil, err
	}
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, sess.baseURL+"/models", nil)
	if err != nil {
		return nil, err
	}
	req.Header.Set("Authorization", "Bearer "+sess.token)
	req.Header.Set("Accept", "application/json")
	for k, v := range copilotHeaders() {
		req.Header.Set(k, v)
	}
	resp, err := newClient().Do(req)
	if err != nil {
		return nil, err
	}
	defer resp.Body.Close()
	if resp.StatusCode >= 400 {
		return nil, fmt.Errorf("copilot models: status %d", resp.StatusCode)
	}
	var lr struct {
		Data []struct {
			ModelMeta
			ContextLength int `json:"context_length"`
		} `json:"data"`
	}
	if err := json.NewDecoder(resp.Body).Decode(&lr); err != nil {
		return nil, err
	}
	out := make([]ModelMeta, 0, len(lr.Data))
	for _, m := range lr.Data {
		mm := m.ModelMeta
		if m.ContextLength > mm.ContextWindow {
			mm.ContextWindow = m.ContextLength
		}
		out = append(out, mm)
	}
	return out, nil
}

// ContextWindow returns the gateway-advertised window for a bare model id,
// fetched once from the account's /models and cached. Logged-out or bare
// endpoints report nothing → 0 (unknown, never an error for the caller).
// ponytail: decode tolerates the undocumented shape; unknown keys are ignored.
func (p *Copilot) ContextWindow(ctx context.Context, model string) (int, error) {
	p.ctxMu.Lock()
	defer p.ctxMu.Unlock()
	if w, ok := p.ctxCache[model]; ok {
		return w, nil
	}
	if p.ctxFetched {
		return 0, nil
	}
	p.ctxFetched = true
	sess, err := p.session(ctx)
	if err != nil {
		return 0, err
	}
	cctx, cancel := context.WithTimeout(ctx, 15*time.Second)
	defer cancel()
	models, err := listOpenAIModels(cctx, sess.baseURL+"/models", sess.token, copilotHeaders())
	if err != nil {
		return 0, err
	}
	if p.ctxCache == nil {
		p.ctxCache = map[string]int{}
	}
	for _, m := range models {
		p.ctxCache[m.ID] = m.ContextWindow
	}
	return p.ctxCache[model], nil
}
