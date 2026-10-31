package llm

// Variant matrix: every selectable effort must normalize, validate, and hit
// the wire in the right shape on all four providers.
import (
	"context"
	"encoding/json"
	"fmt"
	"net/http"
	"net/http/httptest"
	"testing"
)

func TestVariantMatrixAllProviders(t *testing.T) {
	valid := []struct {
		in, want string
		budget   int
	}{
		{"", "", 0},
		{"auto", "", 0},
		{"AUTO", "", 0},
		{"low", "low", 1024},
		{"medium", "medium", 4096},
		{"high", "high", 10000},
		{"xhigh", "xhigh", 20000},
		{"XHIGH", "xhigh", 20000},
		{" High ", "high", 10000},
		{"max", "max", 32000},
		{"Max", "max", 32000},
	}
	for _, tc := range valid {
		if got := NormalizeEffort(tc.in); got != tc.want {
			t.Fatalf("NormalizeEffort(%q)=%q want %q", tc.in, got, tc.want)
		}
		if !ValidEffort(tc.in) {
			t.Fatalf("ValidEffort(%q)=false want true", tc.in)
		}
		if got := EffortBudget(tc.in); got != tc.budget {
			t.Fatalf("EffortBudget(%q)=%d want %d", tc.in, got, tc.budget)
		}
	}
	for _, bad := range []string{"ultra", "x-high", "10", "high!"} {
		if ValidEffort(bad) {
			t.Fatalf("ValidEffort(%q)=true want false", bad)
		}
	}

	capture := func(t *testing.T, handler http.HandlerFunc) (*httptest.Server, *map[string]interface{}) {
		t.Helper()
		var body map[string]interface{}
		srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
			_ = json.NewDecoder(r.Body).Decode(&body)
			handler(w, r)
		}))
		t.Cleanup(srv.Close)
		return srv, &body
	}
	ctx := context.Background()
	msgs := []Message{{Role: "user", Content: "hi"}}

	// OpenAI-compatible: auto omits, rest send lowercase reasoning_effort.
	for _, tc := range valid {
		srv, body := capture(t, func(w http.ResponseWriter, r *http.Request) {
			w.Header().Set("Content-Type", "text/event-stream")
			fmt.Fprint(w, "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n")
		})
		if _, _, err := Complete(ctx, &OpenAICompat{BaseURL: srv.URL}, Request{Model: "m", Messages: msgs, ReasoningEffort: tc.in}); err != nil {
			t.Fatal(err)
		}
		got, ok := (*body)["reasoning_effort"]
		if tc.want == "" {
			if ok {
				t.Fatalf("openai %q: must omit reasoning_effort, got %v", tc.in, got)
			}
			continue
		}
		if got != tc.want {
			t.Fatalf("openai %q: reasoning_effort=%v want %q", tc.in, got, tc.want)
		}
	}

	// Responses: auto omits, rest send reasoning.effort.
	for _, tc := range valid {
		srv, body := capture(t, func(w http.ResponseWriter, r *http.Request) {
			w.Header().Set("Content-Type", "text/event-stream")
			fmt.Fprint(w, "event: response.completed\ndata: {\"type\":\"response.completed\"}\n\n")
		})
		if _, _, err := Complete(ctx, &Responses{BaseURL: srv.URL}, Request{Model: "m", Messages: msgs, ReasoningEffort: tc.in}); err != nil {
			t.Fatal(err)
		}
		raw, ok := (*body)["reasoning"]
		if tc.want == "" {
			if ok {
				t.Fatalf("responses %q: must omit reasoning, got %v", tc.in, raw)
			}
			continue
		}
		m, _ := raw.(map[string]interface{})
		if m["effort"] != tc.want {
			t.Fatalf("responses %q: reasoning.effort=%v want %q", tc.in, m["effort"], tc.want)
		}
	}

	// Anthropic: auto omits thinking; rest enable with budget and raised cap.
	for _, tc := range valid {
		srv, body := capture(t, func(w http.ResponseWriter, r *http.Request) {
			w.Header().Set("Content-Type", "text/event-stream")
			fmt.Fprint(w, "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n")
		})
		if _, _, err := Complete(ctx, &Anthropic{BaseURL: srv.URL, APIKey: "k"}, Request{Model: "m", Messages: msgs, ReasoningEffort: tc.in}); err != nil {
			t.Fatal(err)
		}
		raw, ok := (*body)["thinking"]
		if tc.want == "" {
			if ok {
				t.Fatalf("anthropic %q: must omit thinking, got %v", tc.in, raw)
			}
			continue
		}
		m, _ := raw.(map[string]interface{})
		if m["type"] != "enabled" {
			t.Fatalf("anthropic %q: thinking.type=%v", tc.in, m["type"])
		}
		if int(m["budget_tokens"].(float64)) != tc.budget {
			t.Fatalf("anthropic %q: budget=%v want %d", tc.in, m["budget_tokens"], tc.budget)
		}
		if max, _ := (*body)["max_tokens"].(float64); int(max) < tc.budget+1024 {
			t.Fatalf("anthropic %q: max_tokens=%v below budget+1024", tc.in, max)
		}
	}

	// Ollama: auto omits think; rest send think:true.
	for _, tc := range valid {
		srv, body := capture(t, func(w http.ResponseWriter, r *http.Request) {
			fmt.Fprint(w, "{\"message\":{\"content\":\"ok\"},\"done\":true}\n")
		})
		if _, _, err := Complete(ctx, &Ollama{BaseURL: srv.URL}, Request{Model: "m", Messages: msgs, ReasoningEffort: tc.in}); err != nil {
			t.Fatal(err)
		}
		got, ok := (*body)["think"]
		if tc.want == "" {
			if ok {
				t.Fatalf("ollama %q: must omit think, got %v", tc.in, got)
			}
			continue
		}
		if got != true {
			t.Fatalf("ollama %q: think=%v want true", tc.in, got)
		}
	}
}
