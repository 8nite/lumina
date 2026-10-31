package llm

import (
	"context"
	"encoding/json"
	"strings"
	"sync"
	"time"
)

// Anthropic speaks native /v1/messages SSE with thinking blocks + prompt cache.
type Anthropic struct {
	BaseURL string
	APIKey  string

	mu         sync.Mutex
	ctxCache   map[string]int // model id → advertised window (0 = none)
	ctxFetched bool           // /models was already pulled once
}

func (p *Anthropic) Name() string { return "anthropic-compatible" }

// ContextWindow returns the gateway-advertised window for a bare model id
// (OpenRouter-style `context_length`; plain Anthropic has none → 0). The
// /v1/models document is fetched once and cached per provider.
func (p *Anthropic) ContextWindow(ctx context.Context, model string) (int, error) {
	p.mu.Lock()
	defer p.mu.Unlock()
	if w, ok := p.ctxCache[model]; ok {
		return w, nil
	}
	if p.ctxFetched {
		return 0, nil
	}
	p.ctxFetched = true
	cctx, cancel := context.WithTimeout(ctx, 15*time.Second)
	defer cancel()
	models, err := listOpenAIModels(cctx, strings.TrimSuffix(p.BaseURL, "/")+"/v1/models", p.APIKey, nil)
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

// ListModels returns the account's live models (GET /v1/models, same
// envelope as OpenAI).
func (p *Anthropic) ListModels(ctx context.Context) ([]ModelMeta, error) {
	return listOpenAIModels(ctx, strings.TrimSuffix(p.BaseURL, "/")+"/v1/models", p.APIKey, nil)
}

func (p *Anthropic) Chat(ctx context.Context, req Request) (<-chan Chunk, error) {
	url := strings.TrimSuffix(p.BaseURL, "/") + "/v1/messages"
	var system string
	var msgs []map[string]interface{}
	for _, m := range req.Messages {
		if m.Role == "system" {
			system += m.Content
			continue
		}
		if m.Role == "tool" {
			continue
		}
		content := []map[string]interface{}{{ "type": "text", "text": m.Content }}
		for _, im := range req.Images {
			if m.Role != "user" {
				break
			}
			mt := im.MediaType
			if mt == "" {
				mt = "image/png"
			}
			content = append(content, map[string]interface{}{
				"type": "image",
				"source": map[string]interface{}{
					"type": "base64", "media_type": mt, "data": im.Data,
				},
			})
		}
		msgs = append(msgs, map[string]interface{}{"role": m.Role, "content": content})
	}
	if msgs == nil {
		msgs = []map[string]interface{}{}
	}
	body := map[string]interface{}{
		"model": req.Model, "messages": msgs, "stream": true,
	}
	maxTokens := maxOr(req.MaxTokens, 4096)
	if budget := EffortBudget(req.ReasoningEffort); budget > 0 {
		if maxTokens < budget+1024 {
			maxTokens = budget + 1024
		}
		body["thinking"] = map[string]interface{}{"type": "enabled", "budget_tokens": budget}
	}
	body["max_tokens"] = maxTokens
	if system != "" {
		body["system"] = system
	}
	if len(req.Tools) > 0 {
		var tools []map[string]interface{}
		for _, t := range req.Tools {
			schema := t.Schema
			if schema == nil {
				schema = map[string]interface{}{"type": "object"}
			}
			tools = append(tools, map[string]interface{}{
				"name": t.Name, "description": t.Description,
				"input_schema": schema,
			})
		}
		body["tools"] = tools
	}
	headers := map[string]string{"x-api-key": p.APIKey, "anthropic-version": "2023-06-01"}
	resp, err := postJSON(ctx, url, "", body, headers)
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
		// output_index -> partial tool call
		type partial struct {
			id, name, json string
			isTool         bool
		}
		pending := map[int]*partial{}
		_ = readSSE(ctx, resp, func(data string) bool {
			var ev struct {
				Type  string `json:"type"`
				Index *int   `json:"index"`
				Delta *struct {
					Type     string `json:"type"`
					Text     string `json:"text"`
					Thinking string `json:"thinking"`
					Partial  string `json:"partial_json"`
				} `json:"delta"`
				Block *struct {
					Type string `json:"type"`
					ID   string `json:"id"`
					Name string `json:"name"`
				} `json:"content_block"`
			}
			if err := json.Unmarshal([]byte(data), &ev); err != nil {
				return true
			}
			idx := 0
			if ev.Index != nil {
				idx = *ev.Index
			}
			switch ev.Type {
			case "content_block_start":
				if ev.Block != nil && ev.Block.Type == "tool_use" {
					pending[idx] = &partial{id: ev.Block.ID, name: ev.Block.Name, isTool: true}
				} else {
					pending[idx] = &partial{}
				}
			case "content_block_delta":
				if ev.Delta == nil {
					return true
				}
				switch ev.Delta.Type {
				case "thinking_delta":
					return emit(Chunk{Thinking: ev.Delta.Thinking})
				case "input_json_delta":
					pc, ok := pending[idx]
					if !ok {
						pending[idx] = &partial{json: ev.Delta.Partial, isTool: true}
					} else {
						pc.json += ev.Delta.Partial
						pc.isTool = true
					}
				default:
					if ev.Delta.Text != "" {
						return emit(Chunk{Delta: ev.Delta.Text})
					}
				}
			case "content_block_stop":
				if pc, ok := pending[idx]; ok && pc.isTool && pc.name != "" {
					var args map[string]interface{}
					_ = json.Unmarshal([]byte(pc.json), &args)
					emit(Chunk{ToolCalls: []ToolCallRequest{{ID: pc.id, Name: pc.name, Args: args}}})
				}
				delete(pending, idx)
			case "message_stop":
				emit(Chunk{Done: true})
				return false
			}
			return true
		})
		select {
		case out <- Chunk{Done: true}:
		default:
		}
	}()
	return out, nil
}

func maxOr(v, d int) int {
	if v > 0 {
		return v
	}
	return d
}
