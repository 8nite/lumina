package llm

import (
	"context"
	"fmt"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"sync/atomic"
	"testing"
	"time"

	"github.com/8nite/lumina/internal/config"
)

func copilotMock(t *testing.T, exchanges *int32) (tokenURL, chatURL string, close func()) {
	t.Helper()
	chat := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path != "/chat/completions" {
			t.Errorf("chat path = %s", r.URL.Path)
		}
		if r.Header.Get("Copilot-Integration-Id") == "" {
			t.Error("missing Copilot-Integration-Id")
		}
		if r.Header.Get("Authorization") != "Bearer sess-1" {
			t.Errorf("chat auth = %q", r.Header.Get("Authorization"))
		}
		w.Header().Set("Content-Type", "text/event-stream")
		fmt.Fprint(w, "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"},\"finish_reason\":\"stop\"}]}\n\n")
	}))
	tok := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		atomic.AddInt32(exchanges, 1)
		if r.Header.Get("Authorization") != "Bearer ghu-test" {
			t.Errorf("exchange auth = %q", r.Header.Get("Authorization"))
		}
		w.Header().Set("Content-Type", "application/json")
		fmt.Fprintf(w, `{"token":"sess-1","expires_at":%d,"endpoints":{"api":%q}}`,
			time.Now().Unix()+1800, chat.URL)
	}))
	return tok.URL, chat.URL, func() { tok.Close(); chat.Close() }
}

func TestCopilotExchangeAndChat(t *testing.T) {
	var exchanges int32
	tokenURL, _, close := copilotMock(t, &exchanges)
	defer close()
	p := &Copilot{GitHubToken: "ghu-test", TokenURL: tokenURL}
	text, _, err := Complete(context.Background(), p, Request{Model: "m", Messages: []Message{{Role: "user", Content: "hi"}}})
	if err != nil {
		t.Fatal(err)
	}
	if text != "hi" {
		t.Fatalf("got %q", text)
	}
	// Cached session: second chat must not re-exchange.
	text2, _, err := Complete(context.Background(), p, Request{Model: "m", Messages: []Message{{Role: "user", Content: "hi"}}})
	if err != nil || text2 != "hi" {
		t.Fatalf("second chat: %q %v", text2, err)
	}
	if atomic.LoadInt32(&exchanges) != 1 {
		t.Fatalf("exchange called %d times, want 1 (cached)", exchanges)
	}
}

func TestCopilotBaseURLOverrideWins(t *testing.T) {
	var exchanges int32
	tokenURL, chatURL, close := copilotMock(t, &exchanges)
	defer close()
	// Exchange points elsewhere, but explicit BaseURL must win.
	p := &Copilot{GitHubToken: "ghu-test", TokenURL: tokenURL, BaseURL: chatURL}
	text, _, err := Complete(context.Background(), p, Request{Model: "m", Messages: []Message{{Role: "user", Content: "hi"}}})
	if err != nil || text != "hi" {
		t.Fatalf("got %q err %v", text, err)
	}
}

func TestCopilotExchangeFailure(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(403)
		fmt.Fprint(w, `{"message":"Resource not accessible"}`)
	}))
	defer srv.Close()
	p := &Copilot{GitHubToken: "bad", TokenURL: srv.URL}
	_, _, err := Complete(context.Background(), p, Request{Model: "m"})
	if err == nil {
		t.Fatal("expected exchange failure")
	}
}

func TestDeviceFlowMock(t *testing.T) {
	var polls int32
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		switch r.URL.Path {
		case "/login/device/code":
			fmt.Fprint(w, `{"device_code":"dev-1","user_code":"ABCD-1234","verification_uri":"https://example.test/device","expires_in":900,"interval":1}`)
		case "/login/oauth/access_token":
			if atomic.AddInt32(&polls, 1) == 1 {
				fmt.Fprint(w, `{"error":"authorization_pending"}`)
				return
			}
			fmt.Fprint(w, `{"access_token":"ghu-device-ok"}`)
		default:
			w.WriteHeader(404)
		}
	}))
	defer srv.Close()
	// Point device host at the mock by using its host as token host.
	dc, err := StartDeviceFlow(context.Background(), srv.URL+"/copilot_internal/v2/token", "")
	if err != nil {
		t.Fatal(err)
	}
	if dc.UserCode != "ABCD-1234" {
		t.Fatalf("user code = %q", dc.UserCode)
	}
	if _, err := PollDeviceFlow(context.Background(), srv.URL+"/copilot_internal/v2/token", dc.DeviceCode); err == nil || err.Error() != "pending" {
		t.Fatalf("first poll should pend, got %v", err)
	}
	tok, err := PollDeviceFlow(context.Background(), srv.URL+"/copilot_internal/v2/token", dc.DeviceCode)
	if err != nil || tok != "ghu-device-ok" {
		t.Fatalf("second poll: tok=%q err=%v", tok, err)
	}
}

func TestTokenStoreRoundtrip(t *testing.T) {
	home := t.TempDir()
	t.Setenv("HOME", home)
	if err := WriteTokenStore("Copilot", "ghu-store-test"); err != nil {
		t.Fatal(err)
	}
	got, err := ReadTokenStore("copilot")
	if err != nil || got != "ghu-store-test" {
		t.Fatalf("store roundtrip: %q %v", got, err)
	}
	if !HasTokenStore("copilot") {
		t.Fatal("HasTokenStore should be true")
	}
	st, err := os.Stat(filepath.Join(home, ".config", "lumina", "tokens", "copilot.json"))
	if err != nil {
		t.Fatal(err)
	}
	if st.Mode().Perm() != 0o600 {
		t.Fatalf("token file perms = %o, want 600", st.Mode().Perm())
	}
}

// A daemon that starts with no usable key (failing api_key_cmd, e.g. `gh`
// logged out) must list models right after device auth with no restart:
// RefreshCopilotKey installs the fresh token into the live provider.
func TestRefreshCopilotKeyPopulatesModels(t *testing.T) {
	t.Setenv("HOME", t.TempDir())
	models := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path != "/models" {
			w.WriteHeader(404)
			return
		}
		if r.Header.Get("Authorization") != "Bearer sess-fresh" {
			w.WriteHeader(401)
			return
		}
		w.Header().Set("Content-Type", "application/json")
		fmt.Fprint(w, `{"data":[{"id":"copilot/gpt-4o","name":"GPT-4o"},{"id":"copilot/claude","name":"Claude"}]}`)
	}))
	defer models.Close()
	exchange := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Header.Get("Authorization") != "Bearer ghu-fresh" {
			w.WriteHeader(401)
			fmt.Fprint(w, `{"message":"bad token"}`)
			return
		}
		w.Header().Set("Content-Type", "application/json")
		fmt.Fprintf(w, `{"token":"sess-fresh","expires_at":%d,"endpoints":{"api":%q}}`,
			time.Now().Unix()+1800, models.URL)
	}))
	defer exchange.Close()

	c := config.Defaults()
	c.Providers = map[string]config.ProviderConfig{
		"copilot": {Protocol: "copilot", TokenURL: exchange.URL, APIKeyCmd: "exit 3"},
	}
	r, err := NewRouter(c)
	if err != nil {
		t.Fatalf("router build: %v", err)
	}
	p, ok := r.Provider("copilot")
	if !ok {
		t.Fatal("copilot provider missing")
	}
	lister, ok := p.(ModelLister)
	if !ok {
		t.Fatal("copilot must list models")
	}
	// Pre-auth: no key anywhere, exchange rejects.
	if _, err := lister.ListModels(context.Background()); err == nil {
		t.Fatal("expected pre-auth ListModels failure")
	}
	// Simulate completed /connect: daemon stored the token and refreshed
	// the live router (Server.OnAuthRefresh path).
	if err := WriteProviderAuth("copilot", "ghu-fresh", "", ""); err != nil {
		t.Fatal(err)
	}
	r.RefreshCopilotKey("copilot", "ghu-fresh")
	got, err := lister.ListModels(context.Background())
	if err != nil {
		t.Fatalf("post-auth ListModels: %v", err)
	}
	if len(got) != 2 || got[0].ID != "copilot/gpt-4o" {
		t.Fatalf("models=%+v", got)
	}
}

// A construction-time key that the API accepts but the Copilot exchange
// rejects (classic PATs 404) must fall back to the device-authed store
// token: the exact /connect-then-/model failure.
func TestExchangeFallsBackToStoreToken(t *testing.T) {
	t.Setenv("HOME", t.TempDir())
	var exchanges int32
	models := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Header.Get("Authorization") != "Bearer sess-store" {
			w.WriteHeader(401)
			return
		}
		w.Header().Set("Content-Type", "application/json")
		fmt.Fprint(w, `{"data":[{"id":"copilot/gpt-4o","name":"GPT-4o"}]}`)
	}))
	defer models.Close()
	exchange := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		atomic.AddInt32(&exchanges, 1)
		if r.Header.Get("Authorization") != "Bearer ghu-store" {
			w.WriteHeader(404)
			fmt.Fprint(w, `{"message":"Not Found"}`)
			return
		}
		w.Header().Set("Content-Type", "application/json")
		fmt.Fprintf(w, `{"token":"sess-store","expires_at":%d,"endpoints":{"api":%q}}`,
			time.Now().Unix()+1800, models.URL)
	}))
	defer exchange.Close()
	if err := WriteProviderAuth("copilot", "ghu-store", "", ""); err != nil {
		t.Fatal(err)
	}
	p := &Copilot{GitHubToken: "ghp-construction", TokenURL: exchange.URL, ProviderName: "copilot"}
	got, err := p.ListModels(context.Background())
	if err != nil {
		t.Fatalf("store fallback failed: %v", err)
	}
	if len(got) != 1 || got[0].ID != "copilot/gpt-4o" {
		t.Fatalf("models=%+v", got)
	}
	if atomic.LoadInt32(&exchanges) != 2 {
		t.Fatalf("expected construction attempt + store retry, got %d", exchanges)
	}
}
