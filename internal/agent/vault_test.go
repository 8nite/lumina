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
	m := vaultRefRe.FindString(sub)
	if m == "" {
		t.Fatalf("no ref minted: %q", sub)
	}
	// Stable within the session.
	sub2, _ := st.substituteSecrets(sess.ID, "again api_key="+testSecretValue+" end")
	if !strings.Contains(sub2, m) {
		t.Fatalf("ref not stable: %q vs %q", m, sub2)
	}
	// Isolated across sessions.
	sess2, _ := st.CreateSession(t.TempDir(), "build", "ask", "", "")
	sub3, _ := st.substituteSecrets(sess2.ID, testSecretText)
	if strings.Contains(sub3, m) {
		t.Fatalf("ref leaked across sessions: %q", sub3)
	}
	// Round-trip.
	plain, err := st.resolveRefs(sess.ID, sub)
	if err != nil {
		t.Fatal(err)
	}
	if plain != testSecretText {
		t.Fatalf("round-trip: %q", plain)
	}
	// Unknown refs pass through; non-ref text untouched.
	if got, _ := st.resolveRefs(sess.ID, "see [LUMINA-SECRET-FFFFFF] ok"); got != "see [LUMINA-SECRET-FFFFFF] ok" {
		t.Fatalf("unknown ref rewritten: %q", got)
	}
	if got, _ := st.resolveRefs(sess.ID, "plain"); got != "plain" {
		t.Fatalf("plain rewritten: %q", got)
	}
	// Idempotent: refs match no secret pattern.
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

func TestTurnEncryptionAtRest(t *testing.T) {
	dir := t.TempDir()
	st, err := Open(filepath.Join(dir, "s.db"))
	if err != nil {
		t.Fatal(err)
	}
	sess, _ := st.CreateSession(dir, "build", "ask", "", "")
	if _, err := st.AddTurn(sess.ID, "", "user", testSecretText); err != nil {
		t.Fatal(err)
	}
	// Raw DB must hold ciphertext only.
	var raw string
	if err := st.db.QueryRow(`SELECT text FROM turns`).Scan(&raw); err != nil {
		t.Fatal(err)
	}
	if !strings.HasPrefix(raw, "v1:") {
		t.Fatalf("turn not sealed: %q", raw)
	}
	if strings.Contains(raw, testSecretValue) {
		t.Fatal("secret in raw DB row")
	}
	// Key file is 0600.
	fi, err := os.Stat(filepath.Join(dir, keyFileName))
	if err != nil {
		t.Fatal(err)
	}
	if fi.Mode().Perm() != 0o600 {
		t.Fatalf("key perms %o, want 600", fi.Mode().Perm())
	}
	st.Close()
	// Reopen: key persists, turns decrypt.
	st2, err := Open(filepath.Join(dir, "s.db"))
	if err != nil {
		t.Fatal(err)
	}
	defer st2.Close()
	turns, err := st2.ListTurns(sess.ID)
	if err != nil {
		t.Fatal(err)
	}
	if len(turns) != 1 || turns[0].Text != testSecretText {
		t.Fatalf("reopen decrypt: %+v", turns)
	}
	// Legacy plaintext rows still read.
	if _, err := st2.db.Exec(`INSERT INTO turns(id,session_id,parent_id,role,text,created_at) VALUES('legacy','` + sess.ID + `','','user','plain old','x')`); err != nil {
		t.Fatal(err)
	}
	turns, _ = st2.ListTurns(sess.ID)
	found := false
	for _, tr := range turns {
		if tr.Text == "plain old" {
			found = true
		}
	}
	if !found {
		t.Fatalf("legacy row unreadable: %+v", turns)
	}
}

// TestVaultBoundaryEndToEnd proves the split: every byte sent to the LLM
// carries refs only, while the user-facing return, transcript, and tool
// effects carry values.
func TestVaultBoundaryEndToEnd(t *testing.T) {
	refRe := regexp.MustCompile(`\[LUMINA-SECRET-[0-9A-F]{6}\]`)
	var mu sync.Mutex
	var bodies []string
	var calls int64
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path == "/models" {
			w.Header().Set("Content-Type", "application/json")
			fmt.Fprint(w, `{"data":[]}`)
			return
		}
		var raw []byte
		raw, _ = io.ReadAll(r.Body)
		mu.Lock()
		bodies = append(bodies, string(raw))
		body := string(raw)
		mu.Unlock()
		nCalls := atomic.AddInt64(&calls, 1)
		w.Header().Set("Content-Type", "text/event-stream")
		if nCalls == 1 {
			// Echo the ref back inside a tool arg: daemon must resolve it.
			ref := refRe.FindString(body)
			fmt.Fprintf(w, "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"id\":\"1\",\"function\":{\"name\":\"write\",\"arguments\":\"{\\\"path\\\":\\\"k.txt\\\",\\\"content\\\":\\\"%s\\\"}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n", ref)
			return
		}
		ref := refRe.FindString(body)
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
	// Provider side: refs only, in every request.
	mu.Lock()
	defer mu.Unlock()
	if len(bodies) == 0 {
		t.Fatal("no model requests captured")
	}
	for i, b := range bodies {
		if strings.Contains(b, testSecretValue) {
			t.Fatalf("request %d leaked the value", i)
		}
		if !strings.Contains(b, "[LUMINA-SECRET-") {
			t.Fatalf("request %d has no ref", i)
		}
	}
	// User side: values everywhere.
	if !strings.Contains(out, testSecretValue) {
		t.Fatalf("return kept the ref: %q", out)
	}
	data, err := os.ReadFile(filepath.Join(ws, "k.txt"))
	if err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(string(data), testSecretValue) {
		t.Fatalf("tool got the ref, not the value: %q", data)
	}
	turns, _ := st.ListTurns(sess.ID)
	var sawAssistant bool
	for _, tr := range turns {
		if tr.Role == "assistant" {
			sawAssistant = true
			if !strings.Contains(tr.Text, testSecretValue) {
				t.Fatalf("transcript kept the ref: %q", tr.Text)
			}
		}
	}
	if !sawAssistant {
		t.Fatal("no assistant turn stored")
	}
	// Disk: no plaintext anywhere in turns or vault.
	var raws []string
	rows, _ := st.db.Query(`SELECT text FROM turns`)
	for rows.Next() {
		var s string
		rows.Scan(&s)
		raws = append(raws, s)
	}
	rows.Close()
	rows2, _ := st.db.Query(`SELECT sealed FROM vault`)
	for rows2.Next() {
		var s string
		rows2.Scan(&s)
		raws = append(raws, s)
	}
	rows2.Close()
	for _, s := range raws {
		if strings.Contains(s, testSecretValue) {
			t.Fatalf("plaintext on disk: %q", s)
		}
	}
}
