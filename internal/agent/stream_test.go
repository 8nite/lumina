package agent

import (
	"context"
	"fmt"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"sync/atomic"
	"testing"

	"github.com/8nite/lumina/internal/config"
	"github.com/8nite/lumina/internal/llm"
	"github.com/8nite/lumina/internal/tools"
)

// stubProvider replays canned chunks, proving the runner forwards every
// delta/thinking slice while still collecting the full text + tool calls.
type stubProvider struct{ chunks []llm.Chunk }

func (s stubProvider) Name() string { return "stub" }
func (s stubProvider) Chat(context.Context, llm.Request) (<-chan llm.Chunk, error) {
	out := make(chan llm.Chunk, len(s.chunks))
	for _, c := range s.chunks {
		out <- c
	}
	close(out)
	return out, nil
}

func TestCompleteStreamForwardsChunks(t *testing.T) {
	p := stubProvider{chunks: []llm.Chunk{
		{Delta: "hel"},
		{Delta: "lo", Thinking: "plan"},
		{Delta: "", Thinking: "more"},
		{Delta: "!", Done: true},
	}}
	var deltas, thoughts []string
	text, calls, _, err := completeStream(context.Background(), p, llm.Request{}, func(d, th string) {
		deltas = append(deltas, d)
		thoughts = append(thoughts, th)
	})
	if err != nil {
		t.Fatal(err)
	}
	if text != "hello!" {
		t.Fatalf("text=%q", text)
	}
	if len(calls) != 0 {
		t.Fatalf("calls=%v", calls)
	}
	if strings.Join(deltas, "") != "hello!" {
		t.Fatalf("deltas=%q", deltas)
	}
	if strings.Join(thoughts, "") != "planmore" {
		t.Fatalf("thoughts=%q", thoughts)
	}
}

func TestCompleteStreamReturnsUsage(t *testing.T) {
	p := stubProvider{chunks: []llm.Chunk{
		{Delta: "hi"},
		{Delta: "", Usage: &llm.Usage{Input: 10, Output: 3}, Done: true},
	}}
	_, _, usage, err := completeStream(context.Background(), p, llm.Request{}, nil)
	if err != nil {
		t.Fatal(err)
	}
	if usage == nil || usage.Input != 10 || usage.Output != 3 {
		t.Fatalf("usage=%+v", usage)
	}
}

func TestTokPerSec(t *testing.T) {
	if got := (TurnStats{OutputTokens: 100, DurationMs: 2000}).TokPerSec(); got != 50 {
		t.Fatalf("tps=%v", got)
	}
	if got := (TurnStats{}).TokPerSec(); got != 0 {
		t.Fatalf("zero stats must give 0, got %v", got)
	}
}

func TestSteerQueueInbox(t *testing.T) {
	st, err := Open(t.TempDir() + "/s.db")
	if err != nil {
		t.Fatal(err)
	}
	defer st.Close()
	r := NewRunner(st, nil, nil)
	if got := r.takeSteered("s1"); got != "" {
		t.Fatalf("empty inbox must drain empty, got %q", got)
	}
	r.Steer("s1", "  ")
	r.Steer("s1", "first")
	r.Steer("s1", "second")
	if got := r.takeSteered("s1"); got != "first\n\nsecond" {
		t.Fatalf("drain=%q", got)
	}
	if got := r.takeSteered("s1"); got != "" {
		t.Fatalf("second drain must be empty, got %q", got)
	}
	if _, ok := r.TakeQueued("s1"); ok {
		t.Fatal("empty queue must miss")
	}
	if pos := r.Queue("s1", TurnInput{Text: "a"}); pos != 1 {
		t.Fatalf("pos=%d", pos)
	}
	if pos := r.Queue("s1", TurnInput{Text: "b"}); pos != 2 {
		t.Fatalf("pos=%d", pos)
	}
	next, ok := r.TakeQueued("s1")
	if !ok || next.Text != "a" {
		t.Fatalf("fifo violated: %+v %v", next, ok)
	}
	next, ok = r.TakeQueued("s1")
	if !ok || next.Text != "b" {
		t.Fatalf("fifo violated: %+v %v", next, ok)
	}
	if _, ok := r.TakeQueued("s1"); ok {
		t.Fatal("drained queue must miss")
	}
}

func TestSteeredInputJoinsLiveTurn(t *testing.T) {
	var calls int64
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "text/event-stream")
		n := atomic.AddInt64(&calls, 1)
		if n == 1 {
			fmt.Fprint(w, "data: "+`{"choices":[{"delta":{"tool_calls":[{"id":"1","function":{"name":"read","arguments":"{\"path\":\"a.txt\"}"}}]},"finish_reason":"tool_calls"}]}`+"\n\n")
			return
		}
		fmt.Fprint(w, "data: {\"choices\":[{\"delta\":{\"content\":\"steered-aware\"},\"finish_reason\":\"stop\"}]}\n\n")
	}))
	defer srv.Close()

	ws := t.TempDir()
	if err := os.WriteFile(filepath.Join(ws, "a.txt"), []byte("alpha"), 0o644); err != nil {
		t.Fatal(err)
	}
	cfg := config.Defaults()
	cfg.Providers = map[string]config.ProviderConfig{
		"testprov": {Protocol: "openai-compatible", BaseURL: srv.URL},
	}
	cfg.Router.Executor = "testprov/echo"
	router, err := llm.NewRouter(cfg)
	if err != nil {
		t.Fatal(err)
	}
	st, err := Open(filepath.Join(t.TempDir(), "s.db"))
	if err != nil {
		t.Fatal(err)
	}
	defer st.Close()
	reg := tools.NewRegistry()
	r := NewRunner(st, router, reg)
	sess, err := st.CreateSession(ws, "build", "bypass", "", "")
	if err != nil {
		t.Fatal(err)
	}
	// Steer lands while round one is in flight (buffered before the loop).
	r.Steer(sess.ID, "also tell me the file size")
	out, err := r.RunPrompt(context.Background(), sess.ID, TurnInput{Text: "read a.txt"})
	if err != nil {
		t.Fatal(err)
	}
	if out == "" {
		t.Fatal("empty reply")
	}
	turns, _ := st.ListTurns(sess.ID)
	var sawSteer, sawTool bool
	for _, tr := range turns {
		if tr.Role == "user" && strings.Contains(tr.Text, "also tell me the file size") {
			sawSteer = true
		}
		if tr.Role == "tool" {
			sawTool = true
		}
	}
	if !sawSteer {
		t.Fatalf("steered text missing from transcript: %+v", turns)
	}
	if !sawTool {
		t.Fatalf("tool turn missing: %+v", turns)
	}
}

func TestTurnStatsSummary(t *testing.T) {
	if got := (TurnStats{}).Summary(); got != "" {
		t.Fatalf("empty stats must summarize empty, got %q", got)
	}
	got := (TurnStats{Model: "prov/m", DurationMs: 2500, OutputTokens: 100}).Summary()
	if got != "prov/m · 2.5s · 40 tok/s" {
		t.Fatalf("summary=%q", got)
	}
	// No usage reported: duration only, never fabricated tok/s.
	got = (TurnStats{Model: "prov/m", DurationMs: 3200}).Summary()
	if got != "prov/m · 3.2s" {
		t.Fatalf("summary=%q", got)
	}
}

func TestExecToolBracketsExecution(t *testing.T) {
	ws := t.TempDir()
	st, err := Open(ws + "/s.db")
	if err != nil {
		t.Fatal(err)
	}
	defer st.Close()
	reg := tools.NewRegistry()
	r := NewRunner(st, nil, reg)
	var starts, dones []string
	var previews []string
	var errs []bool
	r.OnToolStart = func(_, tool, target string) { starts = append(starts, tool+" "+target) }
	r.OnToolDone = func(_, tool, target, preview string, isErr bool, _ int64) {
		dones = append(dones, tool+" "+target)
		previews = append(previews, preview)
		errs = append(errs, isErr)
	}
	sc := tools.SecCtx{WorkspaceDir: ws, Permission: "bypass"}
	if _, err := r.execTool(context.Background(), "s1", sc, "write",
		map[string]interface{}{"path": "a.txt", "content": "hello"}, "a.txt"); err != nil {
		t.Fatal(err)
	}
	if len(starts) != 1 || starts[0] != "write a.txt" {
		t.Fatalf("starts=%v", starts)
	}
	if len(dones) != 1 || len(previews) != 1 || previews[0] == "" || errs[0] {
		t.Fatalf("dones=%v previews=%v errs=%v", dones, previews, errs)
	}
	if _, err := r.execTool(context.Background(), "s1", sc, "read",
		map[string]interface{}{"path": "missing.txt"}, "missing.txt"); err == nil {
		t.Fatal("expected read error")
	}
	if len(errs) != 2 || !errs[1] {
		t.Fatalf("error event missing: %v", errs)
	}
}

func TestToolPreviewTruncates(t *testing.T) {
	if got := toolPreview("a\nb"); got != "a b" {
		t.Fatalf("newlines must fold: %q", got)
	}
	long := strings.Repeat("x", 400)
	if got := toolPreview(long); len([]rune(got)) != 303 {
		t.Fatalf("len=%d", len([]rune(got)))
	}
}
