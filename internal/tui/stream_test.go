package tui

import (
	"encoding/json"
	"strings"
	"testing"

	"github.com/8nite/lumina/pkg/sdk"
	tea "github.com/charmbracelet/bubbletea"
)

func eventMsg(typ, session string, data interface{}) EventMsg {
	raw, _ := json.Marshal(data)
	return EventMsg{Event: sdk.Event{Type: typ, SessionID: session, Data: raw}}
}

func applyEvent(m Model, e EventMsg) Model {
	next, _ := m.updateEvent(e.Event)
	return next.(Model)
}

func TestDeltaAccumulatesPending(t *testing.T) {
	m := testModel()
	m = applyEvent(m, eventMsg("turn.delta", "s1", map[string]string{"delta": "hel"}))
	m = applyEvent(m, eventMsg("turn.delta", "s1", map[string]string{"delta": "lo", "thinking": "plan"}))
	if m.Pending != "hello" {
		t.Fatalf("pending=%q", m.Pending)
	}
	if m.PendingThinking != "plan" {
		t.Fatalf("thinking=%q", m.PendingThinking)
	}
	if !strings.Contains(m.View(), "hello") {
		t.Fatal("pending text must render live")
	}
	// Foreign sessions never touch this timeline.
	m = applyEvent(m, eventMsg("turn.delta", "other", map[string]string{"delta": "X"}))
	if m.Pending != "hello" {
		t.Fatalf("cross-session leak: %q", m.Pending)
	}
	// Completion replaces the stream with the final render.
	m = applyEvent(m, eventMsg("turn.completed", "s1", map[string]string{"text": "hello!"}))
	if m.Pending != "" || m.PendingThinking != "" {
		t.Fatal("pending must clear on completed")
	}
	found := false
	for _, l := range m.Lines {
		if strings.Contains(l, "hello!") {
			found = true
		}
	}
	if !found {
		t.Fatal("final text missing from timeline")
	}
}

func TestTurnSummaryFooter(t *testing.T) {
	m := testModel()
	m = applyEvent(m, eventMsg("turn.completed", "s1",
		map[string]interface{}{"text": "done", "model": "prov/m", "duration_ms": float64(2500), "output_tokens": float64(100)}))
	last := m.Lines[len(m.Lines)-1]
	if !strings.Contains(last, "prov/m") || !strings.Contains(last, "2.5s") || !strings.Contains(last, "40 tok/s") {
		t.Fatalf("footer=%q", last)
	}
	// No summary fields → no extra line (old daemons, tests).
	m2 := testModel()
	n := len(m2.Lines)
	m2 = applyEvent(m2, eventMsg("turn.completed", "s1", map[string]string{"text": "done"}))
	if len(m2.Lines) != n+1 {
		t.Fatalf("expected one line, got %d", len(m2.Lines)-n)
	}
}

func TestThinkingHiddenByDefault(t *testing.T) {
	m := testModel()
	m = applyEvent(m, eventMsg("turn.delta", "s1", map[string]string{"delta": "hi", "thinking": "secret plan"}))
	if strings.Contains(m.View(), "secret plan") {
		t.Fatal("thinking must hide by default (opencode session.thinking)")
	}
	m.ApplyCommand("/thinking show")
	if !strings.Contains(m.View(), "secret plan") {
		t.Fatal("thinking must render after /thinking show")
	}
	m.ApplyCommand("/thinking hide")
	if strings.Contains(m.View(), "secret plan") {
		t.Fatal("thinking must hide after /thinking hide")
	}
}

func TestCtrlXArmsQueue(t *testing.T) {
	m := testModel()
	if got := m.sendDelivery(); got != "steer" {
		t.Fatalf("idle must steer, got %q", got)
	}
	m.Working = true
	if got := m.sendDelivery(); got != "steer" {
		t.Fatalf("working without ctrl+x must steer, got %q", got)
	}
	m = sendKey(m, tea.KeyMsg{Type: tea.KeyCtrlX})
	if !m.QueueArmed {
		t.Fatal("ctrl+x must arm queue mode")
	}
	if got := m.sendDelivery(); got != "queue" {
		t.Fatalf("armed+working must queue, got %q", got)
	}
	m = sendKey(m, tea.KeyMsg{Type: tea.KeyEsc})
	if m.QueueArmed {
		t.Fatal("esc must disarm queue mode")
	}
	// Enter while armed consumes the arm and reports queue delivery.
	m.Working = true
	m.QueueArmed = true
	m.Input = "later"
	m.Cursor = 5
	m = sendKey(m, key("", tea.KeyEnter))
	if m.QueueArmed {
		t.Fatal("enter must consume the arm")
	}
	if !m.Working {
		t.Fatal("enter must mark working")
	}
}

func TestWorkingFlagFollowsTurnEvents(t *testing.T) {
	m := testModel()
	m = applyEvent(m, eventMsg("turn.started", "s1", map[string]string{"text": "hi"}))
	if !m.Working {
		t.Fatal("turn.started must mark working")
	}
	m = applyEvent(m, eventMsg("turn.completed", "s1", map[string]string{"text": "done"}))
	if m.Working {
		t.Fatal("turn.completed must clear working")
	}
	m = applyEvent(m, eventMsg("turn.started", "s1", map[string]string{"text": "hi"}))
	m = applyEvent(m, eventMsg("turn.failed", "s1", map[string]string{"error": "boom"}))
	if m.Working {
		t.Fatal("turn.failed must clear working")
	}
	// Foreign sessions never touch the flag.
	m = applyEvent(m, eventMsg("turn.started", "other", map[string]string{"text": "hi"}))
	if m.Working {
		t.Fatal("cross-session start must not mark working")
	}
}

func TestToolEventsRenderSteps(t *testing.T) {
	m := testModel()
	n := len(m.Lines)
	m = applyEvent(m, eventMsg("turn.tool", "s1",
		map[string]interface{}{"phase": "start", "tool": "read", "target": "a.go"}))
	m = applyEvent(m, eventMsg("turn.tool", "s1",
		map[string]interface{}{"phase": "done", "tool": "read", "target": "a.go", "preview": "package main", "error": false, "duration_ms": 12}))
	if len(m.Lines) != n+2 {
		t.Fatalf("expected start+done lines, got %d", len(m.Lines)-n)
	}
	if !strings.Contains(m.Lines[n], "read") || !strings.Contains(m.Lines[n+1], "package main") {
		t.Fatalf("lines=%v", m.Lines[n:])
	}
	// Errors render inline so failures are visible mid-run.
	m = applyEvent(m, eventMsg("turn.tool", "s1",
		map[string]interface{}{"phase": "done", "tool": "bash", "target": "rm -rf /", "preview": "denied", "error": true}))
	if !strings.Contains(m.Lines[len(m.Lines)-1], "error") {
		t.Fatalf("error line missing: %q", m.Lines[len(m.Lines)-1])
	}
}
