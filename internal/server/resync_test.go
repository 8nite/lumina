package server

import (
	"context"
	"encoding/json"
	"net/http/httptest"
	"strings"
	"testing"
	"time"

	"github.com/8nite/lumina/internal/agent"
	"github.com/8nite/lumina/internal/tools"
)

// TestSessionRunningFlagAndStateResync simulates a long task + browser
// refresh: the SSE stream drops mid-turn, the reloaded UI resyncs via
// GET .../state + export, and history still renders after completion.
func TestSessionRunningFlagAndStateResync(t *testing.T) {
	st, err := agent.Open(t.TempDir() + "/s.db")
	if err != nil {
		t.Fatal(err)
	}
	defer st.Close()
	s := New(Options{})
	s.Store = st
	release := make(chan struct{})
	s.Exec = func(ctx context.Context, id string, in agent.TurnInput) (string, error) {
		// Mirror Runner.RunPrompt: the user turn persists immediately so a
		// reloaded UI sees it mid-turn via export (the reported bug was
		// that history stayed empty until completion).
		if _, err := st.AddTurn(id, "", "user", in.Text); err != nil {
			return "", err
		}
		select {
		case <-ctx.Done():
			return "", ctx.Err()
		case <-release:
		}
		if _, err := st.AddTurn(id, "", "assistant", "long task done"); err != nil {
			return "", err
		}
		return "long task done", nil
	}
	h := s.Handler()

	cr := httptest.NewRecorder()
	h.ServeHTTP(cr, httptest.NewRequest("POST", "/api/v1/sessions", strings.NewReader(`{"workspace_dir":"/tmp/ws","agent":"build"}`)))
	if cr.Code != 201 {
		t.Fatalf("create status=%d", cr.Code)
	}
	var created struct {
		ID string `json:"id"`
	}
	_ = json.Unmarshal(cr.Body.Bytes(), &created)

	ch, unsub := s.Broker().Subscribe(64)
	defer unsub()

	// Start the long task. Use the real Store-backed prompt path so the
	// user turn lands in the transcript immediately (like RunPrompt).
	rec := httptest.NewRecorder()
	req := httptest.NewRequest("POST", "/api/v1/sessions/"+created.ID+"/prompt", strings.NewReader(`{"text":"long task please"}`))
	h.ServeHTTP(rec, req)
	if rec.Code != 202 {
		t.Fatalf("prompt status=%d body=%s", rec.Code, rec.Body.String())
	}

	// Wait until the Exec is in flight.
	deadline := time.Now().Add(5 * time.Second)
	for !s.isRunning(created.ID) && time.Now().Before(deadline) {
		time.Sleep(10 * time.Millisecond)
	}
	if !s.isRunning(created.ID) {
		t.Fatal("session should be running mid-turn")
	}

	// Simulate what the reloaded web UI does: GET session (running flag),
	// GET state (atomic resync), GET export (history).
	get := func(path string) (int, map[string]interface{}) {
		r := httptest.NewRecorder()
		h.ServeHTTP(r, httptest.NewRequest("GET", path, nil))
		var out map[string]interface{}
		_ = json.Unmarshal(r.Body.Bytes(), &out)
		return r.Code, out
	}
	if code, out := get("/api/v1/sessions/" + created.ID); code != 200 || out["running"] != true {
		t.Fatalf("GET session should report running=true, got %d %v", code, out)
	}
	if code, out := get("/api/v1/sessions/" + created.ID + "/state"); code != 200 {
		t.Fatalf("GET state status=%d", code)
	} else {
		if out["running"] != true {
			t.Fatalf("state running should be true mid-turn: %v", out)
		}
		if sess, ok := out["session"].(map[string]interface{}); !ok || sess["id"] != created.ID {
			t.Fatalf("state session missing: %v", out)
		}
	}
	// Export mid-turn must already contain the user turn: history renders
	// even before the assistant finishes (user reported it didn't). Poll
	// briefly: POST returns 202 before the async Exec persists the turn,
	// so an immediate fetch can race ahead of AddTurn on a loaded box.
	foundUser := false
	deadline = time.Now().Add(5 * time.Second)
	for !foundUser && time.Now().Before(deadline) {
		r := httptest.NewRecorder()
		h.ServeHTTP(r, httptest.NewRequest("GET", "/api/v1/sessions/"+created.ID+"/export", nil))
		var out map[string]interface{}
		_ = json.Unmarshal(r.Body.Bytes(), &out)
		raw, _ := json.Marshal(out["transcript"])
		if strings.Contains(string(raw), "long task please") {
			foundUser = true
			break
		}
		time.Sleep(20 * time.Millisecond)
	}
	if !foundUser {
		t.Fatal("mid-turn export must contain the in-flight user text")
	}
	// List must also carry the flag so the sidebar can show a spinner.
	r := httptest.NewRecorder()
	h.ServeHTTP(r, httptest.NewRequest("GET", "/api/v1/sessions", nil))
	var list []map[string]interface{}
	_ = json.Unmarshal(r.Body.Bytes(), &list)
	found := false
	for _, x := range list {
		if x["id"] == created.ID {
			found = true
			if x["running"] != true {
				t.Fatalf("list should report running=true: %v", x)
			}
		}
	}
	if !found {
		t.Fatal("session missing from list")
	}

	// Finish the long task; the terminal event must still fire for the
	// reconnected client, and the final export must render full history.
	close(release)
	completed := false
	deadline = time.Now().Add(10 * time.Second)
	for !completed && time.Now().Before(deadline) {
		select {
		case e := <-ch:
			if e.Type == "turn.completed" && e.SessionID == created.ID {
				completed = true
			}
		case <-time.After(100 * time.Millisecond):
		}
	}
	if !completed {
		t.Fatal("no turn.completed after release")
	}
	// Drain the running-flag cleanup (runTurns defers the delete).
	deadline = time.Now().Add(5 * time.Second)
	for s.isRunning(created.ID) && time.Now().Before(deadline) {
		time.Sleep(10 * time.Millisecond)
	}
	if s.isRunning(created.ID) {
		t.Fatal("session should be idle after completion")
	}
	if code, out := get("/api/v1/sessions/" + created.ID + "/state"); code != 200 || out["running"] == true {
		t.Fatalf("state should report idle after completion: %d %v", code, out)
	}
	if code, out := get("/api/v1/sessions/" + created.ID + "/export"); code != 200 {
		t.Fatalf("final export status=%d", code)
	} else {
		raw, _ := json.Marshal(out["transcript"])
		if !strings.Contains(string(raw), "long task please") || !strings.Contains(string(raw), "long task done") {
			t.Fatalf("final export must contain user + assistant history: %s", raw)
		}
	}
}

// TestStateRecoversPendingPermission simulates a refresh while the turn
// waits on approval: the modal is gone client-side, but GET .../state must
// return it so the UI can re-render and unblock the "stuck" turn.
func TestStateRecoversPendingPermission(t *testing.T) {
	st, err := agent.Open(t.TempDir() + "/s.db")
	if err != nil {
		t.Fatal(err)
	}
	defer st.Close()
	s := New(Options{})
	s.Store = st
	s.Exec = func(ctx context.Context, id string, in agent.TurnInput) (string, error) {
		d, err := s.RequestPermission(ctx, id, "bash", "rm -rf /tmp/x")
		if err != nil {
			return "", err
		}
		return "decided:" + d, nil
	}
	h := s.Handler()
	cr := httptest.NewRecorder()
	h.ServeHTTP(cr, httptest.NewRequest("POST", "/api/v1/sessions", strings.NewReader(`{"workspace_dir":"/tmp/ws","agent":"build"}`)))
	var created struct {
		ID string `json:"id"`
	}
	_ = json.Unmarshal(cr.Body.Bytes(), &created)

	ch, unsub := s.Broker().Subscribe(64)
	defer unsub()

	rec := httptest.NewRecorder()
	h.ServeHTTP(rec, httptest.NewRequest("POST", "/api/v1/sessions/"+created.ID+"/prompt", strings.NewReader(`{"text":"do risky thing"}`)))
	if rec.Code != 202 {
		t.Fatalf("prompt status=%d", rec.Code)
	}
	// Wait for the permission block.
	deadline := time.Now().Add(5 * time.Second)
	var reqID string
	for time.Now().Before(deadline) {
		s.permMu.Lock()
		for id, meta := range s.permMeta {
			if meta["session_id"] == created.ID {
				if _, ok := s.permPending[id]; ok {
					reqID = id
				}
			}
		}
		s.permMu.Unlock()
		if reqID != "" {
			break
		}
		time.Sleep(10 * time.Millisecond)
	}
	if reqID == "" {
		t.Fatal("no pending permission")
	}
	// Reloaded UI resync: state must carry the pending prompt.
	r := httptest.NewRecorder()
	h.ServeHTTP(r, httptest.NewRequest("GET", "/api/v1/sessions/"+created.ID+"/state", nil))
	if r.Code != 200 {
		t.Fatalf("state status=%d", r.Code)
	}
	var state struct {
		Running    bool              `json:"running"`
		Permission map[string]string `json:"permission"`
	}
	_ = json.Unmarshal(r.Body.Bytes(), &state)
	if !state.Running {
		t.Fatal("state should report running while blocked on approval")
	}
	if state.Permission["request_id"] != reqID || state.Permission["tool"] != "bash" {
		t.Fatalf("state permission wrong: %+v", state.Permission)
	}
	// Answering via the API unblocks the turn and fires the terminal event.
	ar := httptest.NewRecorder()
	h.ServeHTTP(ar, httptest.NewRequest("POST", "/api/v1/permissions/"+reqID, strings.NewReader(`{"decision":"allow_once"}`)))
	if ar.Code != 200 {
		t.Fatalf("answer status=%d", ar.Code)
	}
	completed := false
	deadline = time.Now().Add(10 * time.Second)
	for !completed && time.Now().Before(deadline) {
		select {
		case e := <-ch:
			if e.Type == "turn.completed" && e.SessionID == created.ID {
				raw, _ := json.Marshal(e.Data)
				if !strings.Contains(string(raw), "allow_once") {
					t.Fatalf("completed should carry decision: %s", raw)
				}
				completed = true
			}
		case <-time.After(100 * time.Millisecond):
		}
	}
	if !completed {
		t.Fatal("no turn.completed after approval")
	}
}

// TestStateRecoversPendingQuestion covers the same reload path for
// question.asked: the payload must survive the disconnect.
func TestStateRecoversPendingQuestion(t *testing.T) {
	st, err := agent.Open(t.TempDir() + "/s.db")
	if err != nil {
		t.Fatal(err)
	}
	defer st.Close()
	s := New(Options{})
	s.Store = st
	s.Exec = func(ctx context.Context, id string, in agent.TurnInput) (string, error) {
		ans, err := s.RequestQuestion(ctx, id, []tools.Question{{Question: "pick one"}})
		if err != nil {
			return "", err
		}
		if len(ans) == 0 || len(ans[0]) == 0 {
			return "no-answer", nil
		}
		return "picked:" + ans[0][0], nil
	}
	h := s.Handler()
	cr := httptest.NewRecorder()
	h.ServeHTTP(cr, httptest.NewRequest("POST", "/api/v1/sessions", strings.NewReader(`{"workspace_dir":"/tmp/ws","agent":"build"}`)))
	var created struct {
		ID string `json:"id"`
	}
	_ = json.Unmarshal(cr.Body.Bytes(), &created)
	ch, unsub := s.Broker().Subscribe(64)
	defer unsub()

	rec := httptest.NewRecorder()
	h.ServeHTTP(rec, httptest.NewRequest("POST", "/api/v1/sessions/"+created.ID+"/prompt", strings.NewReader(`{"text":"ask me"}`)))
	if rec.Code != 202 {
		t.Fatalf("prompt status=%d", rec.Code)
	}
	deadline := time.Now().Add(5 * time.Second)
	var reqID string
	for time.Now().Before(deadline) {
		s.qMu.Lock()
		for id, meta := range s.qMeta {
			if meta["session_id"] == created.ID {
				if _, ok := s.qPending[id]; ok {
					reqID = id
				}
			}
		}
		s.qMu.Unlock()
		if reqID != "" {
			break
		}
		time.Sleep(10 * time.Millisecond)
	}
	if reqID == "" {
		t.Fatal("no pending question")
	}
	r := httptest.NewRecorder()
	h.ServeHTTP(r, httptest.NewRequest("GET", "/api/v1/sessions/"+created.ID+"/state", nil))
	if r.Code != 200 {
		t.Fatalf("state status=%d", r.Code)
	}
	var state struct {
		Running  bool `json:"running"`
		Question struct {
			RequestID string            `json:"request_id"`
			Questions []tools.Question `json:"questions"`
		} `json:"question"`
	}
	_ = json.Unmarshal(r.Body.Bytes(), &state)
	if !state.Running {
		t.Fatal("state should report running while blocked on question")
	}
	if state.Question.RequestID != reqID || len(state.Question.Questions) != 1 || state.Question.Questions[0].Question != "pick one" {
		t.Fatalf("state question wrong: %+v", state.Question)
	}
	ar := httptest.NewRecorder()
	h.ServeHTTP(ar, httptest.NewRequest("POST", "/api/v1/questions/"+reqID, strings.NewReader(`{"answers":[["opt-a"]]}`)))
	if ar.Code != 200 {
		t.Fatalf("answer status=%d", ar.Code)
	}
	completed := false
	deadline = time.Now().Add(10 * time.Second)
	for !completed && time.Now().Before(deadline) {
		select {
		case e := <-ch:
			if e.Type == "turn.completed" && e.SessionID == created.ID {
				completed = true
			}
		case <-time.After(100 * time.Millisecond):
		}
	}
	if !completed {
		t.Fatal("no turn.completed after answering")
	}
}
