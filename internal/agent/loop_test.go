package agent

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

// TestAgenticToolLoop drives RunPrompt against a scripted OpenAI-compatible
// mock: turn 1 emits write+read calls, turn 2 emits the final answer.
func TestAgenticToolLoop(t *testing.T) {
	var calls int64
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path == "/models" {
			// Runner's context-window lookup; not part of the round count.
			w.Header().Set("Content-Type", "application/json")
			fmt.Fprint(w, `{"data":[]}`)
			return
		}
		w.Header().Set("Content-Type", "text/event-stream")
		n := atomic.AddInt64(&calls, 1)
		if n == 1 {
			fmt.Fprint(w, "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"id\":\"1\",\"function\":{\"name\":\"write\",\"arguments\":\"{\\\"path\\\":\\\"loop.txt\\\",\\\"content\\\":\\\"hi\\\"}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n")
			return
		}
		if n == 2 {
			fmt.Fprint(w, "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"id\":\"2\",\"function\":{\"name\":\"read\",\"arguments\":\"{\\\"path\\\":\\\"loop.txt\\\"}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n")
			return
		}
		fmt.Fprint(w, "data: {\"choices\":[{\"delta\":{\"content\":\"all done\"},\"finish_reason\":\"stop\"}]}\n\n")
	}))
	defer srv.Close()

	ws := t.TempDir()
	os.WriteFile(filepath.Join(ws, "x.txt"), []byte("x"), 0o644)
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
	sess, err := st.CreateSession(ws, "build", "auto-edit", "", "")
	if err != nil {
		t.Fatal(err)
	}
	out, err := r.RunPrompt(context.Background(), sess.ID, TurnInput{Text: "create loop.txt then read it back"})
	if err != nil {
		t.Fatal(err)
	}
	if out != "all done" {
		t.Fatalf("final=%q", out)
	}
	turns, _ := st.ListTurns(sess.ID)
	var sawWrite, sawRead bool
	for _, tr := range turns {
		if tr.Role == "tool" {
			if len(tr.Text) >= 5 && tr.Text[:5] == "write" {
				sawWrite = true
			}
			if len(tr.Text) >= 4 && tr.Text[:4] == "read" {
				sawRead = true
			}
		}
	}
	if !sawWrite || !sawRead {
		t.Fatalf("tool turns missing write/read: %+v", turns)
	}
	if atomic.LoadInt64(&calls) != 3 {
		t.Fatalf("expected 3 model rounds, got %d", atomic.LoadInt64(&calls))
	}
}

// TestAgenticLoopPassesSoftCap keeps emitting tool calls past the old
// 8-round cliff: the turn must continue through the checkpoint instead
// of ending with a "budget reached" summary.
func TestAgenticLoopPassesSoftCap(t *testing.T) {
	var calls int64
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path == "/models" {
			w.Header().Set("Content-Type", "application/json")
			fmt.Fprint(w, `{"data":[]}`)
			return
		}
		w.Header().Set("Content-Type", "text/event-stream")
		n := atomic.AddInt64(&calls, 1)
		if n <= 10 {
			fmt.Fprint(w, "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"id\":\"1\",\"function\":{\"name\":\"read\",\"arguments\":\"{\\\"path\\\":\\\"f.txt\\\"}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n")
			return
		}
		fmt.Fprint(w, "data: {\"choices\":[{\"delta\":{\"content\":\"finished after checkpoint\"},\"finish_reason\":\"stop\"}]}\n\n")
	}))
	defer srv.Close()

	ws := t.TempDir()
	os.WriteFile(filepath.Join(ws, "f.txt"), []byte("f"), 0o644)
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
	sess, err := st.CreateSession(ws, "build", "auto-edit", "", "")
	if err != nil {
		t.Fatal(err)
	}
	out, err := r.RunPrompt(context.Background(), sess.ID, TurnInput{Text: "read f.txt repeatedly"})
	if err != nil {
		t.Fatal(err)
	}
	if out != "finished after checkpoint" {
		t.Fatalf("final=%q", out)
	}
	if atomic.LoadInt64(&calls) != 11 {
		t.Fatalf("expected 11 model rounds (10 tool + final), got %d", atomic.LoadInt64(&calls))
	}
}

// TestToolTranscriptKeepsTarget: the stored tool turn must keep the call
// target (command/path), not just the output — otherwise repeats are
// indistinguishable after compaction drops the args (the 12x identical
// `bash: $ tsc --noEmit` wall).
func TestToolTranscriptKeepsTarget(t *testing.T) {
	var calls int64
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path == "/models" {
			w.Header().Set("Content-Type", "application/json")
			fmt.Fprint(w, `{"data":[]}`)
			return
		}
		w.Header().Set("Content-Type", "text/event-stream")
		if atomic.AddInt64(&calls, 1) == 1 {
			fmt.Fprint(w, "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"id\":\"1\",\"function\":{\"name\":\"bash\",\"arguments\":\"{\\\"command\\\":\\\"echo hello\\\"}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n")
			return
		}
		fmt.Fprint(w, "data: {\"choices\":[{\"delta\":{\"content\":\"done\"},\"finish_reason\":\"stop\"}]}\n\n")
	}))
	defer srv.Close()

	ws := t.TempDir()
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
	r := NewRunner(st, router, tools.NewRegistry())
	sess, err := st.CreateSession(ws, "build", "bypass", "", "")
	if err != nil {
		t.Fatal(err)
	}
	if _, err := r.RunPrompt(context.Background(), sess.ID, TurnInput{Text: "say hi via shell"}); err != nil {
		t.Fatal(err)
	}
	turns, _ := st.ListTurns(sess.ID)
	for _, tr := range turns {
		if tr.Role == "tool" {
			if !strings.HasPrefix(tr.Text, "bash") {
				t.Fatalf("tool turn lost name prefix: %q", tr.Text)
			}
			if !strings.Contains(tr.Text, "echo hello") {
				t.Fatalf("tool turn lost target (command): %q", tr.Text)
			}
			return
		}
	}
	t.Fatal("no tool turn recorded")
}

// TestRepeatGuardWarns: three identical tool rounds in a row must inject a
// loop-guard instruction into the next prompt instead of silently looping
// to the 40-round cap.
func TestRepeatGuardWarns(t *testing.T) {
	var calls int64
	var mu sync.Mutex
	var sentBodies []string
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
		var blob strings.Builder
		for _, m := range body.Messages {
			blob.WriteString(m.Content + "\n")
		}
		mu.Lock()
		sentBodies = append(sentBodies, blob.String())
		mu.Unlock()
		w.Header().Set("Content-Type", "text/event-stream")
		if atomic.AddInt64(&calls, 1) <= 3 {
			fmt.Fprint(w, "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"id\":\"1\",\"function\":{\"name\":\"read\",\"arguments\":\"{\\\"path\\\":\\\"a.txt\\\"}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n")
			return
		}
		fmt.Fprint(w, "data: {\"choices\":[{\"delta\":{\"content\":\"broke the loop\"},\"finish_reason\":\"stop\"}]}\n\n")
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
	r := NewRunner(st, router, tools.NewRegistry())
	sess, err := st.CreateSession(ws, "build", "bypass", "", "")
	if err != nil {
		t.Fatal(err)
	}
	out, err := r.RunPrompt(context.Background(), sess.ID, TurnInput{Text: "read a.txt"})
	if err != nil {
		t.Fatal(err)
	}
	if out != "broke the loop" {
		t.Fatalf("final=%q", out)
	}
	if n := atomic.LoadInt64(&calls); n != 4 {
		t.Fatalf("expected 4 model rounds (3 tool + final), got %d", n)
	}
	mu.Lock()
	defer mu.Unlock()
	if len(sentBodies) != 4 {
		t.Fatalf("expected 4 captured prompts, got %d", len(sentBodies))
	}
	if !strings.Contains(sentBodies[3], "Loop guard") {
		t.Fatal("4th prompt (after 3 identical rounds) carries no loop-guard instruction")
	}
	if strings.Contains(sentBodies[1], "Loop guard") {
		t.Fatal("guard fired too early (only 1 identical round so far)")
	}
}

// TestPerCallRepeatWarns: the round guard only fires on 3 CONSECUTIVE
// identical rounds, so alternating calls (read a, read b, read a, …) slip
// through while one invocation repeats uselessly. The per-call ledger must
// name the repeated call on its 3rd execution even across mixed rounds.
func TestPerCallRepeatWarns(t *testing.T) {
	var calls int64
	var mu sync.Mutex
	var sentBodies []string
	toolChunk := func(name, args string) string {
		return "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"id\":\"1\",\"function\":{\"name\":\"" + name + "\",\"arguments\":\"" + args + "\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n"
	}
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
		var blob strings.Builder
		for _, m := range body.Messages {
			blob.WriteString(m.Content + "\n")
		}
		mu.Lock()
		sentBodies = append(sentBodies, blob.String())
		mu.Unlock()
		w.Header().Set("Content-Type", "text/event-stream")
		switch n := atomic.AddInt64(&calls, 1); n {
		case 1, 3, 5:
			fmt.Fprint(w, toolChunk("read", "{\\\"path\\\":\\\"a.txt\\\"}"))
		case 2, 4:
			fmt.Fprint(w, toolChunk("read", "{\\\"path\\\":\\\"b.txt\\\"}"))
		default:
			fmt.Fprint(w, "data: {\"choices\":[{\"delta\":{\"content\":\"settled\"},\"finish_reason\":\"stop\"}]}\n\n")
		}
	}))
	defer srv.Close()

	ws := t.TempDir()
	for _, f := range []string{"a.txt", "b.txt"} {
		if err := os.WriteFile(filepath.Join(ws, f), []byte("data"), 0o644); err != nil {
			t.Fatal(err)
		}
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
	r := NewRunner(st, router, tools.NewRegistry())
	sess, err := st.CreateSession(ws, "build", "bypass", "", "")
	if err != nil {
		t.Fatal(err)
	}
	out, err := r.RunPrompt(context.Background(), sess.ID, TurnInput{Text: "read both"})
	if err != nil {
		t.Fatal(err)
	}
	if out != "settled" {
		t.Fatalf("final=%q", out)
	}
	if n := atomic.LoadInt64(&calls); n != 6 {
		t.Fatalf("expected 6 model rounds (5 tool + final), got %d", n)
	}
	mu.Lock()
	defer mu.Unlock()
	if len(sentBodies) != 6 {
		t.Fatalf("expected 6 captured prompts, got %d", len(sentBodies))
	}
	// 3rd read-a.txt execution happens in round 5, so request 6 carries it.
	if !strings.Contains(sentBodies[5], "already ran 3 times") {
		t.Fatal("6th prompt carries no per-call repeat warning")
	}
	if !strings.Contains(sentBodies[5], "read") {
		t.Fatal("per-call warning must name the repeated call")
	}
	for i, b := range sentBodies[:5] {
		if strings.Contains(b, "already ran") {
			t.Fatalf("per-call warning fired too early (prompt %d)", i+1)
		}
		if strings.Contains(b, "identical tool round") {
			t.Fatalf("round guard must stay silent on alternating rounds (prompt %d)", i+1)
		}
	}
}

// TestIntermediateAssistantPersisted: model text accompanying tool calls
// must be stored as its own turn (true time order), not kept only in
// memory with the final reply.
func TestIntermediateAssistantPersisted(t *testing.T) {
	var calls int64
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path == "/models" {
			w.Header().Set("Content-Type", "application/json")
			fmt.Fprint(w, `{"data":[]}`)
			return
		}
		w.Header().Set("Content-Type", "text/event-stream")
		if atomic.AddInt64(&calls, 1) == 1 {
			fmt.Fprint(w, "data: {\"choices\":[{\"delta\":{\"content\":\"working on it\",\"tool_calls\":[{\"id\":\"1\",\"function\":{\"name\":\"read\",\"arguments\":\"{\\\"path\\\":\\\"a.txt\\\"}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n")
			return
		}
		fmt.Fprint(w, "data: {\"choices\":[{\"delta\":{\"content\":\"done\"},\"finish_reason\":\"stop\"}]}\n\n")
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
	r := NewRunner(st, router, tools.NewRegistry())
	sess, err := st.CreateSession(ws, "build", "bypass", "", "")
	if err != nil {
		t.Fatal(err)
	}
	if _, err := r.RunPrompt(context.Background(), sess.ID, TurnInput{Text: "read a.txt"}); err != nil {
		t.Fatal(err)
	}
	turns, _ := st.ListTurns(sess.ID)
	var seq []string
	for _, tr := range turns {
		seq = append(seq, tr.Role+":"+strings.SplitN(tr.Text, "\n", 2)[0])
	}
	// Expect: user, assistant(working on it), tool(read…), assistant(done).
	if len(seq) != 4 {
		t.Fatalf("expected 4 turns, got %v", seq)
	}
	if !strings.HasPrefix(seq[0], "user:") || seq[1] != "assistant:working on it" ||
		!strings.HasPrefix(seq[2], "tool:read") || seq[3] != "assistant:done" {
		t.Fatalf("turns out of time order: %v", seq)
	}
}

// TestRoundCallKeySkipsTaskOutput: background-task polling must not count
// as a loop; identical args must key equal, differing args unequal.
func TestRoundCallKeySkipsTaskOutput(t *testing.T) {
	poll := []llm.ToolCallRequest{{Name: "task_output", Args: map[string]interface{}{"task_id": "task-1"}}}
	if got := roundCallKey(poll); got != "" {
		t.Fatalf("task_output poll must key empty, got %q", got)
	}
	a := []llm.ToolCallRequest{{Name: "read", Args: map[string]interface{}{"path": "a.txt"}}}
	b := []llm.ToolCallRequest{{Name: "read", Args: map[string]interface{}{"path": "a.txt"}}}
	c := []llm.ToolCallRequest{{Name: "read", Args: map[string]interface{}{"path": "b.txt"}}}
	if roundCallKey(a) != roundCallKey(b) {
		t.Fatal("identical calls must key equal")
	}
	if roundCallKey(a) == roundCallKey(c) {
		t.Fatal("different args must key different")
	}
	if roundCallKey(a) == roundCallKey([]llm.ToolCallRequest{{Name: "bash", Args: map[string]interface{}{"command": "a.txt"}}}) {
		t.Fatal("different tool names must key different")
	}
}
