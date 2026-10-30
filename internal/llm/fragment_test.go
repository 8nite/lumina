package llm

import (
	"context"
	"fmt"
	"net/http"
	"net/http/httptest"
	"testing"
	"time"
)

// TestFragmentedToolCalls proves the OpenAI-compatible parser reassembles
// tool calls whose id/name arrive first and whose arguments stream in
// fragments (the Copilot/gpt streaming shape). Regression test: the old
// parser emitted one broken call per fragment (empty name, nil args).
func TestFragmentedToolCalls(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "text/event-stream")
		frames := []string{
			`{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"websearch","arguments":""}}]}}]}`,
			`{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"qu"}}]}}]}`,
			`{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"ery\":\"BTC\"}"}}]}}]}`,
			`{"choices":[{"delta":{"tool_calls":[{"index":1,"id":"call_2","function":{"name":"read","arguments":""}}]}}]}`,
			`{"choices":[{"delta":{"tool_calls":[{"index":1,"function":{"arguments":"{\"path\":\"a"}}]}}]}`,
			`{"choices":[{"delta":{"tool_calls":[{"index":1,"function":{"arguments":".txt\"}"}}]}}]}`,
			`{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}`,
		}
		for _, f := range frames {
			fmt.Fprintf(w, "data: %s\n\n", f)
		}
	}))
	defer srv.Close()

	p := &OpenAICompat{BaseURL: srv.URL}
	ch, err := p.Chat(context.Background(), Request{Model: "m"})
	if err != nil {
		t.Fatal(err)
	}
	var calls []ToolCallRequest
	deadline := time.After(10 * time.Second)
	for {
		select {
		case c, ok := <-ch:
			if !ok {
				goto check
			}
			calls = append(calls, c.ToolCalls...)
			if c.Done {
				goto check
			}
		case <-deadline:
			t.Fatal("stream never finished")
		}
	}
check:
	if len(calls) != 2 {
		t.Fatalf("expected 2 reassembled calls, got %+v", calls)
	}
	if calls[0].Name != "websearch" || calls[0].ID != "call_1" {
		t.Fatalf("call0=%+v", calls[0])
	}
	if q, _ := calls[0].Args["query"].(string); q != "BTC" {
		t.Fatalf("call0 args=%v", calls[0].Args)
	}
	if calls[1].Name != "read" {
		t.Fatalf("call1=%+v", calls[1])
	}
	if pth, _ := calls[1].Args["path"].(string); pth != "a.txt" {
		t.Fatalf("call1 args=%v", calls[1].Args)
	}
}
