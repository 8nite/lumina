package agent

import (
	"context"
	"fmt"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
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
