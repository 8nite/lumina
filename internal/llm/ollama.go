package llm

import (
	"bufio"
	"context"
	"encoding/json"
	"fmt"
	"net/http"
	"strings"
	"sync"
	"time"
)

// Ollama speaks local /api/chat NDJSON streaming.
type Ollama struct {
	BaseURL string

	mu       sync.Mutex
	ctxCache map[string]int // model name → context_length from /api/show
}

// ContextWindow reads the model's context_length from /api/show
// (e.g. `llama.context_length`), cached per model name.
func (p *Ollama) ContextWindow(ctx context.Context, model string) (int, error) {
	p.mu.Lock()
	defer p.mu.Unlock()
	if w, ok := p.ctxCache[model]; ok {
		return w, nil
	}
	cctx, cancel := context.WithTimeout(ctx, 15*time.Second)
	defer cancel()
	resp, err := postJSON(cctx, strings.TrimSuffix(p.BaseURL, "/")+"/api/show", "", map[string]interface{}{"name": model}, nil)
	if err != nil {
		return 0, err
	}
	var doc struct {
		ModelInfo map[string]interface{} `json:"model_info"`
	}
	if err := json.NewDecoder(resp.Body).Decode(&doc); err != nil {
		resp.Body.Close()
		return 0, err
	}
	resp.Body.Close()
	w := 0
	for k, v := range doc.ModelInfo {
		if k == "context_length" || strings.HasSuffix(k, ".context_length") {
			switch n := v.(type) {
			case float64:
				if n > 0 {
					w = int(n)
				}
			}
		}
		if w > 0 {
			break
		}
	}
	if p.ctxCache == nil {
		p.ctxCache = map[string]int{}
	}
	p.ctxCache[model] = w
	return w, nil
}

func (p *Ollama) Name() string { return "ollama" }

// ListModels returns local models (GET /api/tags).
func (p *Ollama) ListModels(ctx context.Context) ([]ModelMeta, error) {
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, strings.TrimSuffix(p.BaseURL, "/")+"/api/tags", nil)
	if err != nil {
		return nil, err
	}
	resp, err := newClient().Do(req)
	if err != nil {
		return nil, err
	}
	defer resp.Body.Close()
	if resp.StatusCode >= 400 {
		return nil, fmt.Errorf("ollama tags: status %d", resp.StatusCode)
	}
	var lr struct {
		Models []struct {
			Name string `json:"name"`
		} `json:"models"`
	}
	if err := json.NewDecoder(resp.Body).Decode(&lr); err != nil {
		return nil, err
	}
	var out []ModelMeta
	for _, m := range lr.Models {
		out = append(out, ModelMeta{ID: m.Name, Name: m.Name})
	}
	return out, nil
}

func (p *Ollama) Chat(ctx context.Context, req Request) (<-chan Chunk, error) {
	url := strings.TrimSuffix(p.BaseURL, "/") + "/api/chat"
	msgs := make([]map[string]interface{}, 0, len(req.Messages))
	for _, m := range req.Messages {
		msgs = append(msgs, map[string]interface{}{"role": m.Role, "content": m.Content})
	}
	if len(req.Images) > 0 {
		for i := len(msgs) - 1; i >= 0; i-- {
			if msgs[i]["role"] == "user" {
				var imgs []string
				for _, im := range req.Images {
					imgs = append(imgs, im.Data)
				}
				msgs[i]["images"] = imgs
				break
			}
		}
	}
	resp, err := postJSON(ctx, url, "", map[string]interface{}{
		"model": req.Model, "messages": msgs, "stream": true,
	}, nil)
	if err != nil {
		return nil, err
	}
	out := make(chan Chunk, 64)
	go func() {
		defer close(out)
		defer resp.Body.Close()
		sc := bufio.NewScanner(resp.Body)
		sc.Buffer(make([]byte, 1024*1024), 1024*1024)
		for sc.Scan() {
			select {
			case <-ctx.Done():
				return
			default:
			}
			var ev struct {
				Message struct {
					Content string `json:"content"`
				} `json:"message"`
				Done bool `json:"done"`
			}
			if err := json.Unmarshal(sc.Bytes(), &ev); err != nil {
				continue
			}
			c := Chunk{Delta: ev.Message.Content, Done: ev.Done}
			if c.Delta != "" || c.Done {
				select {
				case out <- c:
				case <-ctx.Done():
					return
				}
			}
			if ev.Done {
				return
			}
		}
	}()
	return out, nil
}
