package agent

// Behavioral eval suite for the harness: fast, deterministic, local-only
// checks that assert on what the harness DOES (prompts sent, calls made,
// turns kept) instead of model prose. Each test targets one observed
// failure mode; the rest of the suite acts as the regression guardrail.
// (Per the harness-engineering literature: behavioral evals are the
// iteration partner, end-to-end benchmarks only the report card.)

import (
	"context"
	"encoding/json"
	"fmt"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"sync/atomic"
	"testing"

	"github.com/8nite/lumina/internal/config"
	"github.com/8nite/lumina/internal/llm"
	"github.com/8nite/lumina/internal/tools"
)

// evalServer serves a scripted OpenAI-compatible gateway: /models advertises
// the given window, /chat/completions captures the request messages and
// replies with a final answer (no tool calls). chatHits counts prompt
// requests; sentMsgs holds the last decoded message list.
type evalServer struct {
	srv      *httptest.Server
	chatHits *int64
	sentMsgs *[]llm.Message
}

func newEvalServer(t *testing.T, window int) *evalServer {
	t.Helper()
	var hits int64
	var mu sync.Mutex
	var sent []llm.Message
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path == "/models" {
			w.Header().Set("Content-Type", "application/json")
			fmt.Fprintf(w, `{"data":[{"id":"m","name":"M","context_length":%d}]}`, window)
			return
		}
		var body struct {
			Messages []llm.Message `json:"messages"`
		}
		if err := json.NewDecoder(r.Body).Decode(&body); err != nil {
			t.Errorf("decode chat body: %v", err)
		}
		mu.Lock()
		sent = body.Messages
		mu.Unlock()
		atomic.AddInt64(&hits, 1)
		w.Header().Set("Content-Type", "text/event-stream")
		fmt.Fprint(w, "data: "+`{"choices":[{"delta":{"content":"done"},"finish_reason":"stop"}]}`+"\n\n")
	}))
	t.Cleanup(srv.Close)
	return &evalServer{srv: srv, chatHits: &hits, sentMsgs: &sent}
}

func (e *evalServer) router(t *testing.T) *llm.Router {
	t.Helper()
	cfg := config.Defaults()
	cfg.Providers = map[string]config.ProviderConfig{
		"testprov": {Protocol: "openai-compatible", BaseURL: e.srv.URL},
	}
	cfg.Router.Executor = "testprov/m"
	r, err := llm.NewRouter(cfg)
	if err != nil {
		t.Fatal(err)
	}
	return r
}

// TestOverWindowTurnFailsFast: a system prompt that cannot fit the
// advertised window (the 19k-prompt vs 12k-model incident) must fail before
// any provider request, with an error naming the model and both sizes.
func TestOverWindowTurnFailsFast(t *testing.T) {
	es := newEvalServer(t, 64) // smaller than any real system prompt
	ws := t.TempDir()
	st, err := Open(filepath.Join(t.TempDir(), "s.db"))
	if err != nil {
		t.Fatal(err)
	}
	defer st.Close()
	r := NewRunner(st, es.router(t), tools.NewRegistry())
	sess, err := st.CreateSession(ws, "build", "bypass", "", "")
	if err != nil {
		t.Fatal(err)
	}
	_, err = r.RunPrompt(context.Background(), sess.ID, TurnInput{Text: "hi"})
	if err == nil || !strings.Contains(err.Error(), "window") {
		t.Fatalf("expected window error, got %v", err)
	}
	if n := atomic.LoadInt64(es.chatHits); n != 0 {
		t.Fatalf("doomed prompt reached the provider %d times", n)
	}
}

// TestHistoryCompactsUnderWindow seeds history far beyond the mock window
// (the window is sized well above the local system prompt + specs so the
// test is independent of global-rules size) and asserts the harness sends
// newest-first history under 80% of the window — keeping the current
// prompt, evicting the oldest.
func TestHistoryCompactsUnderWindow(t *testing.T) {
	const window = 64000
	es := newEvalServer(t, window)
	ws := t.TempDir()
	st, err := Open(filepath.Join(t.TempDir(), "s.db"))
	if err != nil {
		t.Fatal(err)
	}
	defer st.Close()
	r := NewRunner(st, es.router(t), tools.NewRegistry())
	sess, err := st.CreateSession(ws, "build", "bypass", "", "")
	if err != nil {
		t.Fatal(err)
	}
	oldest := "OLDEST-MARKER-" + strings.Repeat("x", 790)
	for i := 0; i < 400; i++ {
		text := fmt.Sprintf("history-%d-", i) + strings.Repeat("y", 790)
		if i == 0 {
			text = oldest
		}
		if _, err := st.AddTurn(sess.ID, "", "user", text); err != nil {
			t.Fatal(err)
		}
		if _, err := st.AddTurn(sess.ID, "", "assistant", "ack"); err != nil {
			t.Fatal(err)
		}
	}
	newest := "NEWEST-PROMPT-" + strings.Repeat("z", 100)
	out, err := r.RunPrompt(context.Background(), sess.ID, TurnInput{Text: newest})
	if err != nil {
		t.Fatal(err)
	}
	if out != "done" {
		t.Fatalf("reply=%q", out)
	}
	if n := atomic.LoadInt64(es.chatHits); n != 1 {
		t.Fatalf("expected exactly 1 prompt request, got %d", n)
	}
	sent := *es.sentMsgs
	if got := EstimateContextTokens(sent, toolSpecs()); got > window*8/10 {
		t.Fatalf("sent ~%d tokens, over 80%% of %d", got, window)
	}
	var blob strings.Builder
	for _, m := range sent {
		blob.WriteString(m.Content)
	}
	if !strings.Contains(blob.String(), newest) {
		t.Fatal("current prompt missing from compacted request (oldest-first eviction?)")
	}
	if strings.Contains(blob.String(), "OLDEST-MARKER-") {
		t.Fatal("oldest history survived while over budget (no eviction?)")
	}
}

// TestConcurrentWritesLoseNothing: 16 writers x 25 turns against one session
// must record all 400 turns (regression test for the SQLITE_BUSY turn loss).
func TestConcurrentWritesLoseNothing(t *testing.T) {
	st, err := Open(filepath.Join(t.TempDir(), "s.db"))
	if err != nil {
		t.Fatal(err)
	}
	defer st.Close()
	sess, err := st.CreateSession(t.TempDir(), "build", "bypass", "", "")
	if err != nil {
		t.Fatal(err)
	}
	const writers, perWriter = 16, 25
	var wg sync.WaitGroup
	errCh := make(chan error, writers*perWriter)
	for w := 0; w < writers; w++ {
		wg.Add(1)
		go func(w int) {
			defer wg.Done()
			for i := 0; i < perWriter; i++ {
				if _, err := st.AddTurn(sess.ID, "", "user", fmt.Sprintf("w%d-%d %s", w, i, strings.Repeat("p", 50))); err != nil {
					errCh <- err
				}
			}
		}(w)
	}
	wg.Wait()
	close(errCh)
	for err := range errCh {
		t.Fatalf("concurrent write lost: %v", err)
	}
	turns, err := st.ListTurns(sess.ID)
	if err != nil {
		t.Fatal(err)
	}
	if len(turns) != writers*perWriter {
		t.Fatalf("recorded %d/%d turns", len(turns), writers*perWriter)
	}
}

// TestUnknownWindowKeepsLegacyCeiling: when the provider reports no window
// (/models empty), long history still sends newest-first under the 40-message
// ceiling instead of dropping the current prompt.
func TestUnknownWindowKeepsLegacyCeiling(t *testing.T) {
	var hits int64
	var mu sync.Mutex
	var sent []llm.Message
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path == "/models" {
			w.Header().Set("Content-Type", "application/json")
			fmt.Fprint(w, `{"data":[]}`)
			return
		}
		var body struct {
			Messages []llm.Message `json:"messages"`
		}
		_ = json.NewDecoder(r.Body).Decode(&body)
		mu.Lock()
		sent = body.Messages
		mu.Unlock()
		atomic.AddInt64(&hits, 1)
		w.Header().Set("Content-Type", "text/event-stream")
		fmt.Fprint(w, "data: "+`{"choices":[{"delta":{"content":"done"},"finish_reason":"stop"}]}`+"\n\n")
	}))
	t.Cleanup(srv.Close)
	cfg := config.Defaults()
	cfg.Providers = map[string]config.ProviderConfig{
		"testprov": {Protocol: "openai-compatible", BaseURL: srv.URL},
	}
	cfg.Router.Executor = "testprov/m"
	router, err := llm.NewRouter(cfg)
	if err != nil {
		t.Fatal(err)
	}
	ws := t.TempDir()
	st, err := Open(filepath.Join(t.TempDir(), "s.db"))
	if err != nil {
		t.Fatal(err)
	}
	defer st.Close()
	_ = os.MkdirAll(ws, 0o755)
	r := NewRunner(st, router, tools.NewRegistry())
	sess, err := st.CreateSession(ws, "build", "bypass", "", "")
	if err != nil {
		t.Fatal(err)
	}
	for i := 0; i < 60; i++ {
		if _, err := st.AddTurn(sess.ID, "", "user", fmt.Sprintf("old-%d", i)); err != nil {
			t.Fatal(err)
		}
	}
	newest := "NEWEST-UNKNOWN-WINDOW"
	if _, err := r.RunPrompt(context.Background(), sess.ID, TurnInput{Text: newest}); err != nil {
		t.Fatal(err)
	}
	if n := atomic.LoadInt64(&hits); n != 1 {
		t.Fatalf("expected 1 prompt request, got %d", n)
	}
	if len(sent) > 41 {
		t.Fatalf("sent %d messages, over legacy ceiling", len(sent))
	}
	var blob strings.Builder
	for _, m := range sent {
		blob.WriteString(m.Content)
	}
	if !strings.Contains(blob.String(), newest) {
		t.Fatal("current prompt missing under unknown window")
	}
	if strings.Contains(blob.String(), "old-0") {
		t.Fatal("oldest history kept while newest should win")
	}
}
