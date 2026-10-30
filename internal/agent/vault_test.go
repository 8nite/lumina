package agent

import (
	"context"
	"fmt"
	"io"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"regexp"
	"strings"
	"sync"
	"sync/atomic"
	"testing"

	"github.com/8nite/lumina/internal/config"
	"github.com/8nite/lumina/internal/llm"
	"github.com/8nite/lumina/internal/tools"
)

const (
	testSecretValue = "sk-testfakevaultkey12345678"
	testSecretText  = "my api_key=" + testSecretValue
)

var testRefRe = regexp.MustCompile(`\{placeholder_[a-z]{3}\}`)

func TestVaultSubstituteResolve(t *testing.T) {
	st, err := Open(filepath.Join(t.TempDir(), "s.db"))
	if err != nil {
		t.Fatal(err)
	}
	defer st.Close()
	sess, _ := st.CreateSession(t.TempDir(), "build", "ask", "", "")

	sub, err := st.substituteSecrets(sess.ID, testSecretText)
	if err != nil {
		t.Fatal(err)
	}
	if strings.Contains(sub, testSecretValue) {
		t.Fatalf("value leaked into substituted text: %q", sub)
	}
	if !strings.Contains(sub, "api_key=") {
		t.Fatalf("label dropped: %q", sub)
	}
	m := testRefRe.FindString(sub)
	if m == "" {
		t.Fatalf("no placeholder minted: %q", sub)
	}
	// Stable within the session.
	sub2, _ := st.substituteSecrets(sess.ID, "again api_key="+testSecretValue+" end")
	if !strings.Contains(sub2, m) {
		t.Fatalf("placeholder not stable: %q vs %q", m, sub2)
	}
	// Isolated across sessions.
	sess2, _ := st.CreateSession(t.TempDir(), "build", "ask", "", "")
	sub3, _ := st.substituteSecrets(sess2.ID, testSecretText)
	if strings.Contains(sub3, m) {
		t.Fatalf("placeholder leaked across sessions: %q", sub3)
	}
	// Round-trip.
	plain, err := st.resolveRefs(sess.ID, sub)
	if err != nil {
		t.Fatal(err)
	}
	if plain != testSecretText {
		t.Fatalf("round-trip: %q", plain)
	}
	// Unknown placeholders pass through; non-placeholder text untouched.
	if got, _ := st.resolveRefs(sess.ID, "see {placeholder_zzz} ok"); got != "see {placeholder_zzz} ok" {
		t.Fatalf("unknown placeholder rewritten: %q", got)
	}
	if got, _ := st.resolveRefs(sess.ID, "plain"); got != "plain" {
		t.Fatalf("plain rewritten: %q", got)
	}
	// Idempotent: placeholders match no secret pattern.
	if again, _ := st.substituteSecrets(sess.ID, sub); again != sub {
		t.Fatalf("not idempotent: %q", again)
	}
	// Arg walk.
	args := map[string]interface{}{"command": "deploy " + m, "nested": []interface{}{m}}
	rv, err := st.resolveArgRefs(sess.ID, args)
	if err != nil {
		t.Fatal(err)
	}
	got := rv.(map[string]interface{})
	if got["command"] != "deploy "+testSecretValue {
		t.Fatalf("args not resolved: %v", got)
	}
}

func TestVaultTokenIdPasswordSegments(t *testing.T) {
	st, err := Open(filepath.Join(t.TempDir(), "s.db"))
	if err != nil {
		t.Fatal(err)
	}
	defer st.Close()
	sess, _ := st.CreateSession(t.TempDir(), "build", "ask", "", "")

	vaulted := []struct{ in, label string }{
		{"login password=123456", "password="},
		{"passwd: hunter2", "passwd: "},
		{"token: abcdef", "token: "},
		{"access_token=xyz789", "access_token="},
		{"user_id=42", "user_id="},
		{"call 123456 now", "call "},
		{"code 1234-12345 ok", "code "},
		{"card 1234 5678 9012", "card "},
	}
	for _, c := range vaulted {
		sub, err := st.substituteSecrets(sess.ID, c.in)
		if err != nil {
			t.Fatal(err)
		}
		if !testRefRe.MatchString(sub) {
			t.Fatalf("%q: no placeholder: %q", c.in, sub)
		}
		if !strings.Contains(sub, c.label) {
			t.Fatalf("%q: label lost: %q", c.in, sub)
		}
		plain, err := st.resolveRefs(sess.ID, sub)
		if err != nil {
			t.Fatal(err)
		}
		if plain != c.in {
			t.Fatalf("round-trip %q: got %q", c.in, plain)
		}
		if again, _ := st.substituteSecrets(sess.ID, sub); again != sub {
			t.Fatalf("not idempotent %q: %q", c.in, again)
		}
	}
	// One ref per segmented code, spacing preserved.
	sub, _ := st.substituteSecrets(sess.ID, "call 1234-12345 now")
	if n := strings.Count(sub, "{placeholder_"); n != 1 {
		t.Fatalf("segmented code split into %d refs: %q", n, sub)
	}
	if !strings.HasPrefix(sub, "call ") || !strings.HasSuffix(sub, " now") {
		t.Fatalf("spacing mangled: %q", sub)
	}
	sub, _ = st.substituteSecrets(sess.ID, "call 123456 now")
	if !strings.HasPrefix(sub, "call ") || !strings.HasSuffix(sub, " now") {
		t.Fatalf("boundary space dropped: %q", sub)
	}

	// Benign lookalikes stay byte-identical.
	plain := []string{
		"loop.txt", "valid=true", "bypass=x", "version 1.3.10",
		"the id is 7", "abc1234", "port 80", "2026-10-30",
		"12-34", "score 100-98", "1234x", "v2_1234beta",
	}
	for _, in := range plain {
		got, err := st.substituteSecrets(sess.ID, in)
		if err != nil {
			t.Fatal(err)
		}
		if got != in {
			t.Fatalf("false positive %q: %q", in, got)
		}
	}
}

// TestVaultSurvivesReopen proves mappings persist: a restart still remaps
// placeholders minted before it.
// TestVaultGuidanceInert guards the layer-0 example: {placeholder_ABC}
// must never match the ref pattern (uppercase is unmintable), so neither
// the model nor the mocks can mistake the documentation for a live token.
func TestVaultGuidanceInert(t *testing.T) {
	if vaultRefRe.MatchString("{placeholder_ABC}") {
		t.Fatal("guidance example matches the ref pattern")
	}
	st, err := Open(filepath.Join(t.TempDir(), "s.db"))
	if err != nil {
		t.Fatal(err)
	}
	defer st.Close()
	sess, _ := st.CreateSession(t.TempDir(), "build", "ask", "", "")
	sub, err := st.substituteSecrets(sess.ID, vaultGuidance)
	if err != nil {
		t.Fatal(err)
	}
	if sub != vaultGuidance {
		t.Fatalf("guidance rewritten: %q", sub)
	}
}

// TestVaultSessionScoped proves mappings live for the session only: a new
// Store (daemon restart) no longer resolves old refs — they pass through as
// inert text, while the transcript's real values simply mint fresh refs.
func TestVaultSessionScoped(t *testing.T) {
	dir := t.TempDir()
	dbPath := filepath.Join(dir, "s.db")
	st, err := Open(dbPath)
	if err != nil {
		t.Fatal(err)
	}
	sess, _ := st.CreateSession(dir, "build", "ask", "", "")
	sub, err := st.substituteSecrets(sess.ID, testSecretText)
	if err != nil {
		t.Fatal(err)
	}
	if !testRefRe.MatchString(sub) {
		t.Fatalf("no placeholder: %q", sub)
	}
	for _, c := range testRefRe.FindString(sub)[len("{placeholder_") : len("{placeholder_")+3] {
		if c < 'a' || c > 'z' {
			t.Fatalf("ref not letters-only: %q", sub)
		}
	}
	st.Close()

	st2, err := Open(dbPath)
	if err != nil {
		t.Fatal(err)
	}
	defer st2.Close()
	got, err := st2.resolveRefs(sess.ID, sub)
	if err != nil {
		t.Fatal(err)
	}
	if got != sub {
		t.Fatalf("stale ref resolved after restart: %q", got)
	}
	// Same session text re-vaults cleanly with a fresh ref.
	sub2, err := st2.substituteSecrets(sess.ID, testSecretText)
	if err != nil {
		t.Fatal(err)
	}
	if !testRefRe.MatchString(sub2) {
		t.Fatalf("no fresh placeholder: %q", sub2)
	}
	plain, err := st2.resolveRefs(sess.ID, sub2)
	if err != nil {
		t.Fatal(err)
	}
	if plain != testSecretText {
		t.Fatalf("round-trip: %q", plain)
	}
}

// TestVaultBoundaryEndToEnd proves the split: every byte sent to the LLM
// carries placeholders only, while the user-facing return, transcript, and
// tool effects carry values.
func TestVaultBoundaryEndToEnd(t *testing.T) {
	var mu sync.Mutex
	var bodies []string
	var calls int64
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path == "/models" {
			w.Header().Set("Content-Type", "application/json")
			fmt.Fprint(w, `{"data":[]}`)
			return
		}
		raw, _ := io.ReadAll(r.Body)
		mu.Lock()
		bodies = append(bodies, string(raw))
		body := string(raw)
		mu.Unlock()
		nCalls := atomic.AddInt64(&calls, 1)
		w.Header().Set("Content-Type", "text/event-stream")
		if nCalls == 1 {
			// Echo the placeholder back inside a tool arg: the harness
			// must remap it to the real value before execution.
			ref := testRefRe.FindString(body)
			fmt.Fprintf(w, "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"id\":\"1\",\"function\":{\"name\":\"write\",\"arguments\":\"{\\\"path\\\":\\\"k.txt\\\",\\\"content\\\":\\\"%s\\\"}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n", ref)
			return
		}
		ref := testRefRe.FindString(body)
		fmt.Fprintf(w, "data: {\"choices\":[{\"delta\":{\"content\":\"saved %s\"},\"finish_reason\":\"stop\"}]}\n\n", ref)
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
	sess, _ := st.CreateSession(ws, "build", "auto-edit", "", "")

	out, err := r.RunPrompt(context.Background(), sess.ID, TurnInput{Text: "store this: " + testSecretText})
	if err != nil {
		t.Fatal(err)
	}
	// Provider side: placeholders only, in every request.
	mu.Lock()
	defer mu.Unlock()
	if len(bodies) == 0 {
		t.Fatal("no model requests captured")
	}
	for i, b := range bodies {
		if strings.Contains(b, testSecretValue) {
			t.Fatalf("request %d leaked the value", i)
		}
		if !strings.Contains(b, "{placeholder_") {
			t.Fatalf("request %d has no placeholder", i)
		}
	}
	// User side: values everywhere.
	if !strings.Contains(out, testSecretValue) {
		t.Fatalf("return kept the placeholder: %q", out)
	}
	data, err := os.ReadFile(filepath.Join(ws, "k.txt"))
	if err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(string(data), testSecretValue) {
		t.Fatalf("tool got the placeholder, not the value: %q", data)
	}
	turns, _ := st.ListTurns(sess.ID)
	var sawAssistant bool
	for _, tr := range turns {
		if tr.Role == "assistant" {
			sawAssistant = true
			if !strings.Contains(tr.Text, testSecretValue) {
				t.Fatalf("transcript kept the placeholder: %q", tr.Text)
			}
		}
	}
	if !sawAssistant {
		t.Fatal("no assistant turn stored")
	}
}
