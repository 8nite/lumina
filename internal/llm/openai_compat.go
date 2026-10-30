package llm

import (
	"context"
	"encoding/json"
	"fmt"
	"net/http"
	"sort"
	"strings"
)

// OpenAICompat speaks /v1/chat/completions (+ /v1/responses fallback) SSE.
type OpenAICompat struct {
	BaseURL string
	APIKey  string
	// Headers are extra request headers (Copilot editor/integration headers).
	Headers map[string]string
}

func (p *OpenAICompat) Name() string { return "openai-compatible" }

// ListModels returns the account's live models (GET /v1/models).
func (p *OpenAICompat) ListModels(ctx context.Context) ([]ModelMeta, error) {
	return listOpenAIModels(ctx, strings.TrimSuffix(p.BaseURL, "/")+"/models", p.APIKey, p.Headers)
}

func listOpenAIModels(ctx context.Context, url, key string, headers map[string]string) ([]ModelMeta, error) {
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, url, nil)
	if err != nil {
		return nil, err
	}
	if key != "" {
		req.Header.Set("Authorization", "Bearer "+key)
	}
	for k, v := range headers {
		if strings.EqualFold(k, "authorization") {
			continue
		}
		req.Header.Set(k, v)
	}
	// Gateways (opencode zen) reject generic library UAs and require a
	// routing session even for GET /models.
	if req.Header.Get("User-Agent") == "" {
		req.Header.Set("User-Agent", LuminaUserAgent)
	}
	if req.Header.Get("x-opencode-session") == "" {
		sid := newSessionID()
		req.Header.Set("x-opencode-session", sid)
		req.Header.Set("x-lumina-session", sid)
	}
	resp, err := newClient().Do(req)
	if err != nil {
		return nil, err
	}
	defer resp.Body.Close()
	if resp.StatusCode >= 400 {
		return nil, fmt.Errorf("list models: status %d", resp.StatusCode)
	}
	var lr struct {
		Data []ModelMeta `json:"data"`
	}
	if err := json.NewDecoder(resp.Body).Decode(&lr); err != nil {
		return nil, err
	}
	return lr.Data, nil
}

func (p *OpenAICompat) Chat(ctx context.Context, req Request) (<-chan Chunk, error) {
	url := strings.TrimSuffix(p.BaseURL, "/") + "/chat/completions"
	msgs := make([]map[string]interface{}, 0, len(req.Messages))
	for _, m := range req.Messages {
		msgs = append(msgs, map[string]interface{}{"role": m.Role, "content": m.Content})
	}
	if len(req.Images) > 0 {
		// Vision shape: last user message carries text + image_url parts.
		for i := len(msgs) - 1; i >= 0; i-- {
			if msgs[i]["role"] == "user" {
				parts := []map[string]interface{}{
					{"type": "text", "text": msgs[i]["content"]},
				}
				for _, im := range req.Images {
					parts = append(parts, map[string]interface{}{
						"type":      "image_url",
						"image_url": map[string]string{"url": "data:" + im.MediaType + ";base64," + im.Data},
					})
				}
				msgs[i]["content"] = parts
				break
			}
		}
	}

	body := map[string]interface{}{
		"model": req.Model, "messages": msgs, "stream": true,
	}
	if req.MaxTokens > 0 {
		body["max_tokens"] = req.MaxTokens
	}
	if req.ReasoningEffort != "" {
		body["reasoning_effort"] = req.ReasoningEffort
	}
	if len(req.Tools) > 0 {
		tools := make([]map[string]interface{}, 0, len(req.Tools))
		for _, t := range req.Tools {
			tools = append(tools, map[string]interface{}{
				"type": "function",
				"function": map[string]interface{}{
					"name": t.Name, "description": t.Description,
					"parameters": t.Schema,
				},
			})
		}
		body["tools"] = tools
	}
	// Session routing headers: lumina's own plus the opencode zen gateway's
	// (x-opencode-session), required when BaseURL points at opencode.ai/zen.
	headers := map[string]string{}
	for k, v := range p.Headers {
		headers[k] = v
	}
	sid := ""
	for _, k := range []string{"x-opencode-session", "x-lumina-session"} {
		if _, ok := headers[k]; !ok {
			if sid == "" {
				sid = newSessionID()
			}
			headers[k] = sid
		}
	}
	resp, err := postJSON(ctx, url, p.APIKey, body, headers)
	if err != nil {
		return nil, err
	}
	out := make(chan Chunk, 64)
	go func() {
		defer close(out)
		emit := func(c Chunk) bool {
			select {
			case out <- c:
				return true
			case <-ctx.Done():
				return false
			}
		}
		// Tool calls stream as fragments: the first chunk for an index
		// carries id+name, later chunks carry argument substrings only.
		// Accumulate by index and flush complete calls on finish.
		type pendingCall struct {
			id, name string
			args     strings.Builder
		}
		pending := map[int]*pendingCall{}
		flush := func() []ToolCallRequest {
			var idxs []int
			for i := range pending {
				idxs = append(idxs, i)
			}
			sort.Ints(idxs)
			var out []ToolCallRequest
			for _, i := range idxs {
				pc := pending[i]
				var args map[string]interface{}
				_ = json.Unmarshal([]byte(pc.args.String()), &args)
				out = append(out, ToolCallRequest{ID: pc.id, Name: pc.name, Args: args})
				delete(pending, i)
			}
			return out
		}
		_ = readSSE(ctx, resp, func(data string) bool {
			var ev struct {
				Choices []struct {
					Delta struct {
						Content          *string `json:"content"`
						ReasoningContent *string `json:"reasoning_content"`
						ToolCalls        []struct {
							Index    int    `json:"index"`
							ID       string `json:"id"`
							Function struct {
								Name      string `json:"name"`
								Arguments string `json:"arguments"`
							} `json:"function"`
						} `json:"tool_calls"`
					} `json:"delta"`
					FinishReason *string `json:"finish_reason"`
				} `json:"choices"`
			}
			if err := json.Unmarshal([]byte(data), &ev); err != nil || len(ev.Choices) == 0 {
				return true
			}
			d := ev.Choices[0].Delta
			var c Chunk
			if d.Content != nil {
				c.Delta = *d.Content
			}
			if d.ReasoningContent != nil {
				c.Thinking = *d.ReasoningContent
			}
			for _, tc := range d.ToolCalls {
				pc, ok := pending[tc.Index]
				if !ok {
					pc = &pendingCall{}
					pending[tc.Index] = pc
				}
				if tc.ID != "" {
					pc.id = tc.ID
				}
				if tc.Function.Name != "" {
					pc.name = tc.Function.Name
				}
				pc.args.WriteString(tc.Function.Arguments)
			}
			if ev.Choices[0].FinishReason != nil {
				c.ToolCalls = flush()
				c.Done = true
				emit(c)
				return false
			}
			if c.Delta != "" || c.Thinking != "" {
				return emit(c)
			}
			return true
		})
		for _, tc := range flush() {
			select {
			case out <- Chunk{ToolCalls: []ToolCallRequest{tc}}:
			default:
			}
		}
		select {
		case out <- Chunk{Done: true}:
		default:
		}
	}()
	return out, nil
}
