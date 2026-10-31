package tui

import (
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"github.com/8nite/lumina/pkg/sdk"
	tea "github.com/charmbracelet/bubbletea"
)

func testModel() Model {
	return New(nil, &sdk.Session{ID: "s1", Agent: "build", PermissionMode: "ask"})
}

func sendKey(m Model, k tea.KeyMsg) Model {
	next, _ := m.Update(k)
	return next.(Model)
}

func key(s string, t tea.KeyType) tea.KeyMsg {
	return tea.KeyMsg{Type: t, Runes: []rune(s)}
}

func TestTabTogglesPlanBuild(t *testing.T) {
	m := sendKey(testModel(), key("tab", tea.KeyTab))
	if m.Agent != "plan" {
		t.Fatalf("tab should switch to plan, got %s", m.Agent)
	}
	m = sendKey(m, key("tab", tea.KeyTab))
	if m.Agent != "build" {
		t.Fatalf("tab should switch back to build, got %s", m.Agent)
	}
}

func TestShiftTabCyclesApproval(t *testing.T) {
	m := testModel()
	want := []string{"auto-edit", "auto-approve", "bypass", "ask"}
	for _, w := range want {
		m = sendKey(m, tea.KeyMsg{Type: tea.KeyShiftTab})
		if m.Perm != w {
			t.Fatalf("shift+tab: got %s want %s", m.Perm, w)
		}
	}
}

func TestSlashCommands(t *testing.T) {
	m := testModel()
	m.ApplyCommand("/plan")
	if m.Agent != "plan" {
		t.Fatalf("expected plan, got %s", m.Agent)
	}
	m.ApplyCommand("/mode auto-approve")
	if m.Perm != "auto-approve" {
		t.Fatalf("expected auto-approve, got %s", m.Perm)
	}
	m.ApplyCommand("/compact")
}

func TestResumePickerAndModal(t *testing.T) {
	m := testModel()
	m.ApplyCommand("/resume")
	if !m.Picking {
		t.Fatal("expected picker open")
	}
	m.Modal = &PermModal{RequestID: "r1", Tool: "bash", Target: "go test ./..."}
	m = sendKey(m, key("", tea.KeyRight))
	m = sendKey(m, key("", tea.KeyEnter))
	if m.Modal != nil {
		t.Fatal("modal should resolve on enter")
	}
	if !strings.Contains(m.Status, "allow_session") {
		t.Fatalf("expected allow_session, got %q", m.Status)
	}
}

func TestCompletion(t *testing.T) {
	got := CompleteInput("read @src/ma", []string{"src/main.go"}, nil)
	if len(got) != 1 || got[0] != "@src/main.go" {
		t.Fatalf("file completion: %v", got)
	}
	got = CompleteInput("see [[mem", nil, []string{"memory-notes"})
	if len(got) != 1 {
		t.Fatalf("memory completion: %v", got)
	}
}

func TestViewRenders(t *testing.T) {
	m := testModel()
	m.Width = 80
	v := m.View()
	if !strings.Contains(v, "BUILD") || !strings.Contains(v, "ASK") {
		t.Fatalf("view missing pills:\n%s", v)
	}
}

func TestSpaceInsertsRune(t *testing.T) {
	// Bare space arrives as KeySpace (not KeyRunes); it must type a space.
	m := testModel()
	next, _ := m.Update(tea.KeyMsg{Type: tea.KeyRunes, Runes: []rune("hi")})
	m = next.(Model)
	next, _ = m.Update(tea.KeyMsg{Type: tea.KeySpace, Runes: []rune(" ")})
	m = next.(Model)
	next, _ = m.Update(tea.KeyMsg{Type: tea.KeyRunes, Runes: []rune("there")})
	m = next.(Model)
	if m.Input != "hi there" {
		t.Fatalf("space input broken, got %q", m.Input)
	}
}

func ctxEvent(typ, sid string, data map[string]interface{}) EventMsg {
	raw, _ := json.Marshal(data)
	return EventMsg{Event: sdk.Event{Type: typ, SessionID: sid, Data: raw}}
}

func TestContextGaugeAndFooter(t *testing.T) {
	m := testModel()
	next, _ := m.Update(ctxEvent("turn.context", "s1", map[string]interface{}{
		"context_tokens": 12345, "max_tokens": 200000,
	}))
	m = next.(Model)
	if m.CtxUsed != 12345 || m.CtxMax != 200000 {
		t.Fatalf("ctx gauge not stored: %+v", m)
	}
	m.Width = 100
	if v := m.View(); !strings.Contains(v, "ctx ~12.3k/200.0k") {
		t.Fatalf("header missing ctx pill:\n%s", v)
	}
	next, _ = m.Update(ctxEvent("turn.completed", "s1", map[string]interface{}{
		"text": "done", "model": "prov/m", "duration_ms": float64(2000),
		"output_tokens": float64(100), "context_tokens": float64(13000), "max_tokens": float64(200000),
	}))
	m = next.(Model)
	if m.Working {
		t.Fatal("completed must clear Working")
	}
	found := false
	for _, ln := range m.Lines {
		if strings.Contains(ln, "ctx ~13.0k/200.0k") {
			found = true
		}
	}
	if !found {
		t.Fatalf("footer missing ctx summary: %v", m.Lines)
	}
}

func TestModelPickerConnect(t *testing.T) {
	var patched, routerPut int
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		switch {
		case r.URL.Path == "/api/v1/providers":
			_ = json.NewEncoder(w).Encode(map[string]interface{}{"providers": []map[string]string{{"name": "copilot", "protocol": "copilot"}, {"name": "other", "protocol": "openai-compatible"}}})
		case r.URL.Path == "/api/v1/models":
			if r.URL.Query().Get("provider") != "copilot" {
				w.WriteHeader(502)
				_, _ = w.Write([]byte(`{"error":"no models"}`))
				return
			}
			_ = json.NewEncoder(w).Encode(map[string]interface{}{"models": []map[string]string{{"id": "copilot/gpt-4o", "name": "GPT-4o"}}})
		case r.Method == http.MethodPatch:
			patched++
			_ = json.NewEncoder(w).Encode(sdk.Session{ID: "s1", Agent: "build", PermissionMode: "ask"})
		case r.Method == http.MethodPut:
			routerPut++
			_ = json.NewEncoder(w).Encode(map[string]string{"status": "ok"})
		default:
			w.WriteHeader(200)
		}
	}))
	defer srv.Close()
	m := New(sdk.New(srv.URL, ""), &sdk.Session{ID: "s1", Agent: "build", PermissionMode: "ask"})
	cmd := m.ApplyCommand("/model")
	if cmd == nil {
		t.Fatal("expected models fetch cmd")
	}
	msg := cmd()
	mm, ok := msg.(ModelsMsg)
	if !ok || len(mm.Models) != 1 || mm.Models[0] != "copilot/gpt-4o" {
		t.Fatalf("models msg: %#v", msg)
	}
	next, _ := m.Update(mm)
	m = next.(Model)
	if !m.PickingModel || len(m.ModelOptions) != 1 {
		t.Fatal("picker did not open")
	}
	next, cmd = m.Update(key("", tea.KeyEnter))
	m = next.(Model)
	if cmd == nil {
		t.Fatal("expected connect cmd on enter")
	}
	if out := cmd(); out == nil {
		t.Fatal("connect cmd returned nil")
	}
	if patched != 1 || routerPut != 1 {
		t.Fatalf("daemon sync: patched=%d routerPut=%d", patched, routerPut)
	}
}

func TestConnectWizard(t *testing.T) {
	var polls int
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		switch r.URL.Path {
		case "/api/v1/providers":
			_ = json.NewEncoder(w).Encode(map[string]interface{}{"providers": []map[string]string{{"name": "copilot", "protocol": "copilot"}, {"name": "other", "protocol": "openai-compatible"}}})
		case "/api/v1/auth/device/start":
			_ = json.NewEncoder(w).Encode(map[string]interface{}{
				"device_code": "dev-9", "user_code": "WXYZ-1234",
				"verification_uri": "https://example.test/d", "expires_in": 60, "interval": 1,
			})
		case "/api/v1/auth/device/poll":
			polls++
			if polls == 1 {
				_ = json.NewEncoder(w).Encode(map[string]string{"status": "pending"})
				return
			}
			_ = json.NewEncoder(w).Encode(map[string]string{"status": "authorized"})
		default:
			w.WriteHeader(404)
		}
	}))
	defer srv.Close()
	m := New(sdk.New(srv.URL, ""), &sdk.Session{ID: "s1", Agent: "build", PermissionMode: "ask"})

	// Bare /connect opens the provider picker.
	cmd := m.ApplyCommand("/connect")
	if cmd == nil {
		t.Fatal("expected provider fetch cmd")
	}
	msg := cmd()
	cp, ok := msg.(ConnectProvidersMsg)
	if !ok || len(cp.Providers) != 1 {
		t.Fatalf("providers msg: %#v", msg)
	}
	next, _ := m.Update(cp)
	m = next.(Model)
	if !m.PickingConnect {
		t.Fatal("connect picker did not open")
	}
	// Pick copilot -> prompts for GitHub URL.
	next, cmd = m.Update(key("", tea.KeyEnter))
	m = next.(Model)
	if !m.AwaitingURL {
		t.Fatal("expected URL prompt after provider pick")
	}
	// Enter URL -> device start.
	m.Input = "https://github.com"
	next, cmd = m.Update(key("", tea.KeyEnter))
	_ = next
	if cmd == nil {
		t.Fatal("expected device start cmd")
	}
	out := cmd()
	dm, ok := out.(DeviceStartedMsg)
	if !ok || dm.UserCode != "WXYZ-1234" {
		t.Fatalf("device started msg: %#v", out)
	}
	// Poll loop authorizes on second try.
	next, cmd = m.Update(dm)
	_ = next
	if cmd == nil {
		t.Fatal("expected poll cmd")
	}
	done := cmd()
	td, ok := done.(TurnDoneMsg)
	if !ok || td.Text != "connected copilot via web auth" {
		t.Fatalf("connect result: %#v", done)
	}
	if polls != 2 {
		t.Fatalf("expected 2 polls, got %d", polls)
	}
}

func TestConnectEmptyInputUsesDefaultHost(t *testing.T) {
	// Regression: pressing Enter on the empty GitHub URL prompt must start
	// device auth with github.com (previously swallowed by the empty-input
	// early return).
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path == "/api/v1/auth/device/start" {
			_ = json.NewEncoder(w).Encode(map[string]interface{}{
				"device_code": "dev-9", "user_code": "WXYZ-1234",
				"verification_uri": "https://github.com/login/device",
				"expires_in":       60, "interval": 1,
			})
			return
		}
		w.WriteHeader(404)
	}))
	defer srv.Close()
	m := New(sdk.New(srv.URL, ""), &sdk.Session{ID: "s1", Agent: "build", PermissionMode: "ask"})
	m.AwaitingURL = true
	m.ConnectProvider = "copilot"
	m.Input = "" // user just hits enter for the default host
	next, cmd := m.Update(key("", tea.KeyEnter))
	m = next.(Model)
	if cmd == nil {
		t.Fatal("empty enter must start device auth, got nil cmd")
	}
	if m.AwaitingURL {
		t.Fatal("AwaitingURL should clear on submit")
	}
	msg := cmd()
	dm, ok := msg.(DeviceStartedMsg)
	if !ok {
		t.Fatalf("expected DeviceStartedMsg, got %#v", msg)
	}
	if dm.GitHubURL != "https://github.com" || dm.UserCode != "WXYZ-1234" {
		t.Fatalf("device msg: %+v", dm)
	}
}

func TestModelPickerShowsDaemonError(t *testing.T) {
	// When every provider fails, /model must surface the daemon error
	// instead of only hinting at /connect (the failing-secret case).
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		switch {
		case r.URL.Path == "/api/v1/providers":
			_ = json.NewEncoder(w).Encode(map[string]interface{}{"providers": []map[string]string{{"name": "copilot", "protocol": "copilot"}}})
		case r.URL.Path == "/api/v1/models":
			w.WriteHeader(502)
			_, _ = w.Write([]byte(`{"error":"copilot token exchange: status 401"}`))
		default:
			w.WriteHeader(200)
		}
	}))
	defer srv.Close()
	m := New(sdk.New(srv.URL, ""), &sdk.Session{ID: "s1", Agent: "build", PermissionMode: "ask"})
	cmd := m.ApplyCommand("/model")
	if cmd == nil {
		t.Fatal("expected models fetch cmd")
	}
	msg, ok := cmd().(TurnDoneMsg)
	if !ok {
		t.Fatalf("expected TurnDoneMsg, got %#v", cmd())
	}
	if !strings.Contains(msg.Text, "no live models:") || !strings.Contains(msg.Text, "401") {
		t.Fatalf("error surfaced: %q", msg.Text)
	}
}

func testModelWS(ws string) Model {
	return New(nil, &sdk.Session{ID: "s1", Agent: "build", PermissionMode: "ask", WorkspaceDir: ws})
}

func typeText(m Model, s string) (Model, tea.Cmd) {
	next, cmd := m.Update(tea.KeyMsg{Type: tea.KeyRunes, Runes: []rune(s)})
	return next.(Model), cmd
}

func TestSlashPopupListsWithDescriptions(t *testing.T) {
	m := testModelWS(t.TempDir())
	m, _ = typeText(m, "/")
	if !m.Completing || len(m.CompleteOptions) == 0 {
		t.Fatalf("expected open popup, completing=%v options=%v", m.Completing, m.CompleteOptions)
	}
	found := false
	for _, o := range m.CompleteOptions {
		if o.Text == "/plan" && o.Desc != "" {
			found = true
		}
	}
	if !found {
		t.Fatalf("missing /plan with description: %+v", m.CompleteOptions)
	}
	if !strings.Contains(m.View(), "/plan") || !strings.Contains(m.View(), "read-only plan mode") {
		t.Fatalf("popup not rendered with description:\n%s", m.View())
	}
}

func TestSlashPopupFilterAndTabAccept(t *testing.T) {
	m := testModelWS(t.TempDir())
	m, _ = typeText(m, "/p")
	if len(m.CompleteOptions) != 1 || m.CompleteOptions[0].Text != "/plan" {
		t.Fatalf("expected only /plan, got %+v", m.CompleteOptions)
	}
	next, _ := m.Update(tea.KeyMsg{Type: tea.KeyTab})
	m = next.(Model)
	if m.Input != "/plan " {
		t.Fatalf("tab should accept to '/plan ', got %q", m.Input)
	}
	if m.Completing {
		t.Fatal("popup should close after accept")
	}
	// Agent stayed put: Tab was consumed by completion, not the toggle.
	if m.Agent != "build" {
		t.Fatalf("agent must not toggle on completion accept, got %s", m.Agent)
	}
}

func TestSlashPopupNavigateAndEsc(t *testing.T) {
	m := testModelWS(t.TempDir())
	m, _ = typeText(m, "/")
	if len(m.CompleteOptions) < 3 {
		t.Fatalf("expected full menu, got %d", len(m.CompleteOptions))
	}
	mm := m
	for i := 0; i < 2; i++ {
		var nx tea.Model
		nx, _ = mm.Update(tea.KeyMsg{Type: tea.KeyDown})
		mm = nx.(Model)
	}
	if mm.CompleteIdx != 2 {
		t.Fatalf("expected idx 2, got %d", mm.CompleteIdx)
	}
	next, _ := mm.Update(tea.KeyMsg{Type: tea.KeyEsc})
	if next.(Model).Completing {
		t.Fatal("esc should close popup")
	}
	if next.(Model).Input != "/" {
		t.Fatalf("esc must keep input, got %q", next.(Model).Input)
	}
}

func TestCustomCommandFileNoteCompletion(t *testing.T) {
	ws := t.TempDir()
	os.MkdirAll(filepath.Join(ws, ".lumina", "commands"), 0o755)
	os.WriteFile(filepath.Join(ws, ".lumina", "commands", "deploy.md"), []byte("# Deploy\nDeploys the app to prod.\n"), 0o644)
	os.WriteFile(filepath.Join(ws, "README.md"), []byte("hi"), 0o644)
	os.MkdirAll(filepath.Join(ws, "memory"), 0o755)
	os.WriteFile(filepath.Join(ws, "memory", "plan.md"), []byte("# plan"), 0o644)

	m := testModelWS(ws)
	m, cmd := typeText(m, "/dep")
	if cmd == nil {
		t.Fatal("expected lazy loader cmd on first slash use")
	}
	msg := cmd()
	next, _ := m.Update(msg)
	m = next.(Model)
	names := []string{}
	for _, o := range m.CompleteOptions {
		names = append(names, o.Text)
	}
	if len(names) != 1 || names[0] != "/deploy" {
		t.Fatalf("custom command not suggested: %v", names)
	}
	if m.CompleteOptions[0].Desc != "Deploy" && m.CompleteOptions[0].Desc != "Deploys the app to prod." {
		t.Fatalf("custom description missing: %q", m.CompleteOptions[0].Desc)
	}

	// @file completion from the same loaded workspace.
	m2 := testModelWS(ws)
	m2, cmd2 := typeText(m2, "read @READ")
	if cmd2 == nil {
		t.Fatal("expected loader cmd for @file too")
	}
	next2, _ := m2.Update(cmd2())
	m2 = next2.(Model)
	if len(m2.CompleteOptions) != 1 || m2.CompleteOptions[0].Text != "@README.md" {
		t.Fatalf("@file options: %+v", m2.CompleteOptions)
	}
	next2, _ = m2.Update(tea.KeyMsg{Type: tea.KeyTab})
	if next2.(Model).Input != "read @README.md" {
		t.Fatalf("tab accept @file: %q", next2.(Model).Input)
	}

	// [[memory]] completion.
	m3 := testModelWS(ws)
	m3, cmd3 := typeText(m3, "see [[pl")
	if cmd3 == nil {
		t.Fatal("expected loader cmd for [[note]]")
	}
	next3, _ := m3.Update(cmd3())
	m3 = next3.(Model)
	if len(m3.CompleteOptions) != 1 || m3.CompleteOptions[0].Text != "[[plan]]" {
		t.Fatalf("[[note]] options: %+v", m3.CompleteOptions)
	}
}

func TestModelFilterConnectsSingleMatch(t *testing.T) {
	// "/model <text>" filters the live list: one match connects directly
	// (PATCH + PUT, like picking it), several open a filtered picker.
	var patched, routerPut int
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		switch {
		case r.URL.Path == "/api/v1/providers":
			_ = json.NewEncoder(w).Encode(map[string]interface{}{"providers": []map[string]string{{"name": "copilot", "protocol": "copilot"}}})
		case r.URL.Path == "/api/v1/models":
			_ = json.NewEncoder(w).Encode(map[string]interface{}{"models": []map[string]string{
				{"id": "copilot/gpt-4o", "name": "GPT-4o"},
				{"id": "copilot/claude-sonnet", "name": "Claude Sonnet"},
			}})
		case r.Method == http.MethodPatch:
			patched++
			_ = json.NewEncoder(w).Encode(sdk.Session{ID: "s1", Agent: "build", PermissionMode: "ask"})
		case r.Method == http.MethodPut:
			routerPut++
			_ = json.NewEncoder(w).Encode(map[string]string{"status": "ok"})
		default:
			w.WriteHeader(200)
		}
	}))
	defer srv.Close()
	newModel := func() Model {
		return New(sdk.New(srv.URL, ""), &sdk.Session{ID: "s1", Agent: "build", PermissionMode: "ask"})
	}

	// Single (case-insensitive) match connects directly.
	m := newModel()
	msg := m.ApplyCommand("/model GPT")()
	td, ok := msg.(TurnDoneMsg)
	if !ok || td.Text != "connected to copilot/gpt-4o" || patched != 1 || routerPut != 1 {
		t.Fatalf("single match: %#v patched=%d routerPut=%d", msg, patched, routerPut)
	}

	// Several matches open a filtered picker.
	m = newModel()
	msg = m.ApplyCommand("/model copilot")()
	mm, ok := msg.(ModelsMsg)
	if !ok || len(mm.Models) != 2 {
		t.Fatalf("multi match: %#v", msg)
	}

	// No match reports the filter.
	m = newModel()
	msg = m.ApplyCommand("/model zzz")()
	td, ok = msg.(TurnDoneMsg)
	if !ok || td.Text != `no models match "zzz"` {
		t.Fatalf("no match: %#v", msg)
	}
}

func TestModelPickerListsAllProviders(t *testing.T) {
	// /model lists every provider's live models, not just copilot; bare
	// ids are provider-qualified (as the web picker does).
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		switch {
		case r.URL.Path == "/api/v1/providers":
			_ = json.NewEncoder(w).Encode(map[string]interface{}{"providers": []map[string]string{
				{"name": "copilot", "protocol": "copilot"},
				{"name": "lumina", "protocol": "openai-responses"},
				{"name": "dead", "protocol": "openai-compatible"},
			}})
		case r.URL.Path == "/api/v1/models":
			q := r.URL.Query().Get("provider")
			switch q {
			case "copilot":
				_ = json.NewEncoder(w).Encode(map[string]interface{}{"models": []map[string]string{{"id": "copilot/gpt-4o", "name": "GPT-4o"}}})
			case "lumina":
				_ = json.NewEncoder(w).Encode(map[string]interface{}{"models": []map[string]string{{"id": "muse-spark-1.3-contributor", "name": ""}, {"id": "deepseek-v4.1-flash", "name": ""}}})
			default:
				w.WriteHeader(502)
				_, _ = w.Write([]byte(`{"error":"unknown provider"}`))
			}
		default:
			w.WriteHeader(200)
		}
	}))
	defer srv.Close()
	m := New(sdk.New(srv.URL, ""), &sdk.Session{ID: "s1", Agent: "build", PermissionMode: "ask"})
	msg, ok := m.ApplyCommand("/model")().(ModelsMsg)
	if !ok {
		t.Fatalf("expected ModelsMsg, got %#v", msg)
	}
	want := map[string]bool{"copilot/gpt-4o": true, "lumina/muse-spark-1.3-contributor": true, "lumina/deepseek-v4.1-flash": true}
	if len(msg.Models) != 3 {
		t.Fatalf("models=%v", msg.Models)
	}
	for _, id := range msg.Models {
		if !want[id] {
			t.Fatalf("unexpected id %q in %v", id, msg.Models)
		}
	}
	// Filter reaches across providers: one match connects directly.
	m = New(sdk.New(srv.URL, ""), &sdk.Session{ID: "s1", Agent: "build", PermissionMode: "ask"})
	// add PATCH/PUT to the shared mock via a second server is overkill;
	// instead filter to two matches and check the filtered picker.
	msg2, ok := m.ApplyCommand("/model 4")().(ModelsMsg)
	if !ok || len(msg2.Models) != 2 {
		t.Fatalf("filtered: %#v", msg2)
	}
}

func testQModal() *QuestionModal {
	return &QuestionModal{
		RequestID: "q1",
		Questions: []QuestionItem{
			{Question: "pick one", Header: "Pick", Options: []QuestionOption{{Label: "a", Description: "first"}, {Label: "b"}}, Multiple: false},
			{Question: "pick any", Options: []QuestionOption{{Label: "x"}, {Label: "y"}}, Multiple: true},
		},
	}
}

func TestQuestionModalAnswerAndAdvance(t *testing.T) {
	m := testModel()
	m.QModal = testQModal()
	// Type "2" + enter answers first question with option b.
	m = sendKey(m, key("2", tea.KeyRunes))
	m = sendKey(m, key("", tea.KeyEnter))
	if m.QModal == nil || m.QModal.Idx != 1 {
		t.Fatalf("should advance to question 2: %+v", m.QModal)
	}
	// Type "1,2" + enter answers multi question with both labels.
	m = sendKey(m, key("1", tea.KeyRunes))
	m = sendKey(m, key(",", tea.KeyRunes))
	m = sendKey(m, key("2", tea.KeyRunes))
	m = sendKey(m, key("", tea.KeyEnter))
	if m.QModal != nil {
		t.Fatal("modal should close after last answer")
	}
	if !strings.Contains(m.Status, "answered") {
		t.Fatalf("status=%q", m.Status)
	}
}

func TestQuestionModalCursorPick(t *testing.T) {
	m := testModel()
	m.QModal = &QuestionModal{RequestID: "q1", Questions: []QuestionItem{{Question: "q", Options: []QuestionOption{{Label: "a"}, {Label: "b"}}}}}
	m = sendKey(m, key("", tea.KeyDown))
	m = sendKey(m, key("", tea.KeyEnter))
	if m.QModal != nil {
		t.Fatal("single-question modal should close")
	}
}

func TestQuestionModalDismiss(t *testing.T) {
	m := testModel()
	m.QModal = testQModal()
	m = sendKey(m, key("", tea.KeyEsc))
	if m.QModal != nil {
		t.Fatal("modal should close on esc")
	}
	if !strings.Contains(m.Status, "dismissed") {
		t.Fatalf("status=%q", m.Status)
	}
}

func TestParseQuestionAnswer(t *testing.T) {
	q := QuestionItem{Question: "q", Options: []QuestionOption{{Label: "a"}, {Label: "b"}}}
	if got := parseQuestionAnswer(q, "2", 0); len(got) != 1 || got[0] != "b" {
		t.Fatalf("number pick: %v", got)
	}
	mq := QuestionItem{Question: "q", Options: []QuestionOption{{Label: "a"}, {Label: "b"}}, Multiple: true}
	if got := parseQuestionAnswer(mq, "1,2", 0); len(got) != 2 {
		t.Fatalf("multi pick: %v", got)
	}
	if got := parseQuestionAnswer(q, "custom thing", 0); len(got) != 1 || got[0] != "custom thing" {
		t.Fatalf("free text: %v", got)
	}
	if got := parseQuestionAnswer(q, "", 1); len(got) != 1 || got[0] != "b" {
		t.Fatalf("cursor pick: %v", got)
	}
}

func TestVariantCommand(t *testing.T) {
	m := testModel()
	if m.Effort != "" {
		t.Fatalf("default variant must be auto, got %q", m.Effort)
	}
	// Direct set, case-insensitive (xhigh is the requested example).
	m.ApplyCommand("/variant XHIGH")
	if m.Effort != "xhigh" {
		t.Fatalf("variant=%q want xhigh", m.Effort)
	}
	if v := m.View(); !strings.Contains(v, "variant:xhigh") {
		t.Fatalf("header missing variant pill:\n%s", v)
	}
	// /effort alias + auto reset.
	m.ApplyCommand("/effort auto")
	if m.Effort != "" {
		t.Fatalf("auto must clear to %q, got %q", "", m.Effort)
	}
	// Bare command opens the picker with all six variants.
	m.ApplyCommand("/variant")
	if !m.PickingVariant || len(m.VariantOptions) != 6 {
		t.Fatalf("picker=%v options=%v", m.PickingVariant, m.VariantOptions)
	}
	// Unknown names report the choices without changing state.
	m.PickingVariant = false
	m.ApplyCommand("/variant ultra")
	if m.PickingVariant || m.Effort != "" {
		t.Fatalf("unknown must not change state: effort=%q picking=%v", m.Effort, m.PickingVariant)
	}
	// Picker enter applies the highlighted variant.
	m.ApplyCommand("/variant")
	m.PickIdx = 4 // xhigh in auto/low/medium/high/xhigh/max order
	next, _ := m.Update(key("", tea.KeyEnter))
	m = next.(Model)
	if m.Effort != "xhigh" || m.PickingVariant {
		t.Fatalf("picker enter: effort=%q picking=%v", m.Effort, m.PickingVariant)
	}
}

func TestVariantPromptCarriesEffort(t *testing.T) {
	var gotEffort, gotText string
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path == "/api/v1/sessions/s1/prompt" {
			var body map[string]interface{}
			_ = json.NewDecoder(r.Body).Decode(&body)
			gotText, _ = body["text"].(string)
			gotEffort, _ = body["effort"].(string)
			_ = json.NewEncoder(w).Encode(map[string]string{"status": "accepted"})
			return
		}
		w.WriteHeader(200)
	}))
	defer srv.Close()
	m := New(sdk.New(srv.URL, ""), &sdk.Session{ID: "s1", Agent: "build", PermissionMode: "ask"})
	m.ApplyCommand("/variant high")
	m.Input = "hello"
	next, cmd := m.Update(key("", tea.KeyEnter))
	_ = next
	if cmd == nil {
		t.Fatal("expected prompt cmd")
	}
	if msg := cmd(); msg != nil {
		t.Fatalf("prompt should succeed silently, got %#v", msg)
	}
	if gotText != "hello" || gotEffort != "high" {
		t.Fatalf("prompt body: text=%q effort=%q", gotText, gotEffort)
	}
}

func TestQuestionAskedEvent(t *testing.T) {
	m := testModel()
	next, _ := m.Update(ctxEvent("question.asked", "s1", map[string]interface{}{
		"request_id": "q9",
		"questions": []interface{}{map[string]interface{}{
			"question": "pick?", "header": "H",
			"options": []interface{}{map[string]interface{}{"label": "a", "description": "d"}},
		}},
	}))
	m = next.(Model)
	if m.QModal == nil || m.QModal.RequestID != "q9" || len(m.QModal.Questions) != 1 {
		t.Fatalf("qmodal not set: %+v", m.QModal)
	}
	if m.QModal.Questions[0].Options[0].Label != "a" {
		t.Fatalf("options not decoded: %+v", m.QModal.Questions[0])
	}
	if v := m.View(); !strings.Contains(v, "pick?") {
		t.Fatalf("view missing question: %q", v[len(v)-200:])
	}
}
