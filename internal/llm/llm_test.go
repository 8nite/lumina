package llm

import (
	"context"
	"encoding/json"
	"fmt"
	"net/http"
	"net/http/httptest"
	"strings"
	"sync"
	"testing"

	"github.com/8nite/lumina/internal/config"
)

func testConfig() *config.Config {
	c := config.Defaults()
	c.Providers = map[string]config.ProviderConfig{
		"local_ollama": {Protocol: "ollama", BaseURL: "http://127.0.0.1:11434"},
		"openrouter":   {Protocol: "openai-compatible", BaseURL: "https://openrouter.ai/api/v1"},
	}
	c.Router = config.RouterConfig{
		Fast:     "local_ollama/qwen2.5-coder:7b",
		Executor: "openrouter/deepseek/deepseek-r1",
		Improver: "openrouter/deepseek/deepseek-r1",
	}
	return c
}

func TestPlannerFallsBackToExecutor(t *testing.T) {
	r, err := NewRouter(testConfig())
	if err != nil {
		t.Fatal(err)
	}
	if got := r.ModelFor("planner", "plan"); got != "openrouter/deepseek/deepseek-r1" {
		t.Fatalf("planner fallback = %q", got)
	}
	if got := r.ModelFor("fast", "build"); got == "" {
		t.Fatal("fast tier must resolve")
	}
}

func TestOpenAICompatStreaming(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "text/event-stream")
		fmt.Fprint(w, "data: {\"choices\":[{\"delta\":{\"content\":\"hello \"}}]}\n\n")
		fmt.Fprint(w, "data: {\"choices\":[{\"delta\":{\"content\":\"world\"},\"finish_reason\":\"stop\"}]}\n\n")
	}))
	defer srv.Close()
	p := &OpenAICompat{BaseURL: srv.URL}
	text, _, err := Complete(context.Background(), p, Request{Model: "m", Messages: []Message{{Role: "user", Content: "hi"}}})
	if err != nil {
		t.Fatal(err)
	}
	if text != "hello world" {
		t.Fatalf("got %q", text)
	}
}

func TestRejectsGoogleProvider(t *testing.T) {
	c := config.Defaults()
	c.Providers = map[string]config.ProviderConfig{
		"g": {Protocol: "openai-compatible", BaseURL: "https://generativelanguage.googleapis.com/v1"},
	}
	if _, err := NewRouter(c); err == nil {
		t.Fatal("expected google rejection")
	}
}

func TestResponsesStreaming(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path != "/responses" {
			t.Errorf("wrong path %s", r.URL.Path)
		}
		w.Header().Set("Content-Type", "text/event-stream")
		fmt.Fprint(w, "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"ready\"}\n\n")
		fmt.Fprint(w, "event: response.completed\ndata: {\"type\":\"response.completed\"}\n\n")
	}))
	defer srv.Close()
	p := &Responses{BaseURL: srv.URL}
	text, _, err := Complete(context.Background(), p, Request{Model: "m", Messages: []Message{{Role: "user", Content: "hi"}}})
	if err != nil {
		t.Fatal(err)
	}
	if text != "ready" {
		t.Fatalf("got %q", text)
	}
}

func TestResponsesFunctionCall(t *testing.T) {
	send := func(w http.ResponseWriter, typ string, obj map[string]interface{}) {
		raw, _ := json.Marshal(obj)
		fmt.Fprintf(w, "event: %s\ndata: %s\n\n", typ, raw)
	}
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "text/event-stream")
		send(w, "response.output_item.added", map[string]interface{}{
			"type": "response.output_item.added", "output_index": 0,
			"item": map[string]interface{}{"type": "function_call", "id": "c1", "call_id": "call_1", "name": "read"},
		})
		send(w, "response.function_call_arguments.delta", map[string]interface{}{
			"type": "response.function_call_arguments.delta", "output_index": 0, "delta": "{\"path\"",
		})
		send(w, "response.function_call_arguments.delta", map[string]interface{}{
			"type": "response.function_call_arguments.delta", "output_index": 0, "delta": ":\"a.txt\"}",
		})
		send(w, "response.completed", map[string]interface{}{"type": "response.completed"})
	}))
	defer srv.Close()
	p := &Responses{BaseURL: srv.URL}
	_, calls, err := Complete(context.Background(), p, Request{Model: "m", Messages: []Message{{Role: "user", Content: "read it"}}})
	if err != nil {
		t.Fatal(err)
	}
	if len(calls) != 1 || calls[0].Name != "read" || calls[0].Args["path"] != "a.txt" {
		t.Fatalf("calls=%+v", calls)
	}
}

func TestAnthropicToolUse(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Header.Get("x-api-key") == "" {
			t.Error("missing x-api-key")
		}
		w.Header().Set("Content-Type", "text/event-stream")
		fmt.Fprint(w, "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"glob\"}}\n\n")
		fmt.Fprint(w, "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"pattern\\\"\"}}\n\n")
		fmt.Fprint(w, "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\": \\\"**/*.go\\\"}\" }}\n\n")
		fmt.Fprint(w, "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":1}\n\n")
		fmt.Fprint(w, "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"plan\"}}\n\n")
		fmt.Fprint(w, "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n")
	}))
	defer srv.Close()
	p := &Anthropic{BaseURL: srv.URL, APIKey: "k"}
	text, calls, err := Complete(context.Background(), p, Request{Model: "m", Messages: []Message{{Role: "user", Content: "find"}}})
	if err != nil {
		t.Fatal(err)
	}
	if text != "plan" {
		t.Fatalf("thinking text=%q", text)
	}
	if len(calls) != 1 || calls[0].Name != "glob" || calls[0].Args["pattern"] != "**/*.go" {
		t.Fatalf("calls=%+v", calls)
	}
}

func TestOpenAIListModels(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path != "/models" {
			t.Errorf("path = %s", r.URL.Path)
		}
		fmt.Fprint(w, `{"data":[{"id":"gpt-4o","name":"GPT-4o"}]}`)
	}))
	defer srv.Close()
	p := &OpenAICompat{BaseURL: srv.URL}
	ms, err := p.ListModels(context.Background())
	if err != nil || len(ms) != 1 || ms[0].ID != "gpt-4o" {
		t.Fatalf("models=%+v err=%v", ms, err)
	}
}

func TestOllamaListModels(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path != "/api/tags" {
			t.Errorf("path = %s", r.URL.Path)
		}
		fmt.Fprint(w, `{"models":[{"name":"qwen3:8b"}]}`)
	}))
	defer srv.Close()
	p := &Ollama{BaseURL: srv.URL}
	ms, err := p.ListModels(context.Background())
	if err != nil || len(ms) != 1 || ms[0].ID != "qwen3:8b" {
		t.Fatalf("models=%+v err=%v", ms, err)
	}
}

func TestOpenAIImageAndEffort(t *testing.T) {
	var body map[string]interface{}
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		_ = json.NewDecoder(r.Body).Decode(&body)
		w.Header().Set("Content-Type", "text/event-stream")
		fmt.Fprint(w, "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n")
	}))
	defer srv.Close()
	p := &OpenAICompat{BaseURL: srv.URL}
	_, _, err := Complete(context.Background(), p, Request{
		Model: "m", Messages: []Message{{Role: "user", Content: "what is this?"}},
		Images:          []ImageAttachment{{Name: "a.png", MediaType: "image/png", Data: "QUJD"}},
		ReasoningEffort: "xhigh",
	})
	if err != nil {
		t.Fatal(err)
	}
	if body["reasoning_effort"] != "xhigh" {
		t.Fatalf("missing reasoning_effort: %v", body)
	}
	msgs, _ := body["messages"].([]interface{})
	last, _ := msgs[len(msgs)-1].(map[string]interface{})
	parts, _ := last["content"].([]interface{})
	if len(parts) != 2 {
		t.Fatalf("expected text+image parts, got %v", last["content"])
	}
	img, _ := parts[1].(map[string]interface{})
	iu, _ := img["image_url"].(map[string]interface{})
	if iu["url"] != "data:image/png;base64,QUJD" {
		t.Fatalf("bad image_url: %v", iu)
	}
}

func TestResponsesImageAndEffort(t *testing.T) {
	var body map[string]interface{}
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		_ = json.NewDecoder(r.Body).Decode(&body)
		w.Header().Set("Content-Type", "text/event-stream")
		fmt.Fprint(w, "event: response.completed\ndata: {\"type\":\"response.completed\"}\n\n")
	}))
	defer srv.Close()
	p := &Responses{BaseURL: srv.URL}
	_, _, err := Complete(context.Background(), p, Request{
		Model: "m", Messages: []Message{{Role: "user", Content: "describe"}},
		Images:          []ImageAttachment{{Name: "b.jpg", MediaType: "image/jpeg", Data: "RkI="}},
		ReasoningEffort: "max",
	})
	if err != nil {
		t.Fatal(err)
	}
	if r, ok := body["reasoning"].(map[string]interface{}); !ok || r["effort"] != "max" {
		t.Fatalf("missing reasoning.effort: %v", body["reasoning"])
	}
	inp, _ := body["input"].([]interface{})
	if len(inp) != 1 {
		t.Fatalf("input = %v", body["input"])
	}
	item, _ := inp[0].(map[string]interface{})
	parts, _ := item["content"].([]interface{})
	if len(parts) != 2 {
		t.Fatalf("expected 2 content parts, got %v", item["content"])
	}
	p1, _ := parts[1].(map[string]interface{})
	if p1["type"] != "input_image" {
		t.Fatalf("second part = %v", p1)
	}
}

func TestAnthropicListModels(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path != "/v1/models" {
			t.Errorf("path = %s", r.URL.Path)
		}
		fmt.Fprint(w, `{"data":[{"id":"claude-sonnet-4-5","name":"Claude Sonnet"}]}`)
	}))
	defer srv.Close()
	p := &Anthropic{BaseURL: srv.URL}
	ms, err := p.ListModels(context.Background())
	if err != nil || len(ms) != 1 || ms[0].ID != "claude-sonnet-4-5" {
		t.Fatalf("models=%+v err=%v", ms, err)
	}
	// Every protocol must satisfy ModelLister so any connected provider
	// appears in pickers with zero per-protocol code.
	for name, prov := range map[string]Provider{
		"openai":    &OpenAICompat{},
		"responses": &Responses{},
		"anthropic": &Anthropic{},
		"ollama":    &Ollama{},
		"copilot":   &Copilot{},
	} {
		if _, ok := prov.(ModelLister); !ok {
			t.Fatalf("%s does not implement ModelLister", name)
		}
	}
}

// TestRoutingSessionHeaders proves every gateway request carries both the
// lumina routing header and the opencode zen routing header (zen 400s
// without x-opencode-session: MissingSessionID).
func TestRoutingSessionHeaders(t *testing.T) {
	var mu sync.Mutex
	got := map[string]map[string]string{}
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		mu.Lock()
		got[r.URL.Path] = map[string]string{
			"lumina":   r.Header.Get("x-lumina-session"),
			"opencode": r.Header.Get("x-opencode-session"),
		}
		mu.Unlock()
		if strings.HasSuffix(r.URL.Path, "/models") {
			w.Header().Set("Content-Type", "application/json")
			fmt.Fprint(w, `{"data":[]}`)
			return
		}
		w.Header().Set("Content-Type", "text/event-stream")
		fmt.Fprint(w, "data: "+`{"choices":[{"delta":{},"finish_reason":"stop"}]}`+"\n\n")
	}))
	defer srv.Close()
	ctx := context.Background()
	if _, err := listOpenAIModels(ctx, srv.URL+"/models", "k", nil); err != nil {
		t.Fatal(err)
	}
	oc := &OpenAICompat{BaseURL: srv.URL, APIKey: "k"}
	ch, err := oc.Chat(ctx, Request{Model: "m", Messages: []Message{{Role: "user", Content: "hi"}}})
	if err != nil {
		t.Fatal(err)
	}
	for range ch {
	}
	rp := &Responses{BaseURL: srv.URL, APIKey: "k"}
	ch, err = rp.Chat(ctx, Request{Model: "m", Messages: []Message{{Role: "user", Content: "hi"}}})
	if err != nil {
		t.Fatal(err)
	}
	for range ch {
	}
	for _, p := range []string{"/models", "/chat/completions", "/responses"} {
		mu.Lock()
		h := got[p]
		mu.Unlock()
		if h["lumina"] == "" || h["opencode"] == "" {
			t.Fatalf("%s missing routing headers: %v", p, h)
		}
	}
}

func TestNormalizeEffort(t *testing.T) {
	cases := map[string]string{"": "", "auto": "", "AUTO": "", "  high  ": "high", "XHIGH": "xhigh", "Max": "max", "low": "low", "medium": "medium"}
	for in, want := range cases {
		if got := NormalizeEffort(in); got != want {
			t.Fatalf("NormalizeEffort(%q)=%q want %q", in, got, want)
		}
		if !ValidEffort(in) {
			t.Fatalf("ValidEffort(%q)=false want true", in)
		}
	}
	for _, bad := range []string{"x-high", "ultra", "high!", "10"} {
		if ValidEffort(bad) {
			t.Fatalf("ValidEffort(%q)=true want false", bad)
		}
		if got := NormalizeEffort(bad); got != "" {
			t.Fatalf("NormalizeEffort(%q)=%q want \"\"", bad, got)
		}
	}
	if EffortLabel("") != "auto" || EffortLabel("xhigh") != "xhigh" {
		t.Fatalf("EffortLabel broken: %q %q", EffortLabel(""), EffortLabel("xhigh"))
	}
	if EffortBudget("") != 0 || EffortBudget("low") != 1024 || EffortBudget("max") != 32000 {
		t.Fatalf("EffortBudget broken")
	}
}

func TestOpenAICompatEffortNormalized(t *testing.T) {
	var body map[string]interface{}
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		_ = json.NewDecoder(r.Body).Decode(&body)
		w.Header().Set("Content-Type", "text/event-stream")
		fmt.Fprint(w, "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n")
	}))
	defer srv.Close()
	p := &OpenAICompat{BaseURL: srv.URL}
	// auto omits the field.
	if _, _, err := Complete(context.Background(), p, Request{Model: "m", Messages: []Message{{Role: "user", Content: "hi"}}, ReasoningEffort: "auto"}); err != nil {
		t.Fatal(err)
	}
	if _, ok := body["reasoning_effort"]; ok {
		t.Fatalf("auto must omit reasoning_effort: %v", body)
	}
	// XHIGH normalizes to xhigh.
	if _, _, err := Complete(context.Background(), p, Request{Model: "m", Messages: []Message{{Role: "user", Content: "hi"}}, ReasoningEffort: "XHIGH"}); err != nil {
		t.Fatal(err)
	}
	if body["reasoning_effort"] != "xhigh" {
		t.Fatalf("want xhigh, got %v", body["reasoning_effort"])
	}
}

func TestAnthropicThinkingBudget(t *testing.T) {
	var body map[string]interface{}
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		_ = json.NewDecoder(r.Body).Decode(&body)
		w.Header().Set("Content-Type", "text/event-stream")
		fmt.Fprint(w, "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n")
	}))
	defer srv.Close()
	p := &Anthropic{BaseURL: srv.URL, APIKey: "k"}
	// xhigh enables thinking with a 20000 budget and bumps max_tokens past it.
	if _, _, err := Complete(context.Background(), p, Request{Model: "m", Messages: []Message{{Role: "user", Content: "hi"}}, ReasoningEffort: "xhigh"}); err != nil {
		t.Fatal(err)
	}
	th, _ := body["thinking"].(map[string]interface{})
	if th["type"] != "enabled" {
		t.Fatalf("thinking not enabled: %v", body["thinking"])
	}
	if b, _ := th["budget_tokens"].(float64); int(b) != 20000 {
		t.Fatalf("budget=%v want 20000", th["budget_tokens"])
	}
	if mx, _ := body["max_tokens"].(float64); int(mx) < 21024 {
		t.Fatalf("max_tokens=%v must exceed budget+1024", body["max_tokens"])
	}
	// auto sends no thinking block.
	body = nil
	if _, _, err := Complete(context.Background(), p, Request{Model: "m", Messages: []Message{{Role: "user", Content: "hi"}}}); err != nil {
		t.Fatal(err)
	}
	if _, ok := body["thinking"]; ok {
		t.Fatalf("auto must omit thinking: %v", body)
	}
}

func TestOllamaThinkFlag(t *testing.T) {
	var body map[string]interface{}
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		_ = json.NewDecoder(r.Body).Decode(&body)
		fmt.Fprint(w, "{\"message\":{\"content\":\"ok\"},\"done\":true}\n")
	}))
	defer srv.Close()
	p := &Ollama{BaseURL: srv.URL}
	if _, _, err := Complete(context.Background(), p, Request{Model: "m", Messages: []Message{{Role: "user", Content: "hi"}}, ReasoningEffort: "high"}); err != nil {
		t.Fatal(err)
	}
	if body["think"] != true {
		t.Fatalf("explicit variant must set think:true: %v", body)
	}
	body = nil
	if _, _, err := Complete(context.Background(), p, Request{Model: "m", Messages: []Message{{Role: "user", Content: "hi"}}}); err != nil {
		t.Fatal(err)
	}
	if _, ok := body["think"]; ok {
		t.Fatalf("auto must omit think: %v", body)
	}
}
