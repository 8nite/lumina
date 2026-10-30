// Package sdk is the typed Go client for the Lumina daemon HTTP + SSE API.
// The Bubble Tea TUI talks to `lumina serve` exclusively through this.
package sdk

import (
	"bufio"
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"strings"
	"time"
)

// Client talks to one daemon base URL (e.g. http://127.0.0.1:7432).
type Client struct {
	BaseURL string
	Token   string
	http    *http.Client
}

// New creates a client.
func New(baseURL, token string) *Client {
	return &Client{
		BaseURL: strings.TrimSuffix(baseURL, "/"),
		Token:   token,
		http:    &http.Client{Timeout: 2 * time.Minute},
	}
}

// Session mirrors the daemon session view.
type Session struct {
	ID             string `json:"id"`
	WorkspaceDir   string `json:"workspace_dir"`
	Agent          string `json:"agent"`
	PermissionMode string `json:"permission_mode"`
	Model          string `json:"model,omitempty"`
	Title          string `json:"title,omitempty"`
}

// Event is one SSE message.
type Event struct {
	Type      string          `json:"type"`
	SessionID string          `json:"session_id,omitempty"`
	Data      json.RawMessage `json:"data,omitempty"`
}

func (c *Client) req(ctx context.Context, method, path string, body interface{}) (*http.Response, error) {
	var r io.Reader
	if body != nil {
		raw, err := json.Marshal(body)
		if err != nil {
			return nil, err
		}
		r = bytes.NewReader(raw)
	}
	req, err := http.NewRequestWithContext(ctx, method, c.BaseURL+path, r)
	if err != nil {
		return nil, err
	}
	if body != nil {
		req.Header.Set("Content-Type", "application/json")
	}
	if c.Token != "" {
		req.Header.Set("Authorization", "Bearer "+c.Token)
	}
	resp, err := c.http.Do(req)
	if err != nil {
		return nil, err
	}
	if resp.StatusCode >= 400 {
		defer resp.Body.Close()
		var eb bytes.Buffer
		_, _ = eb.ReadFrom(resp.Body)
		return nil, fmt.Errorf("sdk %s %s: status %d: %s", method, path, resp.StatusCode, eb.String())
	}
	return resp, nil
}

func decode(resp *http.Response, v interface{}) error {
	defer resp.Body.Close()
	return json.NewDecoder(resp.Body).Decode(v)
}

// CreateSession starts a session in workspaceDir.
func (c *Client) CreateSession(ctx context.Context, workspaceDir, agent, perm string) (*Session, error) {
	resp, err := c.req(ctx, http.MethodPost, "/api/v1/sessions", map[string]string{
		"workspace_dir": workspaceDir, "agent": agent, "permission_mode": perm,
	})
	if err != nil {
		return nil, err
	}
	var s Session
	return &s, decode(resp, &s)
}

// LatestSession resolves -c/--continue (most recent session for directory).
func (c *Client) LatestSession(ctx context.Context, workspaceDir string) (*Session, error) {
	resp, err := c.req(ctx, http.MethodGet, "/api/v1/sessions?latest=1&directory="+url.QueryEscape(workspaceDir), nil)
	if err != nil {
		return nil, err
	}
	var s Session
	return &s, decode(resp, &s)
}

// ListSessions lists workspace/all sessions.
func (c *Client) ListSessions(ctx context.Context, workspaceDir string) ([]*Session, error) {
	p := "/api/v1/sessions"
	if workspaceDir != "" {
		p += "?directory=" + url.QueryEscape(workspaceDir)
	}
	resp, err := c.req(ctx, http.MethodGet, p, nil)
	if err != nil {
		return nil, err
	}
	var out []*Session
	return out, decode(resp, &out)
}

// UpdateSession live-patches agent / permission_mode / model / title.
func (c *Client) UpdateSession(ctx context.Context, id string, patch map[string]string) (*Session, error) {
	resp, err := c.req(ctx, http.MethodPatch, "/api/v1/sessions/"+id, patch)
	if err != nil {
		return nil, err
	}
	var s Session
	return &s, decode(resp, &s)
}

// ForkSession branches a session at turnID.
func (c *Client) ForkSession(ctx context.Context, id string) (*Session, error) {
	resp, err := c.req(ctx, http.MethodPost, "/api/v1/sessions/"+id+"/fork", map[string]string{})
	if err != nil {
		return nil, err
	}
	var s Session
	return &s, decode(resp, &s)
}

// PromptAttachment is one image attached to a turn (base64, never a path).
type PromptAttachment struct {
	Name      string `json:"name"`
	MediaType string `json:"media_type"`
	Data      string `json:"data"`
}

// PromptInput is one submitted turn.
type PromptInput struct {
	Text   string             `json:"text"`
	Images []PromptAttachment `json:"images,omitempty"`
	Effort string             `json:"effort,omitempty"`
	// Delivery selects inbox behavior while a turn is running
	// (steer/queue): steer (default) joins the active
	// turn; queue chains after it completes.
	Delivery string `json:"delivery,omitempty"`
}

// Prompt submits a turn; progress arrives over Subscribe. Delivery
// selects steer (default) or queue when a turn is already running.
func (c *Client) Prompt(ctx context.Context, id string, in PromptInput) error {
	resp, err := c.req(ctx, http.MethodPost, "/api/v1/sessions/"+id+"/prompt", in)
	if err != nil {
		return err
	}
	resp.Body.Close()
	return nil
}

// Revert/Unrevert map to shadow-Git /undo and /redo.
func (c *Client) Revert(ctx context.Context, id string) error {
	resp, err := c.req(ctx, http.MethodPost, "/api/v1/sessions/"+id+"/revert", map[string]string{})
	if err != nil {
		return err
	}
	resp.Body.Close()
	return nil
}

// Unrevert re-applies an undone turn.
func (c *Client) Unrevert(ctx context.Context, id string) error {
	resp, err := c.req(ctx, http.MethodPost, "/api/v1/sessions/"+id+"/unrevert", map[string]string{})
	if err != nil {
		return err
	}
	resp.Body.Close()
	return nil
}

// AnswerPermission resolves a permission prompt.
func (c *Client) AnswerPermission(ctx context.Context, reqID, decision string) error {
	resp, err := c.req(ctx, http.MethodPost, "/api/v1/permissions/"+reqID, map[string]string{"decision": decision})
	if err != nil {
		return err
	}
	resp.Body.Close()
	return nil
}

// AnswerQuestion resolves a question prompt with one string list per
// question (multiple:true questions may carry several).
func (c *Client) AnswerQuestion(ctx context.Context, reqID string, answers [][]string) error {
	resp, err := c.req(ctx, http.MethodPost, "/api/v1/questions/"+reqID, map[string]interface{}{"answers": answers})
	if err != nil {
		return err
	}
	resp.Body.Close()
	return nil
}

// DismissQuestion closes a question prompt without answers.
func (c *Client) DismissQuestion(ctx context.Context, reqID string) error {
	resp, err := c.req(ctx, http.MethodPost, "/api/v1/questions/"+reqID, map[string]bool{"dismissed": true})
	if err != nil {
		return err
	}
	resp.Body.Close()
	return nil
}

// ProviderInfo is one configured provider.
type ProviderInfo struct {
	Name     string `json:"name"`
	Protocol string `json:"protocol"`
}

// ListProviders returns configured providers with protocols.
func (c *Client) ListProviders(ctx context.Context) ([]ProviderInfo, error) {
	resp, err := c.req(ctx, http.MethodGet, "/api/v1/providers", nil)
	if err != nil {
		return nil, err
	}
	var out struct {
		Providers []ProviderInfo `json:"providers"`
	}
	return out.Providers, decode(resp, &out)
}

// ModelInfo is one live provider model.
type ModelInfo struct {
	ID   string `json:"id"`
	Name string `json:"name"`
}

// ListModels returns live models for a provider (copilot gateway).
func (c *Client) ListModels(ctx context.Context, provider string) ([]ModelInfo, error) {
	resp, err := c.req(ctx, http.MethodGet, "/api/v1/models?provider="+url.QueryEscape(provider), nil)
	if err != nil {
		return nil, err
	}
	var out struct {
		Models []ModelInfo `json:"models"`
	}
	return out.Models, decode(resp, &out)
}

// SetRouterModel hot-swaps a tier model (model-persist).
func (c *Client) SetRouterModel(ctx context.Context, role, model string) error {
	resp, err := c.req(ctx, http.MethodPut, "/api/v1/router", map[string]string{role: model})
	if err != nil {
		return err
	}
	resp.Body.Close()
	return nil
}

// DeviceAuth carries device web-auth codes (never a token).
type DeviceAuth struct {
	DeviceCode string `json:"device_code"`
	UserCode   string `json:"user_code"`
	URI        string `json:"verification_uri"`
	ExpiresIn  int    `json:"expires_in"`
	Interval   int    `json:"interval"`
}

// DeviceStart begins provider web-auth.
func (c *Client) DeviceStart(ctx context.Context, provider, githubURL string) (*DeviceAuth, error) {
	resp, err := c.req(ctx, http.MethodPost, "/api/v1/auth/device/start",
		map[string]string{"provider": provider, "github_url": githubURL})
	if err != nil {
		return nil, err
	}
	var d DeviceAuth
	return &d, decode(resp, &d)
}

// DevicePoll checks web-auth approval; "authorized" means the daemon stored
// the token (it is never returned).
func (c *Client) DevicePoll(ctx context.Context, provider, deviceCode, githubURL string) (string, error) {
	resp, err := c.req(ctx, http.MethodPost, "/api/v1/auth/device/poll",
		map[string]string{"provider": provider, "device_code": deviceCode, "github_url": githubURL})
	if err != nil {
		return "", err
	}
	var out struct {
		Status string `json:"status"`
		Error  string `json:"error"`
	}
	if err := decode(resp, &out); err != nil {
		return "", err
	}
	if out.Error != "" {
		return out.Status, fmt.Errorf("%s", out.Error)
	}
	return out.Status, nil
}

// Subscribe streams daemon events until ctx ends.
func (c *Client) Subscribe(ctx context.Context, fn func(Event)) error {
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, c.BaseURL+"/api/v1/events", nil)
	if err != nil {
		return err
	}
	if c.Token != "" {
		req.Header.Set("Authorization", "Bearer "+c.Token)
	}
	req.Header.Set("Accept", "text/event-stream")
	resp, err := c.http.Do(req)
	if err != nil {
		return err
	}
	defer resp.Body.Close()
	sc := bufio.NewScanner(resp.Body)
	sc.Buffer(make([]byte, 1024*1024), 1024*1024)
	var typ, data string
	for sc.Scan() {
		line := sc.Text()
		if line == "" {
			if data != "" {
				fn(Event{Type: typ, Data: json.RawMessage(data)})
				typ, data = "", ""
			}
			continue
		}
		if strings.HasPrefix(line, "event:") {
			typ = strings.TrimSpace(strings.TrimPrefix(line, "event:"))
		} else if strings.HasPrefix(line, "data:") {
			d := strings.TrimSpace(strings.TrimPrefix(line, "data:"))
			var e Event
			if err := json.Unmarshal([]byte(d), &e); err == nil && e.Type != "" {
				fn(e)
			} else {
				data = d
			}
		}
	}
	return sc.Err()
}
