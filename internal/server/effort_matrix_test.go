package server

// Variant matrix: every selectable effort is accepted and normalized;
// anything else is rejected with 400.
import (
	"context"
	"encoding/json"
	"net/http/httptest"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/8nite/lumina/internal/agent"
)

func TestPromptEffortMatrix(t *testing.T) {
	var mu sync.Mutex
	var got agent.TurnInput
	s := New(Options{})
	s.Exec = func(_ context.Context, _ string, in agent.TurnInput) (string, error) {
		mu.Lock()
		got = in
		mu.Unlock()
		return "seen", nil
	}
	h := s.Handler()
	newSession := func(t *testing.T) string {
		t.Helper()
		cr := httptest.NewRecorder()
		creq := httptest.NewRequest("POST", "/api/v1/sessions", strings.NewReader(`{"workspace_dir":"/tmp/ws","agent":"build"}`))
		h.ServeHTTP(cr, creq)
		if cr.Code != 201 {
			t.Fatalf("create status=%d", cr.Code)
		}
		var created struct {
			ID string `json:"id"`
		}
		_ = json.Unmarshal(cr.Body.Bytes(), &created)
		return created.ID
	}
	waitGot := func() agent.TurnInput {
		t.Helper()
		deadline := time.Now().Add(5 * time.Second)
		for time.Now().Before(deadline) {
			mu.Lock()
			cur := got
			mu.Unlock()
			if cur.Text != "" {
				return cur
			}
			time.Sleep(10 * time.Millisecond)
		}
		mu.Lock()
		defer mu.Unlock()
		return got
	}

	cases := []struct{ in, want string }{
		{"", ""},
		{"auto", ""},
		{"AUTO", ""},
		{"low", "low"},
		{"medium", "medium"},
		{"high", "high"},
		{"xhigh", "xhigh"},
		{"XHIGH", "xhigh"},
		{"max", "max"},
	}
	for _, tc := range cases {
		id := newSession(t) // fresh session: idle path, no steer/queue
		mu.Lock()
		got = agent.TurnInput{}
		mu.Unlock()
		raw, _ := json.Marshal(tc.in)
		rec := httptest.NewRecorder()
		req := httptest.NewRequest("POST", "/api/v1/sessions/"+id+"/prompt",
			strings.NewReader(`{"text":"hi","effort":`+string(raw)+`}`))
		h.ServeHTTP(rec, req)
		if rec.Code != 202 {
			t.Fatalf("effort %q: status=%d want 202 (%s)", tc.in, rec.Code, rec.Body.String())
		}
		if cur := waitGot(); cur.Effort != tc.want {
			t.Fatalf("effort %q: forwarded %q want %q", tc.in, cur.Effort, tc.want)
		}
	}
	badID := newSession(t)
	for _, bad := range []string{"ultra", "x-high", "10"} {
		raw, _ := json.Marshal(bad)
		rec := httptest.NewRecorder()
		req := httptest.NewRequest("POST", "/api/v1/sessions/"+badID+"/prompt",
			strings.NewReader(`{"text":"hi","effort":`+string(raw)+`}`))
		h.ServeHTTP(rec, req)
		if rec.Code != 400 {
			t.Fatalf("effort %q: status=%d want 400 (%s)", bad, rec.Code, rec.Body.String())
		}
	}
}

func jsonStr(s string) string {
	raw, _ := json.Marshal(s)
	return string(raw)
}
