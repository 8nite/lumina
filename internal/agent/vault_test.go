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

var testRefRe = regexp.MustCompile(`\{placeholder_[a-z0-9]{3}\}`)

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

// TestVaultSurvivesReopen proves mappings persist: a restart still remaps
// placeholders minted before it.
func TestVaultSurvivesReopen(t *testing.T) {
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
	st.Close()

	st2, err := Open(dbPath)
	if err != nil {
		t.Fatal(err)
	}
	defer st2.Close()
	plain, err := st2.resolveRefs(sess.ID, sub)
	if err != nil {
		t.Fatal(err)
	}
	if plain != testSecretText {
		t.Fatalf("post-reopen resolve: %q", plain)
	}
}

// TestVaultLegacyTable proves dev DBs created by the pre-release sealed-column
// layout still open and vault from scratch.
func TestVaultLegacyTable(t *testing.T) {
	dir := t.TempDir()
	dbPath := filepath.Join(dir, "s.db")
	legacy, err := Open(dbPath)
	if err != nil {
		t.Fatal(err)
	}
	// Simulate the old layout: drop the new table, recreate with `sealed`.
	if _, err := legacy.db.Exec(`DROP TABLE vault`); err != nil {
		t.Fatal(err)
	}
	if _, err := legacy.db.Exec(`CREATE TABLE vault(
 session_id TEXT NOT NULL, ref TEXT NOT NULL, sealed TEXT NOT NULL, created_at TEXT,
 UNIQUE(session_id, ref))`); err != nil {
		t.Fatal(err)
	}
	legacy.Close()

	st, err := Open(dbPath)
	if err != nil {
		t.Fatal(err)
	}
	defer st.Close()
	sess, _ := st.CreateSession(dir, "build", "ask", "", "")
	sub, err := st.substituteSecrets(sess.ID, testSecretText)
	if err != nil {
		t.Fatal(err)
	}
	if !testRefRe.MatchString(sub) {
		t.Fatalf("no placeholder on legacy table: %q", sub)
	}
	plain, err := st.resolveRefs(sess.ID, sub)
	if err != nil {
		t.Fatal(err)
	}
	if plain != testSecretText {
		t.Fatalf("round-trip on legacy table: %q", plain)
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
