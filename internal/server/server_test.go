package server

import (
	"context"
	"encoding/json"
	"fmt"
	"net/http"
	"net/http/httptest"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/8nite/lumina/internal/agent"
	"github.com/8nite/lumina/internal/llm"
)

func TestOpenAPIValid(t *testing.T) {
	s := New(Options{})
	req := httptest.NewRequest("GET", "/openapi.json", nil)
	rec := httptest.NewRecorder()
	s.Handler().ServeHTTP(rec, req)
	if rec.Code != 200 {
		t.Fatalf("openapi status=%d", rec.Code)
	}
	var doc map[string]interface{}
	if err := json.Unmarshal(rec.Body.Bytes(), &doc); err != nil {
		t.Fatal(err)
	}
	if doc["openapi"] != "3.1.0" {
		t.Fatalf("expected OpenAPI 3.1, got %v", doc["openapi"])
	}
	paths, _ := doc["paths"].(map[string]interface{})
	for _, p := range []string{"/api/v1/events", "/api/v1/sessions", "/api/v1/pty", "/api/v1/kb/query", "/api/v1/harness/evolve"} {
		if _, ok := paths[p]; !ok {
			t.Fatalf("openapi missing path %s", p)
		}
	}
}

func TestSessionLifecycle(t *testing.T) {
	s := New(Options{})
	h := s.Handler()

	// Create.
	rec := httptest.NewRecorder()
	req := httptest.NewRequest("POST", "/api/v1/sessions", strings.NewReader(`{"workspace_dir":"/tmp/ws","agent":"plan"}`))
	h.ServeHTTP(rec, req)
	if rec.Code != 201 {
		t.Fatalf("create status=%d body=%s", rec.Code, rec.Body.String())
	}
	var sess Session
	if err := json.Unmarshal(rec.Body.Bytes(), &sess); err != nil {
		t.Fatal(err)
	}
	if sess.Agent != "plan" || sess.ID == "" {
		t.Fatalf("bad session: %+v", sess)
	}

	// Fork.
	rec = httptest.NewRecorder()
	req = httptest.NewRequest("POST", "/api/v1/sessions/"+sess.ID+"/fork", nil)
	h.ServeHTTP(rec, req)
	if rec.Code != 201 {
		t.Fatalf("fork status=%d", rec.Code)
	}

	// Prompt + revert.
	rec = httptest.NewRecorder()
	req = httptest.NewRequest("POST", "/api/v1/sessions/"+sess.ID+"/prompt", strings.NewReader(`{"text":"hi"}`))
	h.ServeHTTP(rec, req)
	if rec.Code != 202 {
		t.Fatalf("prompt status=%d", rec.Code)
	}
	// Revert with no checkpoints is a real 409 (no shadow ref yet).
	rec = httptest.NewRecorder()
	req = httptest.NewRequest("POST", "/api/v1/sessions/"+sess.ID+"/revert", nil)
	h.ServeHTTP(rec, req)
	if rec.Code != 409 {
		t.Fatalf("revert without checkpoint should 409, got %d", rec.Code)
	}
}

func TestAuthEnforced(t *testing.T) {
	s := New(Options{AuthToken: "tok"})
	rec := httptest.NewRecorder()
	req := httptest.NewRequest("GET", "/api/v1/sessions", nil)
	s.Handler().ServeHTTP(rec, req)
	if rec.Code != 401 {
		t.Fatalf("expected 401, got %d", rec.Code)
	}
}

func TestShadowRevertAPI(t *testing.T) {
	// TP-08.2/08.3: revert restores the workspace via the shadow ref.
	if _, err := exec.LookPath("git"); err != nil {
		t.Skip("git missing")
	}
	ws := t.TempDir()
	run := func(args ...string) {
		t.Helper()
		cmd := exec.Command("git", args...)
		cmd.Dir = ws
		if out, err := cmd.CombinedOutput(); err != nil {
			t.Fatalf("git %v: %v %s", args, err, out)
		}
	}
	run("init")
	run("config", "user.email", "t@t")
	run("config", "user.name", "t")
	os.WriteFile(filepath.Join(ws, "f.txt"), []byte("v1"), 0o644)
	run("add", "-A")
	run("commit", "-m", "init")

	st, err := agent.Open(filepath.Join(t.TempDir(), "s.db"))
	if err != nil {
		t.Fatal(err)
	}
	defer st.Close()
	s := New(Options{})
	s.Store = st
	h := s.Handler()

	rec := httptest.NewRecorder()
	req := httptest.NewRequest("POST", "/api/v1/sessions",
		strings.NewReader(`{"workspace_dir":"`+ws+`","agent":"build"}`))
	h.ServeHTTP(rec, req)
	if rec.Code != 201 {
		t.Fatalf("create status=%d", rec.Code)
	}
	var sess Session
	if err := json.Unmarshal(rec.Body.Bytes(), &sess); err != nil {
		t.Fatal(err)
	}
	turn, err := st.AddTurn(sess.ID, "", "assistant", "did work")
	if err != nil {
		t.Fatal(err)
	}
	if _, err := agent.Snapshot(ws, sess.ID, turn.ID); err != nil {
		t.Fatal(err)
	}
	os.WriteFile(filepath.Join(ws, "f.txt"), []byte("mutated"), 0o644)

	rec = httptest.NewRecorder()
	req = httptest.NewRequest("POST", "/api/v1/sessions/"+sess.ID+"/revert", nil)
	h.ServeHTTP(rec, req)
	if rec.Code != 200 {
		t.Fatalf("revert status=%d body=%s", rec.Code, rec.Body.String())
	}
	b, _ := os.ReadFile(filepath.Join(ws, "f.txt"))
	if string(b) != "v1" {
		t.Fatalf("revert did not restore: %q", b)
	}
	// Redo restores the pre-undo state.
	os.WriteFile(filepath.Join(ws, "f.txt"), []byte("mutated"), 0o644)
	rec = httptest.NewRecorder()
	req = httptest.NewRequest("POST", "/api/v1/sessions/"+sess.ID+"/unrevert", nil)
	h.ServeHTTP(rec, req)
	if rec.Code != 200 {
		t.Fatalf("unrevert status=%d body=%s", rec.Code, rec.Body.String())
	}
}

func TestDeviceAuthFlow(t *testing.T) {
	// Device web-auth: start returns a user code (no secret), poll stores
	// the token daemon-side and never returns it.
	home := t.TempDir()
	t.Setenv("HOME", home)
	var polls int
	mock := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		switch r.URL.Path {
		case "/login/device/code":
			fmt.Fprint(w, `{"device_code":"dev-1","user_code":"ABCD-1234","verification_uri":"https://example.test/device","expires_in":900,"interval":1}`)
		case "/login/oauth/access_token":
			polls++
			if polls == 1 {
				fmt.Fprint(w, `{"error":"authorization_pending"}`)
				return
			}
			fmt.Fprint(w, `{"access_token":"ghu-web-auth-test"}`)
		default:
			w.WriteHeader(404)
		}
	}))
	defer mock.Close()
	s := New(Options{})
	s.CopilotTokenURL = func(provider string) string { return mock.URL }
	h := s.Handler()

	rec := httptest.NewRecorder()
	req := httptest.NewRequest("POST", "/api/v1/auth/device/start", strings.NewReader(`{"provider":"copilot"}`))
	h.ServeHTTP(rec, req)
	if rec.Code != 200 {
		t.Fatalf("start status=%d body=%s", rec.Code, rec.Body.String())
	}
	var started map[string]interface{}
	if err := json.Unmarshal(rec.Body.Bytes(), &started); err != nil {
		t.Fatal(err)
	}
	if started["user_code"] != "ABCD-1234" || started["device_code"] != "dev-1" {
		t.Fatalf("start payload: %v", started)
	}
	if _, ok := started["access_token"]; ok {
		t.Fatal("start must never return a token")
	}

	// First poll: still pending.
	rec = httptest.NewRecorder()
	req = httptest.NewRequest("POST", "/api/v1/auth/device/poll", strings.NewReader(`{"provider":"copilot","device_code":"dev-1"}`))
	h.ServeHTTP(rec, req)
	var p1 map[string]string
	_ = json.Unmarshal(rec.Body.Bytes(), &p1)
	if p1["status"] != "pending" {
		t.Fatalf("expected pending, got %v", p1)
	}

	// Second poll: authorized, token stored server-side, never returned.
	rec = httptest.NewRecorder()
	req = httptest.NewRequest("POST", "/api/v1/auth/device/poll", strings.NewReader(`{"provider":"copilot","device_code":"dev-1"}`))
	h.ServeHTTP(rec, req)
	var p2 map[string]string
	_ = json.Unmarshal(rec.Body.Bytes(), &p2)
	body := rec.Body.String()
	if p2["status"] != "authorized" {
		t.Fatalf("expected authorized, got %v", p2)
	}
	if strings.Contains(body, "ghu-web-auth-test") {
		t.Fatal("poll response leaked the token")
	}
	stored, err := os.ReadFile(filepath.Join(home, ".config", "lumina", "tokens", "copilot.json"))
	if err != nil {
		t.Fatalf("token not stored: %v", err)
	}
	if !strings.Contains(string(stored), "ghu-web-auth-test") {
		t.Fatalf("stored file missing token: %s", stored)
	}
}

func TestModelsEndpoint(t *testing.T) {
	s := New(Options{})
	s.ListProviderModels = func(ctx context.Context, provider string) ([]llm.ModelMeta, error) {
		if provider != "copilot" {
			return nil, fmt.Errorf("unknown provider")
		}
		return []llm.ModelMeta{{ID: "copilot/gpt-4o", Name: "GPT-4o"}}, nil
	}
	h := s.Handler()
	rec := httptest.NewRecorder()
	req := httptest.NewRequest("GET", "/api/v1/models?provider=copilot", nil)
	h.ServeHTTP(rec, req)
	if rec.Code != 200 {
		t.Fatalf("models status=%d", rec.Code)
	}
	var out struct {
		Models []llm.ModelMeta `json:"models"`
	}
	if err := json.Unmarshal(rec.Body.Bytes(), &out); err != nil {
		t.Fatal(err)
	}
	if len(out.Models) != 1 || out.Models[0].ID != "copilot/gpt-4o" {
		t.Fatalf("models=%+v", out.Models)
	}
}

func TestGitBranchEndpoint(t *testing.T) {
	if _, err := exec.LookPath("git"); err != nil {
		t.Skip("git missing")
	}
	dir := t.TempDir()
	run := func(args ...string) {
		t.Helper()
		cmd := exec.Command("git", args...)
		cmd.Dir = dir
		cmd.Env = append(os.Environ(),
			"GIT_AUTHOR_NAME=t", "GIT_AUTHOR_EMAIL=t@t",
			"GIT_COMMITTER_NAME=t", "GIT_COMMITTER_EMAIL=t@t")
		if out, err := cmd.CombinedOutput(); err != nil {
			t.Fatalf("git %v: %v %s", args, err, out)
		}
	}
	run("init", "-b", "main")
	os.WriteFile(filepath.Join(dir, "f.txt"), []byte("x"), 0o644)
	run("add", "-A")
	run("commit", "-m", "init")
	s := New(Options{})
	h := s.Handler()
	rec := httptest.NewRecorder()
	req := httptest.NewRequest("GET", "/api/v1/git/branch?directory="+dir, nil)
	h.ServeHTTP(rec, req)
	if rec.Code != 200 {
		t.Fatalf("status=%d", rec.Code)
	}
	var out struct {
		Branch   string `json:"branch"`
		Modified string `json:"modified"`
	}
	if err := json.Unmarshal(rec.Body.Bytes(), &out); err != nil {
		t.Fatal(err)
	}
	if out.Branch != "main" || out.Modified == "" {
		t.Fatalf("branch=%+v", out)
	}
}

func TestDeviceRejectsNonCopilot(t *testing.T) {
	s := New(Options{})
	s.ProviderProtocols = map[string]string{"meta": "openai-compatible", "copilot": "copilot"}
	dead := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(404)
	}))
	defer dead.Close()
	s.CopilotTokenURL = func(provider string) string { return dead.URL }
	h := s.Handler()
	for _, tc := range []struct {
		path, body string
		want       int
	}{
		{"/api/v1/auth/device/start", `{"provider":"meta"}`, 400},
		{"/api/v1/auth/device/start", `{"provider":"copilot"}`, 502}, // no mock token host; fails later, not rejected
		{"/api/v1/auth/device/poll", `{"provider":"meta","device_code":"d"}`, 400},
	} {
		rec := httptest.NewRecorder()
		req := httptest.NewRequest("POST", tc.path, strings.NewReader(tc.body))
		h.ServeHTTP(rec, req)
		if rec.Code != tc.want {
			t.Fatalf("%s: got %d want %d (%s)", tc.path, rec.Code, tc.want, rec.Body.String())
		}
	}
}

func TestPromptWithImagesAndEffort(t *testing.T) {
	var got agent.TurnInput
	s := New(Options{})
	s.Exec = func(ctx context.Context, id string, in agent.TurnInput) (string, error) {
		got = in
		return "seen", nil
	}
	h := s.Handler()
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
	rec := httptest.NewRecorder()
	body := `{"text":"what is this","effort":"high","images":[{"name":"a.png","media_type":"image/png","data":"QUJD"}]}`
	req := httptest.NewRequest("POST", "/api/v1/sessions/"+created.ID+"/prompt", strings.NewReader(body))
	h.ServeHTTP(rec, req)
	if rec.Code != 202 {
		t.Fatalf("prompt status=%d", rec.Code)
	}
	deadline := time.Now().Add(5 * time.Second)
	for got.Text == "" && time.Now().Before(deadline) {
		time.Sleep(20 * time.Millisecond)
	}
	if got.Text != "what is this" || got.Effort != "high" || len(got.Images) != 1 || got.Images[0].Name != "a.png" {
		t.Fatalf("exec got %+v", got)
	}
}

func TestTurnCompletedCarriesSummary(t *testing.T) {
	s := New(Options{})
	s.Exec = func(_ context.Context, id string, _ agent.TurnInput) (string, error) {
		s.NoteTurnEnd(id, agent.TurnStats{Model: "prov/m", DurationMs: 2500, InputTokens: 40, OutputTokens: 100})
		return "seen", nil
	}
	ch, unsub := s.Broker().Subscribe(16)
	defer unsub()
	h := s.Handler()
	cr := httptest.NewRecorder()
	creq := httptest.NewRequest("POST", "/api/v1/sessions", strings.NewReader(`{"workspace_dir":"/tmp/ws","agent":"build"}`))
	h.ServeHTTP(cr, creq)
	var created struct {
		ID string `json:"id"`
	}
	_ = json.Unmarshal(cr.Body.Bytes(), &created)
	rec := httptest.NewRecorder()
	req := httptest.NewRequest("POST", "/api/v1/sessions/"+created.ID+"/prompt", strings.NewReader(`{"text":"hi"}`))
	h.ServeHTTP(rec, req)
	if rec.Code != 202 {
		t.Fatalf("prompt status=%d", rec.Code)
	}
	deadline := time.Now().Add(5 * time.Second)
	for time.Now().Before(deadline) {
		select {
		case e := <-ch:
			if e.Type != "turn.completed" || e.SessionID != created.ID {
				continue
			}
			var d map[string]interface{}
			raw, err := json.Marshal(e.Data)
			if err != nil {
				t.Fatal(err)
			}
			if err := json.Unmarshal(raw, &d); err != nil {
				t.Fatal(err)
			}
			if d["text"] != "seen" || d["model"] != "prov/m" {
				t.Fatalf("data=%v", d)
			}
			if d["duration_ms"] != float64(2500) || d["output_tokens"] != float64(100) {
				t.Fatalf("stats missing: %v", d)
			}
			return
		case <-time.After(100 * time.Millisecond):
		}
	}
	t.Fatal("no turn.completed with summary")
}

func TestPromptQueueChainsTurns(t *testing.T) {
	st, err := agent.Open(t.TempDir() + "/s.db")
	if err != nil {
		t.Fatal(err)
	}
	defer st.Close()
	runner := agent.NewRunner(st, nil, nil)
	s := New(Options{})
	s.Store = st
	releaseFirst := make(chan struct{})
	s.Exec = func(ctx context.Context, id string, in agent.TurnInput) (string, error) {
		if in.Text == "first" {
			// Hold the first turn open until the test queues behind it.
			select {
			case <-ctx.Done():
				return "", ctx.Err()
			case <-time.After(5 * time.Second):
				return "", fmt.Errorf("first turn not released")
			case <-releaseFirst:
			}
		}
		return "done:" + in.Text, nil
	}
	s.SteerPrompt = runner.Steer
	s.QueuePrompt = runner.Queue
	s.TakeQueued = runner.TakeQueued
	ch, unsub := s.Broker().Subscribe(64)
	defer unsub()
	h := s.Handler()
	mkSession := func() string {
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
	prompt := func(id, body string) map[string]interface{} {
		rec := httptest.NewRecorder()
		req := httptest.NewRequest("POST", "/api/v1/sessions/"+id+"/prompt", strings.NewReader(body))
		h.ServeHTTP(rec, req)
		if rec.Code != 202 {
			t.Fatalf("prompt status=%d body=%s", rec.Code, rec.Body.String())
		}
		var out map[string]interface{}
		_ = json.Unmarshal(rec.Body.Bytes(), &out)
		return out
	}
	id := mkSession()
	if got := prompt(id, `{"text":"first"}`)["status"]; got != "accepted" {
		t.Fatalf("first=%v", got)
	}
	// While the first turn runs, queue one and steer another.
	if got := prompt(id, `{"text":"second","delivery":"queue"}`); got["status"] != "queued" || got["position"] != float64(1) {
		t.Fatalf("queue=%v", got)
	}
	if got := prompt(id, `{"text":"third"}`)["status"]; got != "steered" {
		t.Fatalf("steer=%v", got)
	}
	close(releaseFirst)
	// Both turns complete in order; the steered text lands in the transcript.
	var completed []string
	deadline := time.Now().Add(10 * time.Second)
	for len(completed) < 2 && time.Now().Before(deadline) {
		select {
		case e := <-ch:
			if e.Type == "turn.completed" && e.SessionID == id {
				var d struct {
					Text string `json:"text"`
				}
				raw, _ := json.Marshal(e.Data)
				_ = json.Unmarshal(raw, &d)
				completed = append(completed, d.Text)
			}
		case <-time.After(200 * time.Millisecond):
		}
	}
	if len(completed) != 2 || completed[0] != "done:first" || completed[1] != "done:second" {
		t.Fatalf("completed=%v", completed)
	}
	// Steered text stays in the runner inbox for the live loop to consume
	// (agent-level test proves consumption); the queued turn chained above.
	if _, ok := runner.TakeQueued(id); ok {
		t.Fatal("queue should be drained")
	}
}

func TestDevicePollRefreshesRouter(t *testing.T) {
	// After a successful device auth the daemon must hand the fresh token
	// to the live router (OnAuthRefresh) so /model works with no restart.
	home := t.TempDir()
	t.Setenv("HOME", home)
	mock := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		switch r.URL.Path {
		case "/login/device/code":
			fmt.Fprint(w, `{"device_code":"dev-9","user_code":"ZZZZ-9999","verification_uri":"https://example.test/device","expires_in":900,"interval":1}`)
		case "/login/oauth/access_token":
			fmt.Fprint(w, `{"access_token":"ghu-fresh-9"}`)
		default:
			w.WriteHeader(404)
		}
	}))
	defer mock.Close()
	s := New(Options{})
	s.CopilotTokenURL = func(provider string) string { return mock.URL }
	var gotProvider, gotToken string
	s.OnAuthRefresh = func(provider, token string) { gotProvider, gotToken = provider, token }
	h := s.Handler()
	rec := httptest.NewRecorder()
	req := httptest.NewRequest("POST", "/api/v1/auth/device/poll", strings.NewReader(`{"provider":"copilot","device_code":"dev-9"}`))
	h.ServeHTTP(rec, req)
	var p map[string]string
	_ = json.Unmarshal(rec.Body.Bytes(), &p)
	if p["status"] != "authorized" {
		t.Fatalf("expected authorized, got %v", p)
	}
	if gotProvider != "copilot" || gotToken != "ghu-fresh-9" {
		t.Fatalf("OnAuthRefresh = (%q, %q)", gotProvider, "<redacted-check>")
	}
}
