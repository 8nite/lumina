package tui

import (
	"context"
	"net/http/httptest"
	"strings"
	"testing"
	"time"

	"github.com/8nite/lumina/internal/agent"
	"github.com/8nite/lumina/internal/server"
	"github.com/8nite/lumina/pkg/sdk"
	tea "github.com/charmbracelet/bubbletea"
)

// Missed turn.started (SSE gap / late attach) must not leave the timeline
// looking idle: any live row recovers the spinner, mirroring the web UI.
func TestMissedStartedRecoversOnLiveRows(t *testing.T) {
	m := testModel()
	if m.Working {
		t.Fatal("fresh model must be idle")
	}
	m = applyEvent(m, eventMsg("turn.delta", "s1", map[string]string{"delta": "hel"}))
	if !m.Working {
		t.Fatal("delta without started must mark working")
	}
	m = testModel()
	m = applyEvent(m, eventMsg("turn.progress", "s1", map[string]string{"stage": "tool: bash"}))
	if !m.Working {
		t.Fatal("progress without started must mark working")
	}
	if m.Status != "tool: bash" {
		t.Fatalf("status=%q", m.Status)
	}
	// Queued-only progress must not fake a running turn.
	m = testModel()
	m = applyEvent(m, eventMsg("turn.progress", "s1", map[string]string{"stage": "queued #1"}))
	if m.Working {
		t.Fatal("queued progress must not mark working")
	}
	m = testModel()
	m = applyEvent(m, eventMsg("turn.tool", "s1",
		map[string]interface{}{"phase": "start", "tool": "read", "target": "a.go"}))
	if !m.Working {
		t.Fatal("tool without started must mark working")
	}
	// Foreign sessions still never touch the flag.
	m = testModel()
	m = applyEvent(m, eventMsg("turn.delta", "other", map[string]string{"delta": "X"}))
	if m.Working {
		t.Fatal("cross-session delta must not mark working")
	}
}

// A turn blocked on approval/question is still in-flight: the modal must
// arrive with the spinner on, otherwise the wait looks like a stall.
func TestBlockedPromptsKeepWorking(t *testing.T) {
	m := testModel()
	m = applyEvent(m, eventMsg("permission.asked", "s1",
		map[string]string{"request_id": "r1", "tool": "bash", "target": "rm -rf /tmp/x"}))
	if !m.Working || m.Modal == nil || m.Modal.RequestID != "r1" {
		t.Fatalf("permission.asked: working=%v modal=%+v", m.Working, m.Modal)
	}
	if !strings.Contains(m.Status, "waiting approval") {
		t.Fatalf("status=%q", m.Status)
	}
	m = testModel()
	m = applyEvent(m, eventMsg("question.asked", "s1", map[string]interface{}{
		"request_id": "q1", "questions": []interface{}{map[string]interface{}{"question": "pick one"}},
	}))
	if !m.Working || m.QModal == nil || m.QModal.RequestID != "q1" {
		t.Fatalf("question.asked: working=%v qmodal=%+v", m.Working, m.QModal)
	}
}

func TestApplyResyncRebuildsHistory(t *testing.T) {
	m := testModel()
	msg := ResyncMsg{
		HadState: true, Working: true, Status: "working… (reconnected — catching up…)",
		Lines: []string{"**you:** do the thing", "· read a.go → package main", "halfway there"},
	}
	m.applyResync(msg)
	if !m.Working {
		t.Fatal("running resync must set working")
	}
	joined := strings.Join(m.Lines, "\n")
	for _, want := range []string{"**you:** do the thing", "halfway there"} {
		if !strings.Contains(joined, want) {
			t.Fatalf("history missing %q in %v", want, m.Lines)
		}
	}
	// Header lines survive the rebuild.
	if len(m.Lines) < 3 || m.Lines[0] != "# lumina" {
		t.Fatalf("header lost: %v", m.Lines)
	}
	// Idle resync clears the spinner and pending buffers.
	m.applyResync(ResyncMsg{HadState: true, Lines: []string{"**you:** hi", "done"}})
	if m.Working || m.Pending != "" || m.Status != "" {
		t.Fatalf("idle resync must clear working/pending/status: %+v", m)
	}
	// Pending prompts restore their modals.
	m = testModel()
	m.applyResync(ResyncMsg{HadState: true, Working: true,
		Status: "waiting approval: bash x",
		Modal:  &PermModal{RequestID: "r9", Tool: "bash", Target: "x"}})
	if m.Modal == nil || m.Modal.RequestID != "r9" || !m.Working {
		t.Fatalf("perm modal not restored: %+v", m.Modal)
	}
	// Unreachable daemon keeps the local view untouched.
	m = testModel()
	n := len(m.Lines)
	m.applyResync(ResyncMsg{HadState: false})
	if len(m.Lines) != n || m.Working {
		t.Fatal("failed resync must not touch the timeline")
	}
}

func TestApplyResyncKeepsOptimisticTail(t *testing.T) {
	m := testModel()
	m.appendLine("**you:** just sent")
	// Export raced ahead of the daemon's AddTurn: it lacks our row. The
	// rebuild must not delete the just-sent prompt.
	m.applyResync(ResyncMsg{HadState: true, Working: true,
		Status:    "working…",
		Lines:     []string{"**you:** older", "older reply"}})
	last := m.Lines[len(m.Lines)-1]
	if last != "**you:** just sent" {
		t.Fatalf("optimistic tail lost: %v", m.Lines)
	}
}

func TestResyncCmdNilWhenDetached(t *testing.T) {
	attached := New(sdk.New("http://127.0.0.1:1", ""), &sdk.Session{ID: "s1"})
	if attached.resyncCmd() == nil {
		t.Fatal("attached model must offer a resync cmd")
	}
	m := New(nil, &sdk.Session{ID: "s1"})
	if m.resyncCmd() != nil {
		t.Fatal("detached model must not offer a resync cmd")
	}
	next, cmd := attached.Update(RequestResyncMsg{})
	if cmd == nil || next == nil {
		t.Fatal("RequestResyncMsg must yield a fetch cmd when attached")
	}
	next, cmd = m.Update(RequestResyncMsg{})
	if cmd != nil {
		t.Fatal("RequestResyncMsg must yield no cmd when detached")
	}
	_ = next
}

// Switching sessions must drop the previous session's live state and
// fetch the new session's history, for both attach paths (/continue +
// /resume picker).
func TestAttachSwitchResyncs(t *testing.T) {
	attached := func() Model {
		return New(sdk.New("http://127.0.0.1:1", ""), &sdk.Session{ID: "s1", Agent: "build", PermissionMode: "ask"})
	}
	m := attached()
	m.Working = true
	m.Pending = "stale stream"
	m.Modal = &PermModal{RequestID: "old", Tool: "bash", Target: "x"}
	next, cmd := m.Update(AttachedMsg{Session: &sdk.Session{ID: "s2", Agent: "plan", PermissionMode: "bypass"}})
	nm := next.(Model)
	if nm.Session.ID != "s2" || nm.Agent != "plan" || nm.Perm != "bypass" {
		t.Fatalf("attach did not switch session: %+v", nm.Session)
	}
	if nm.Working || nm.Pending != "" || nm.Modal != nil {
		t.Fatal("attach must drop the previous session's live state")
	}
	if cmd == nil {
		t.Fatal("attach must fetch the new session's history")
	}

	m = attached()
	m.Sessions = []*sdk.Session{{ID: "s9", Agent: "build", PermissionMode: "ask"}}
	m.Picking = true
	m.PickIdx = 0
	m.Working = true
	next, cmd = m.Update(key("", tea.KeyEnter))
	nm = next.(Model)
	if nm.Session.ID != "s9" {
		t.Fatalf("picker did not attach: %+v", nm.Session)
	}
	if nm.Working || nm.Picking {
		t.Fatal("picker attach must reset live state and close the picker")
	}
	if cmd == nil {
		t.Fatal("picker attach must fetch the new session's history")
	}
}
// resync shows partial history + spinner (the reported bug showed neither),
// and the post-turn resync shows the full transcript idle.
func TestResyncCmdAgainstLiveDaemon(t *testing.T) {
	st, err := agent.Open(t.TempDir() + "/s.db")
	if err != nil {
		t.Fatal(err)
	}
	defer st.Close()
	srv := server.New(server.Options{})
	srv.Store = st
	release := make(chan struct{})
	srv.Exec = func(ctx context.Context, id string, in agent.TurnInput) (string, error) {
		// Mirror Runner.RunPrompt: user turn persists immediately.
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
	httpSrv := httptest.NewServer(srv.Handler())
	defer httpSrv.Close()
	c := sdk.New(httpSrv.URL, "")

	ctx := context.Background()
	sess, err := c.CreateSession(ctx, "/tmp/ws", "build", "ask")
	if err != nil {
		t.Fatal(err)
	}
	m := New(c, sess)
	// Initial attach loads whatever history exists (empty here, idle).
	if cmd := m.Init(); cmd == nil {
		t.Fatal("Init must load history when attached")
	}

	promptErr := make(chan error, 1)
	go func() { promptErr <- c.Prompt(ctx, sess.ID, sdk.PromptInput{Text: "long task please"}) }()

	// Wait for the turn to be in flight server-side.
	deadline := time.Now().Add(5 * time.Second)
	for time.Now().Before(deadline) {
		s, err := c.GetSession(ctx, sess.ID)
		if err == nil && s.Running {
			break
		}
		time.Sleep(10 * time.Millisecond)
	}
	runResync := func() ResyncMsg {
		cmd := m.resyncCmd()
		if cmd == nil {
			t.Fatal("no resync cmd while attached")
		}
		msg, ok := cmd().(ResyncMsg)
		if !ok {
			t.Fatalf("resync cmd returned %T", cmd())
		}
		return msg
	}
	// Poll: POST returns before the async runner persists, so the first
	// resync can race ahead of AddTurn on a loaded box.
	var mid ResyncMsg
	deadline = time.Now().Add(5 * time.Second)
	for time.Now().Before(deadline) {
		mid = runResync()
		hit := false
		for _, l := range mid.Lines {
			if strings.Contains(l, "long task please") {
				hit = true
			}
		}
		if hit && mid.Working {
			break
		}
		time.Sleep(20 * time.Millisecond)
	}
	m.applyResync(mid)
	found := false
	for _, l := range m.Lines {
		if strings.Contains(l, "long task please") {
			found = true
		}
	}
	if !found || !m.Working {
		t.Fatalf("mid-turn resync must show user history + spinner: working=%v lines=%v", m.Working, m.Lines)
	}

	close(release)
	if err := <-promptErr; err != nil {
		t.Fatalf("prompt: %v", err)
	}
	// Terminal state converges after completion.
	deadline = time.Now().Add(10 * time.Second)
	for time.Now().Before(deadline) {
		mid = runResync()
		hitUser, hitDone := false, false
		for _, l := range mid.Lines {
			if strings.Contains(l, "long task please") {
				hitUser = true
			}
			if strings.Contains(l, "long task done") {
				hitDone = true
			}
		}
		if hitUser && hitDone && !mid.Working {
			break
		}
		time.Sleep(50 * time.Millisecond)
	}
	m.applyResync(mid)
	joined := strings.Join(m.Lines, "\n")
	if !strings.Contains(joined, "long task please") || !strings.Contains(joined, "long task done") {
		t.Fatalf("final resync must render full history: %v", m.Lines)
	}
	if m.Working {
		t.Fatal("final resync must be idle")
	}
}

// A reconnected client recovers a pending approval via resync, so a turn
// waiting on permission no longer looks stuck with no modal.
func TestResyncRestoresPendingPermission(t *testing.T) {
	st, err := agent.Open(t.TempDir() + "/s.db")
	if err != nil {
		t.Fatal(err)
	}
	defer st.Close()
	srv := server.New(server.Options{})
	srv.Store = st
	srv.Exec = func(ctx context.Context, id string, in agent.TurnInput) (string, error) {
		d, err := srv.RequestPermission(ctx, id, "bash", "rm -rf /tmp/x")
		if err != nil {
			return "", err
		}
		return "decided:" + d, nil
	}
	httpSrv := httptest.NewServer(srv.Handler())
	defer httpSrv.Close()
	c := sdk.New(httpSrv.URL, "")
	ctx := context.Background()
	sess, err := c.CreateSession(ctx, "/tmp/ws", "build", "ask")
	if err != nil {
		t.Fatal(err)
	}
	m := New(c, sess)
	done := make(chan error, 1)
	go func() { done <- c.Prompt(ctx, sess.ID, sdk.PromptInput{Text: "do risky thing"}) }()

	var msg ResyncMsg
	deadline := time.Now().Add(5 * time.Second)
	for time.Now().Before(deadline) {
		msg, _ = m.resyncCmd()().(ResyncMsg)
		if msg.Modal != nil && msg.Modal.RequestID != "" {
			break
		}
		time.Sleep(20 * time.Millisecond)
	}
	if msg.Modal == nil || msg.Modal.Tool != "bash" {
		t.Fatalf("resync must restore pending approval: %+v", msg.Modal)
	}
	m.applyResync(msg)
	if m.Modal == nil || !m.Working {
		t.Fatalf("modal/working not restored: %+v working=%v", m.Modal, m.Working)
	}
	if err := c.AnswerPermission(ctx, msg.Modal.RequestID, "allow_once"); err != nil {
		t.Fatal(err)
	}
	select {
	case err := <-done:
		if err != nil {
			t.Fatalf("prompt: %v", err)
		}
	case <-time.After(10 * time.Second):
		t.Fatal("turn never completed after approval")
	}
}
