// Package llm provides vendor-neutral streaming adapters (OpenAI-compatible,
// Anthropic-compatible, Ollama) plus the 3-tier role router.
package llm

import (
	"bufio"
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"net/http"
	"strings"
	"time"
)

// Message is one chat turn.
type Message struct {
	Role     string `json:"role"`
	Content  string `json:"content"`
	Thinking string `json:"thinking,omitempty"`
}

// ToolSpec advertises one callable tool to the model.
type ToolSpec struct {
	Name        string                 `json:"name"`
	Description string                 `json:"description"`
	Schema      map[string]interface{} `json:"schema,omitempty"`
}

// ToolCallRequest is a model-requested invocation.
type ToolCallRequest struct {
	ID   string                 `json:"id"`
	Name string                 `json:"name"`
	Args map[string]interface{} `json:"args"`
}

// ImageAttachment is one attached image (base64 payload, never a path).
type ImageAttachment struct {
	Name      string `json:"name"`
	MediaType string `json:"media_type"`
	Data      string `json:"data"` // raw base64 (no data: prefix)
}

// Chunk is one streaming delta.
type Chunk struct {
	Delta     string            `json:"delta"`
	Thinking  string            `json:"thinking,omitempty"`
	ToolCalls []ToolCallRequest `json:"tool_calls,omitempty"`
	Done      bool              `json:"done"`
	Usage     *Usage            `json:"usage,omitempty"`
}

// Usage tallies tokens when the provider reports them.
type Usage struct {
	Input, Output int
}

// Request is a completion call.
type Request struct {
	Model     string     `json:"model"`
	Messages  []Message  `json:"messages"`
	Tools     []ToolSpec `json:"tools,omitempty"`
	MaxTokens int        `json:"max_tokens,omitempty"`
	// Images attaches pictures to the turn (vision-capable models only).
	Images []ImageAttachment `json:"images,omitempty"`
	// ReasoningEffort selects a reasoning variant ("", low, medium, high,
	// xhigh, max). Empty means the provider default.
	ReasoningEffort string `json:"reasoning_effort,omitempty"`
}

// Provider streams a completion.
type Provider interface {
	Chat(ctx context.Context, req Request) (<-chan Chunk, error)
	Name() string
}

// ModelLister is an optional Provider capability: enumerate the account's
// live models (used by /model pickers and `lumina models`).
type ModelLister interface {
	ListModels(ctx context.Context) ([]ModelMeta, error)
}

// ModelMeta describes one live provider model.
type ModelMeta struct {
	ID   string `json:"id"`
	Name string `json:"name"`
}

// Complete collects a full stream into text + tool calls (for tests/headless).
func Complete(ctx context.Context, p Provider, req Request) (string, []ToolCallRequest, error) {
	ch, err := p.Chat(ctx, req)
	if err != nil {
		return "", nil, err
	}
	var sb strings.Builder
	var calls []ToolCallRequest
	for c := range ch {
		sb.WriteString(c.Delta)
		sb.WriteString(c.Thinking)
		calls = append(calls, c.ToolCalls...)
		if c.Done {
			break
		}
		select {
		case <-ctx.Done():
			return sb.String(), calls, ctx.Err()
		default:
		}
	}
	return sb.String(), calls, nil
}

func newClient() *http.Client {
	return &http.Client{Timeout: 5 * time.Minute}
}

// splitModel splits "provider/model" into provider key + model id.
func splitModel(m string) (string, string) {
	if i := strings.Index(m, "/"); i > 0 {
		return m[:i], m[i+1:]
	}
	return "", m
}

// readSSE consumes a text/event-stream body, invoking fn per data: payload.
func readSSE(ctx context.Context, resp *http.Response, fn func(data string) bool) error {
	defer resp.Body.Close()
	sc := bufio.NewScanner(resp.Body)
	sc.Buffer(make([]byte, 1024*1024), 1024*1024)
	var sb strings.Builder
	flush := func() bool {
		if sb.Len() == 0 {
			return true
		}
		d := sb.String()
		sb.Reset()
		if d == "[DONE]" {
			return false
		}
		return fn(d)
	}
	for sc.Scan() {
		select {
		case <-ctx.Done():
			return ctx.Err()
		default:
		}
		line := sc.Text()
		if line == "" {
			if !flush() {
				return nil
			}
			continue
		}
		if strings.HasPrefix(line, "data:") {
			d := strings.TrimSpace(strings.TrimPrefix(line, "data:"))
			if d == "[DONE]" {
				return nil
			}
			sb.WriteString(d)
		}
	}
	flush()
	return sc.Err()
}

func postJSON(ctx context.Context, url, token string, body interface{}, extraHeaders map[string]string) (*http.Response, error) {
	raw, err := json.Marshal(body)
	if err != nil {
		return nil, err
	}
	req, err := http.NewRequestWithContext(ctx, http.MethodPost, url, bytes.NewReader(raw))
	if err != nil {
		return nil, err
	}
	req.Header.Set("Content-Type", "application/json")
	if token != "" {
		req.Header.Set("Authorization", "Bearer "+token)
	}
	for k, v := range extraHeaders {
		req.Header.Set(k, v)
	}
	resp, err := newClient().Do(req)
	if err != nil {
		return nil, err
	}
	if resp.StatusCode >= 400 {
		defer resp.Body.Close()
		var eb bytes.Buffer
		_, _ = eb.ReadFrom(resp.Body)
		return nil, fmt.Errorf("provider %s: status %d: %s", url, resp.StatusCode, eb.String())
	}
	return resp, nil
}
