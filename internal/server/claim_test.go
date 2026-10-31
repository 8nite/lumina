package server

import (
	"context"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"strings"
	"sync/atomic"
	"testing"
	"time"

	"github.com/8nite/lumina/internal/agent"
)

// Two daemons on one DB file must never double-drive a session: the second
// prompt fails safe (409 naming the holder) instead of interleaving two
// transcripts, and may proceed once the slot frees.
func TestCrossDaemonPromptFailsSafe(t *testing.T) {
	st, err := agent.Open(t.TempDir() + "/s.db")
	if err != nil {
		t.Fatal(err)
	}
	defer st.Close()
	srvA := New(Options{})
	srvA.Store = st
	srvB := New(Options{})
	srvB.Store = st
	if srvA.ID == srvB.ID {
		t.Fatal("daemon IDs must be distinct for claim exclusivity")
	}
	release := make(chan struct{})
	srvA.Exec = func(ctx context.Context, id string, in agent.TurnInput) (string, error) {
		if _, err := st.AddTurn(id, "", "user", in.Text); err != nil {
			return "", err
		}
		select {
		case <-ctx.Done():
			return "", ctx.Err()
		case <-release:
		}
		if _, err := st.AddTurn(id, "", "assistant", "a-done"); err != nil {
			return "", err
		}
		return "a-done", nil
	}
	var bRuns int64
	srvB.Exec = func(_ context.Context, _ string, _ agent.TurnInput) (string, error) {
		atomic.AddInt64(&bRuns, 1)
		return "b-done", nil
	}
	hA, hB := srvA.Handler(), srvB.Handler()

	cr := httptest.NewRecorder()
	hA.ServeHTTP(cr, httptest.NewRequest("POST", "/api/v1/sessions",
		strings.NewReader(`{"workspace_dir":"/tmp/ws","agent":"build"}`)))
	if cr.Code != 201 {
		t.Fatalf("create status=%d", cr.Code)
	}
	var created struct {
		ID string `json:"id"`
	}
	_ = json.Unmarshal(cr.Body.Bytes(), &created)

	if code, _ := doPrompt(hA, created.ID, `{"text":"first"}`); code != 202 {
		t.Fatalf("A prompt status=%d", code)
	}
	// Wait until A holds the shared slot.
	deadline := time.Now().Add(5 * time.Second)
	for {
		if owner, _ := st.TurnOwner(created.ID); owner == srvA.ID {
			break
		}
		if time.Now().After(deadline) {
			t.Fatal("A never claimed the slot")
		}
		time.Sleep(10 * time.Millisecond)
	}
	// B must fail safe: 409 naming the holder, no local running flag, and
	// B's runner must never start.
	code, body := doPrompt(hB, created.ID, `{"text":"second"}`)
	if code != 409 {
		t.Fatalf("B prompt while A runs must 409, got %d (%s)", code, body)
	}
	if !strings.Contains(body, "another daemon") {
		t.Fatalf("409 must name the cause: %s", body)
	}
	if srvB.isRunning(created.ID) {
		t.Fatal("B must roll back its local running flag on 409")
	}
	if atomic.LoadInt64(&bRuns) != 0 {
		t.Fatal("B's runner must never start while A holds the slot")
	}
	// Once A finishes and frees the slot, B proceeds normally.
	close(release)
	deadline = time.Now().Add(10 * time.Second)
	for {
		if owner, _ := st.TurnOwner(created.ID); owner == "" {
			break
		}
		if time.Now().After(deadline) {
			t.Fatal("slot never freed after A's turn")
		}
		time.Sleep(10 * time.Millisecond)
	}
	if code, _ := doPrompt(hB, created.ID, `{"text":"second"}`); code != 202 {
		t.Fatalf("B prompt after release status=%d", code)
	}
	deadline = time.Now().Add(5 * time.Second)
	for atomic.LoadInt64(&bRuns) == 0 && time.Now().Before(deadline) {
		time.Sleep(10 * time.Millisecond)
	}
	if atomic.LoadInt64(&bRuns) != 1 {
		t.Fatal("B's runner must run exactly once after the slot frees")
	}
}

// A claim older than the stale window is dead (crashed owner): a surviving
// daemon takes the slot over instead of 409ing forever.
func TestStaleClaimTakeover(t *testing.T) {
	old := claimStaleAfter
	claimStaleAfter = 80 * time.Millisecond
	defer func() { claimStaleAfter = old }()

	st, err := agent.Open(t.TempDir() + "/s.db")
	if err != nil {
		t.Fatal(err)
	}
	defer st.Close()
	srvA := New(Options{})
	srvA.Store = st
	srvB := New(Options{})
	srvB.Store = st
	release := make(chan struct{})
	defer close(release)
	srvA.Exec = func(ctx context.Context, _ string, _ agent.TurnInput) (string, error) {
		select {
		case <-ctx.Done():
			return "", ctx.Err()
		case <-release:
			return "a-done", nil
		}
	}
	srvB.Exec = func(_ context.Context, _ string, _ agent.TurnInput) (string, error) {
		return "b-done", nil
	}
	hA, hB := srvA.Handler(), srvB.Handler()
	cr := httptest.NewRecorder()
	hA.ServeHTTP(cr, httptest.NewRequest("POST", "/api/v1/sessions",
		strings.NewReader(`{"workspace_dir":"/tmp/ws","agent":"build"}`)))
	var created struct {
		ID string `json:"id"`
	}
	_ = json.Unmarshal(cr.Body.Bytes(), &created)

	if code, _ := doPrompt(hA, created.ID, `{"text":"first"}`); code != 202 {
		t.Fatalf("A prompt status=%d", code)
	}
	deadline := time.Now().Add(5 * time.Second)
	for {
		if owner, _ := st.TurnOwner(created.ID); owner == srvA.ID {
			break
		}
		if time.Now().After(deadline) {
			t.Fatal("A never claimed the slot")
		}
		time.Sleep(10 * time.Millisecond)
	}
	// Age the claim past the (shrunk) stale window while A's turn is still
	// blocked: B must take over instead of 409ing.
	time.Sleep(150 * time.Millisecond)
	if code, body := doPrompt(hB, created.ID, `{"text":"second"}`); code != 202 {
		t.Fatalf("stale claim must be take-over-able, got %d (%s)", code, body)
	}
	deadline = time.Now().Add(5 * time.Second)
	for {
		if owner, _ := st.TurnOwner(created.ID); owner == "" {
			break
		}
		if time.Now().After(deadline) {
			t.Fatal("slot never freed after takeover chain")
		}
		time.Sleep(10 * time.Millisecond)
	}
}

func doPrompt(h interface {
	ServeHTTP(http.ResponseWriter, *http.Request)
}, id, body string) (int, string) {
	rec := httptest.NewRecorder()
	h.ServeHTTP(rec, httptest.NewRequest("POST", "/api/v1/sessions/"+id+"/prompt", strings.NewReader(body)))
	return rec.Code, rec.Body.String()
}
