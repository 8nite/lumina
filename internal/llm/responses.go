package llm

import (
	"context"
	"encoding/json"
	"strings"
	"sync"
	"time"
)

// Responses speaks the OpenAI Responses API (/v1/responses SSE):
// `instructions` + `input` request shape; deltas arrive as
// response.output_text.delta; tool args accumulate per output_index.
type Responses struct {
	BaseURL string
	APIKey  string

	mu         sync.Mutex
	ctxCache   map[string]int // model id → advertised window (0 = none)
	ctxFetched bool           // /models was already pulled once
}

func (p *Responses) Name() string { return "openai-responses" }

// ContextWindow returns the gateway-advertised window for a bare model id
// (`context_length`; plain OpenAI has none → 0). The /models document is
// fetched once and cached per provider.
func (p *Responses) ContextWindow(ctx context.Context, model string) (int, error) {
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
	models, err := listOpenAIModels(cctx, strings.TrimSuffix(p.BaseURL, "/")+"/models", p.APIKey, nil)
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

// ListModels returns the account's live models (GET /v1/models).
func (p *Responses) ListModels(ctx context.Context) ([]ModelMeta, error) {
	return listOpenAIModels(ctx, strings.TrimSuffix(p.BaseURL, "/")+"/models", p.APIKey, nil)
}

func (p *Responses) Chat(ctx context.Context, req Request) (<-chan Chunk, error) {
	url := strings.TrimSuffix(p.BaseURL, "/") + "/responses"
	var instructions string
	var input []map[string]interface{}
	for _, m := range req.Messages {
		if m.Role == "system" {
			if instructions != "" {
				instructions += "\n"
			}
			instructions += m.Content
			continue
		}
		if m.Role == "tool" {
			continue // Responses replays via encrypted reasoning, not tool role
		}
		input = append(input, map[string]interface{}{"role": m.Role, "content": m.Content})
	}
	if len(req.Images) > 0 && len(input) > 0 {
		// Vision shape: last user item carries input_text + input_image parts.
		for i := len(input) - 1; i >= 0; i-- {
			if input[i]["role"] == "user" {
				text, _ := input[i]["content"].(string)
				parts := []map[string]interface{}{{"type": "input_text", "text": text}}
				for _, im := range req.Images {
					mt := im.MediaType
					if mt == "" {
						mt = "image/png"
					}
					parts = append(parts, map[string]interface{}{
						"type": "input_image",
						"image_url": "data:" + mt + ";base64," + im.Data,
					})
				}
				input[i]["content"] = parts
				break
			}
		}
	}
	if input == nil {
		input = []map[string]interface{}{}
	}
	body := map[string]interface{}{
		"model": req.Model, "stream": true, "input": input,
	}
	if instructions != "" {
		body["instructions"] = instructions
	}
	if e := NormalizeEffort(req.ReasoningEffort); e != "" {
		body["reasoning"] = map[string]interface{}{"effort": e}
	}
	if len(req.Tools) > 0 {
		var tools []map[string]interface{}
		for _, t := range req.Tools {
			tools = append(tools, map[string]interface{}{
				"type": "function", "name": t.Name,
				"description": t.Description, "parameters": t.Schema,
			})
		}
		body["tools"] = tools
	}
	sid := newSessionID()
	resp, err := postJSON(ctx, url, p.APIKey, body, map[string]string{
		"x-lumina-session": sid,
		// opencode zen routes on this header; harmless elsewhere.
		"x-opencode-session": sid,
	})
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
		// output_index -> partial function call
		type partial struct {
			id, name, args string
		}
		pending := map[int]*partial{}
		_ = readSSE(ctx, resp, func(data string) bool {
			var ev struct {
				Type        string `json:"type"`
				OutputIndex *int   `json:"output_index"`
				Delta       string `json:"delta"`
				Item        *struct {
					Type   string `json:"type"`
					ID     string `json:"id"`
					CallID string `json:"call_id"`
					Name   string `json:"name"`
				} `json:"item"`
				Response *struct {
					Status string `json:"status"`
				} `json:"response"`
			}
			if err := json.Unmarshal([]byte(data), &ev); err != nil {
				return true
			}
			idx := 0
			if ev.OutputIndex != nil {
				idx = *ev.OutputIndex
			}
			switch ev.Type {
			case "response.output_text.delta":
				if ev.Delta != "" {
					return emit(Chunk{Delta: ev.Delta})
				}
			case "response.output_item.added":
				if ev.Item != nil && ev.Item.Type == "function_call" {
					id := ev.Item.CallID
					if id == "" {
						id = ev.Item.ID
					}
					pending[idx] = &partial{id: id, name: ev.Item.Name}
				}
			case "response.function_call_arguments.delta":
				pc, ok := pending[idx]
				if !ok {
					pending[idx] = &partial{args: ev.Delta}
				} else {
					pc.args += ev.Delta
				}
			case "response.completed", "response.incomplete", "response.failed":
				for _, pc := range pending {
					var args map[string]interface{}
					_ = json.Unmarshal([]byte(pc.args), &args)
					emit(Chunk{ToolCalls: []ToolCallRequest{{ID: pc.id, Name: pc.name, Args: args}}})
				}
				pending = map[int]*partial{}
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
