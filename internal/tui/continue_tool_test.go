package tui

import (
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"net/url"
	"strings"
	"testing"

	"github.com/8nite/lumina/pkg/sdk"
	tea "github.com/charmbracelet/bubbletea"
)

// sessionsServer records the ?directory= query and serves a fixed list.
func sessionsServer(t *testing.T, list []*sdk.Session, seen *string) *httptest.Server {
	t.Helper()
	return httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if strings.HasPrefix(r.URL.Path, "/api/v1/sessions") && r.Method == http.MethodGet {
			*seen = r.URL.Query().Get("directory")
			_ = json.NewEncoder(w).Encode(list)
			return
		}
		w.WriteHeader(404)
	}))
}

func mkSession(id, ws, agent, perm, title string) *sdk.Session {
	return &sdk.Session{ID: id, WorkspaceDir: ws, Agent: agent, PermissionMode: perm, Title: title}
}

func TestResumePassesWorkspaceDir(t *testing.T) {
	var seen string
	srv := sessionsServer(t, []*sdk.Session{
		mkSession("prev1", "/ws", "build", "ask", "old work"),
	}, &seen)
	defer srv.Close()
	m := New(sdk.New(srv.URL, ""), mkSession("cur", "/ws", "build", "ask", ""))
	cmd := m.ApplyCommand("/resume")
	if cmd == nil {
		t.Fatal("expected fetch cmd")
	}
	msg := cmd()
	sm, ok := msg.(SessionsMsg)
	if !ok || len(sm.Sessions) != 1 {
		t.Fatalf("sessions msg: %#v", msg)
	}
	if seen != "/ws" {
		t.Fatalf("directory query not scoped to workspace, got %q", seen)
	}
	// Empty workspace (legacy headless model) lists all.
	var seenAll string
	srv2 := sessionsServer(t, []*sdk.Session{}, &seenAll)
	defer srv2.Close()
	m2 := New(sdk.New(srv2.URL, ""), &sdk.Session{ID: "s1", Agent: "build", PermissionMode: "ask"})
	_ = m2.ApplyCommand("/resume")()
	if seenAll != "" {
		t.Fatalf("empty workspace must list all, got %q", seenAll)
	}
}

func TestSessionsMsgExcludesCurrent(t *testing.T) {
	m := New(nil, mkSession("cur", "/ws", "build", "ask", ""))
	next, _ := m.Update(SessionsMsg{Sessions: []*sdk.Session{
		mkSession("cur", "/ws", "build", "ask", ""),
		mkSession("prev1", "/ws", "plan", "ask", "fix bug"),
		mkSession("prev2", "/ws", "build", "ask", ""),
	}})
	m = next.(Model)
	if !m.Picking || len(m.Sessions) != 2 {
		t.Fatalf("current must be excluded: picking=%v sessions=%v", m.Picking, m.Sessions)
	}
	if m.Sessions[0].ID != "prev1" {
		t.Fatalf("order must be preserved, got %v", m.Sessions[0].ID)
	}
	// Only current -> picker closes with guidance.
	next, _ = m.Update(SessionsMsg{Sessions: []*sdk.Session{
		mkSession("cur", "/ws", "build", "ask", ""),
	}})
	m = next.(Model)
	if m.Picking {
		t.Fatal("picker must close when nothing previous remains")
	}
	found := false
	for _, ln := range m.Lines {
		if strings.Contains(ln, "no previous sessions") {
			found = true
		}
	}
	if !found {
		t.Fatalf("missing empty guidance: %v", m.Lines)
	}
}

func TestContinueAttachesLatestPrevious(t *testing.T) {
	var seen string
	list := []*sdk.Session{
		mkSession("prev-new", "/ws", "plan", "auto-edit", "latest fix"),
		mkSession("cur", "/ws", "build", "ask", ""),
		mkSession("prev-old", "/ws", "build", "ask", ""),
	}
	srv := sessionsServer(t, list, &seen)
	defer srv.Close()
	m := New(sdk.New(srv.URL, ""), mkSession("cur", "/ws", "build", "ask", ""))
	cmd := m.ApplyCommand("/continue")
	if cmd == nil {
		t.Fatal("expected continue fetch cmd")
	}
	msg := cmd()
	am, ok := msg.(AttachedMsg)
	if !ok || am.Session.ID != "prev-new" {
		t.Fatalf("continue must attach latest non-current, got %#v", msg)
	}
	if seen != "/ws" {
		t.Fatalf("continue not workspace-scoped, got %q", seen)
	}
	next, _ := m.Update(am)
	m = next.(Model)
	if m.Session.ID != "prev-new" || m.Agent != "plan" || m.Perm != "auto-edit" {
		t.Fatalf("attach state wrong: %+v", m.Session)
	}
	found := false
	for _, ln := range m.Lines {
		if strings.Contains(ln, "attached → prev-new") {
			found = true
		}
	}
	if !found {
		t.Fatalf("missing attach line: %v", m.Lines)
	}
}

func TestContinueNoPrevious(t *testing.T) {
	var seen string
	srv := sessionsServer(t, []*sdk.Session{
		mkSession("cur", "/ws", "build", "ask", ""),
	}, &seen)
	defer srv.Close()
	m := New(sdk.New(srv.URL, ""), mkSession("cur", "/ws", "build", "ask", ""))
	msg := m.ApplyCommand("/continue")()
	td, ok := msg.(TurnDoneMsg)
	if !ok || !strings.Contains(td.Text, "no previous sessions") {
		t.Fatalf("empty continue: %#v", msg)
	}
}

func TestContinueNoDaemon(t *testing.T) {
	m := New(nil, mkSession("cur", "/ws", "build", "ask", ""))
	if cmd := m.ApplyCommand("/continue"); cmd != nil {
		t.Fatal("offline continue must not fetch")
	}
	found := false
	for _, ln := range m.Lines {
		if strings.Contains(ln, "no daemon attached") {
			found = true
		}
	}
	if !found {
		t.Fatalf("missing offline guidance: %v", m.Lines)
	}
}

func TestResumePickerNavigateAndAttach(t *testing.T) {
	m := New(nil, mkSession("cur", "/ws", "build", "ask", ""))
	next, _ := m.Update(SessionsMsg{Sessions: []*sdk.Session{
		mkSession("a", "/ws", "build", "ask", "first"),
		mkSession("b", "/ws", "plan", "ask", "second"),
	}})
	m = next.(Model)
	if !m.Picking {
		t.Fatal("picker must open")
	}
	if v := m.View(); !strings.Contains(v, "previous sessions in this workspace") {
		t.Fatalf("picker header missing workspace scope:\n%s", v)
	}
	// Down selects b, enter attaches.
	next, _ = m.Update(tea.KeyMsg{Type: tea.KeyDown})
	m = next.(Model)
	if m.PickIdx != 1 {
		t.Fatalf("pick idx=%d", m.PickIdx)
	}
	next, _ = m.Update(key("", tea.KeyEnter))
	m = next.(Model)
	if m.Picking || m.Session.ID != "b" || m.Agent != "plan" {
		t.Fatalf("attach failed: picking=%v sess=%+v", m.Picking, m.Session)
	}
}

func TestSlashPopupIncludesContinue(t *testing.T) {
	found := false
	for _, c := range builtinSlashCommands() {
		if c.Text == "/continue" {
			found = true
		}
	}
	if !found {
		t.Fatal("builtin menu must list /continue")
	}
	m := testModelWS(t.TempDir())
	m, _ = typeText(m, "/cont")
	if len(m.CompleteOptions) != 1 || m.CompleteOptions[0].Text != "/continue" {
		t.Fatalf("popup must suggest /continue, got %+v", m.CompleteOptions)
	}
	next, _ := m.Update(tea.KeyMsg{Type: tea.KeyTab})
	if next.(Model).Input != "/continue " {
		t.Fatalf("tab must accept /continue, got %q", next.(Model).Input)
	}
}

func TestResumeQueryEscapesWorkspace(t *testing.T) {
	ws := "/tmp/my ws"
	var raw string
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		raw = r.URL.RawQuery
		_ = json.NewEncoder(w).Encode([]*sdk.Session{})
	}))
	defer srv.Close()
	m := New(sdk.New(srv.URL, ""), mkSession("cur", ws, "build", "ask", ""))
	_ = m.ApplyCommand("/resume")()
	q, _ := url.ParseQuery(raw)
	if q.Get("directory") != ws {
		t.Fatalf("directory not round-tripped, raw=%q", raw)
	}
}

// --- expanded tool-call rendering ---

func toolEvent(sid, tool, target, phase, preview string, isErr bool) EventMsg {
	return eventMsg("turn.tool", sid, map[string]interface{}{
		"tool": tool, "target": target, "phase": phase, "preview": preview, "error": isErr,
	})
}

func TestToolStartSetsStatusAndLine(t *testing.T) {
	m := testModel()
	m = applyEvent(m, toolEvent("s1", "read", "a.go", "start", "", false))
	if m.Status != "tool: read a.go" {
		t.Fatalf("status=%q", m.Status)
	}
	last := m.Lines[len(m.Lines)-1]
	if last != "· `read` a.go" {
		t.Fatalf("start line=%q", last)
	}
}

func TestToolDoneRendersPreview(t *testing.T) {
	m := testModel()
	n := len(m.Lines)
	m = applyEvent(m, toolEvent("s1", "read", "a.go", "done", "package main", false))
	if len(m.Lines) != n+1 {
		t.Fatalf("expected one done line, got %d", len(m.Lines)-n)
	}
	if !strings.Contains(m.Lines[n], "package main") {
		t.Fatalf("preview missing: %q", m.Lines[n])
	}
	// Empty preview degrades to done.
	m = applyEvent(m, toolEvent("s1", "bash", "ls", "done", "", false))
	if !strings.HasSuffix(m.Lines[len(m.Lines)-1], "→ done") {
		t.Fatalf("empty preview line: %q", m.Lines[len(m.Lines)-1])
	}
}

func TestToolDoneErrorInline(t *testing.T) {
	m := testModel()
	m = applyEvent(m, toolEvent("s1", "edit", "x.go", "done", "permission denied", true))
	last := m.Lines[len(m.Lines)-1]
	if !strings.Contains(last, "error:") || !strings.Contains(last, "permission denied") {
		t.Fatalf("error line=%q", last)
	}
}

func TestToolIgnoresForeignSession(t *testing.T) {
	m := testModel()
	n := len(m.Lines)
	st := m.Status
	m = applyEvent(m, toolEvent("other", "read", "a.go", "start", "", false))
	if len(m.Lines) != n || m.Status != st {
		t.Fatalf("foreign tool leaked: lines=%d status=%q", len(m.Lines)-n, m.Status)
	}
	m = applyEvent(m, toolEvent("other", "read", "a.go", "done", "x", false))
	if len(m.Lines) != n {
		t.Fatal("foreign done leaked")
	}
}

func TestToolMissingToolFieldNoOp(t *testing.T) {
	m := testModel()
	n := len(m.Lines)
	st := m.Status
	m = applyEvent(m, eventMsg("turn.tool", "s1", map[string]string{"target": "a.go"}))
	if len(m.Lines) != n || m.Status != st {
		t.Fatalf("empty tool must be no-op: lines=%d status %q→%q", len(m.Lines)-n, st, m.Status)
	}
}

func TestToolTargetTruncatedInStatus(t *testing.T) {
	long := strings.Repeat("a", 100) + ".go"
	m := testModel()
	m = applyEvent(m, toolEvent("s1", "read", long, "start", "", false))
	if !strings.HasSuffix(m.Status, "...") {
		t.Fatalf("status must truncate long target, got %q", m.Status)
	}
	if !strings.Contains(m.Status, "tool: read ") {
		t.Fatalf("status prefix wrong: %q", m.Status)
	}
	// Timeline keeps the full target for copy-paste.
	if !strings.Contains(m.Lines[len(m.Lines)-1], long) {
		t.Fatalf("timeline must keep full target: %q", m.Lines[len(m.Lines)-1])
	}
}

func TestToolPreviewTruncated(t *testing.T) {
	long := strings.Repeat("x", 500)
	m := testModel()
	m = applyEvent(m, toolEvent("s1", "bash", "ls", "done", long, false))
	last := m.Lines[len(m.Lines)-1]
	// truncate(preview,200) → 197 chars + "...".
	if !strings.HasSuffix(last, "...") {
		t.Fatalf("preview must truncate, tail=%q", last[len(last)-10:])
	}
	if len([]rune(last)) > len("· `bash` ls → ")+203 {
		t.Fatalf("preview line too long (%d runes): %q", len([]rune(last)), last[:60])
	}
}

func TestToolWithoutTarget(t *testing.T) {
	m := testModel()
	n := len(m.Lines)
	m = applyEvent(m, toolEvent("s1", "bash", "", "start", "", false))
	if m.Status != "tool: bash" {
		t.Fatalf("status without target=%q", m.Status)
	}
	if m.Lines[len(m.Lines)-1] != "· `bash` " {
		t.Fatalf("start line without target=%q", m.Lines[len(m.Lines)-1])
	}
	if len(m.Lines) != n+1 {
		t.Fatalf("expected one line, got %d", len(m.Lines)-n)
	}
}

func TestToolUnknownPhaseRendersStartLine(t *testing.T) {
	// Any non-"done" phase (including empty/misspelled) renders the plain
	// start line so new daemon phases degrade visibly instead of silently.
	m := testModel()
	n := len(m.Lines)
	m = applyEvent(m, toolEvent("s1", "read", "a.go", "progress", "partial", false))
	if len(m.Lines) != n+1 || m.Lines[n] != "· `read` a.go" {
		t.Fatalf("unknown phase lines=%v", m.Lines[n:])
	}
	m = applyEvent(m, toolEvent("s1", "read", "a.go", "", "partial", false))
	if m.Lines[len(m.Lines)-1] != "· `read` a.go" {
		t.Fatalf("empty phase line=%q", m.Lines[len(m.Lines)-1])
	}
}

func TestToolMalformedPayloadNoOp(t *testing.T) {
	// Non-object data (raw string) must not panic or append.
	m := testModel()
	n := len(m.Lines)
	st := m.Status
	next, _ := m.updateEvent(sdk.Event{Type: "turn.tool", SessionID: "s1", Data: json.RawMessage(`"just a string"`)})
	m = next.(Model)
	if len(m.Lines) != n || m.Status != st {
		t.Fatalf("malformed payload leaked: lines=%d status %q→%q", len(m.Lines)-n, st, m.Status)
	}
	// Null data likewise.
	next, _ = m.updateEvent(sdk.Event{Type: "turn.tool", SessionID: "s1"})
	m = next.(Model)
	if len(m.Lines) != n || m.Status != st {
		t.Fatal("null payload must be no-op")
	}
}

func TestModelSingleMatchClosesPicker(t *testing.T) {
	// "/model <filter>" with one live match connects directly (TurnDoneMsg).
	// The optimistically opened picker must close — no empty picker left.
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		switch {
		case r.URL.Path == "/api/v1/providers":
			_ = json.NewEncoder(w).Encode(map[string]interface{}{"providers": []map[string]string{{"name": "copilot", "protocol": "copilot"}}})
		case r.URL.Path == "/api/v1/models":
			_ = json.NewEncoder(w).Encode(map[string]interface{}{"models": []map[string]string{{"id": "copilot/gpt-4o", "name": "GPT-4o"}}})
		case r.Method == http.MethodPatch || r.Method == http.MethodPut:
			_ = json.NewEncoder(w).Encode(map[string]string{"status": "ok"})
		default:
			w.WriteHeader(200)
		}
	}))
	defer srv.Close()
	m := New(sdk.New(srv.URL, ""), mkSession("s1", "/ws", "build", "ask", ""))
	msg := m.ApplyCommand("/model gpt")()
	if _, ok := msg.(TurnDoneMsg); !ok {
		t.Fatalf("single match must connect directly, got %#v", msg)
	}
	next, _ := m.Update(msg)
	m = next.(Model)
	if m.PickingModel {
		t.Fatal("empty model picker stuck open after direct connect")
	}
}

func TestResumeFetchErrorClosesPicker(t *testing.T) {
	// A failed session fetch (ErrMsg) must not leave an empty picker open.
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(500)
		_, _ = w.Write([]byte(`{"error":"db locked"}`))
	}))
	defer srv.Close()
	m := New(sdk.New(srv.URL, ""), mkSession("s1", "/ws", "build", "ask", ""))
	cmd := m.ApplyCommand("/resume")
	if cmd == nil {
		t.Fatal("expected fetch cmd")
	}
	msg := cmd()
	if _, ok := msg.(ErrMsg); !ok {
		t.Fatalf("expected ErrMsg, got %#v", msg)
	}
	next, _ := m.Update(msg)
	m = next.(Model)
	if m.Picking {
		t.Fatal("empty resume picker stuck open after fetch error")
	}
}
