package tui

// Variant matrix: every selectable effort applies via /variant and /effort,
// renders in the header pill, and reaches the daemon prompt body.
import (
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"

	"github.com/8nite/lumina/internal/llm"
	"github.com/8nite/lumina/pkg/sdk"
	tea "github.com/charmbracelet/bubbletea"
)

func TestVariantMatrixAllEfforts(t *testing.T) {
	cases := []struct{ in, want, pill string }{
		{"auto", "", "variant:auto"},
		{"low", "low", "variant:low"},
		{"medium", "medium", "variant:medium"},
		{"high", "high", "variant:high"},
		{"xhigh", "xhigh", "variant:xhigh"},
		{"XHIGH", "xhigh", "variant:xhigh"},
		{"max", "max", "variant:max"},
	}
	for _, tc := range cases {
		m := testModel()
		m.ApplyCommand("/variant " + tc.in)
		if m.Effort != tc.want {
			t.Fatalf("/variant %q: effort=%q want %q", tc.in, m.Effort, tc.want)
		}
		if v := m.View(); !strings.Contains(v, tc.pill) {
			t.Fatalf("/variant %q: header missing %q", tc.in, tc.pill)
		}
		m2 := testModel()
		m2.ApplyCommand("/effort " + tc.in)
		if m2.Effort != tc.want {
			t.Fatalf("/effort %q: effort=%q want %q", tc.in, m2.Effort, tc.want)
		}
	}
}

func TestVariantMatrixPromptBodies(t *testing.T) {
	var mu struct {
		bodies []map[string]interface{}
	}
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		var body map[string]interface{}
		_ = json.NewDecoder(r.Body).Decode(&body)
		mu.bodies = append(mu.bodies, body)
		_ = json.NewEncoder(w).Encode(map[string]string{"status": "accepted"})
	}))
	defer srv.Close()
	for _, v := range []string{"", "low", "medium", "high", "xhigh", "max"} {
		mu.bodies = nil
		m := New(sdk.New(srv.URL, ""), &sdk.Session{ID: "s1", Agent: "build", PermissionMode: "ask"})
		if v != "" {
			m.ApplyCommand("/variant " + v)
		}
		m.Input = "hello " + v
		next, cmd := m.Update(key("", tea.KeyEnter))
		_ = next
		if cmd == nil {
			t.Fatalf("variant %q: expected prompt cmd", v)
		}
		if msg := cmd(); msg != nil {
			t.Fatalf("variant %q: prompt failed: %#v", v, msg)
		}
		if len(mu.bodies) != 1 {
			t.Fatalf("variant %q: %d prompt bodies", v, len(mu.bodies))
		}
		got, _ := mu.bodies[0]["effort"].(string) // auto omits the key (omitempty)
		if got != v {
			t.Fatalf("variant %q: prompt effort=%v", v, mu.bodies[0]["effort"])
		}
	}
}

func TestVariantPickerStartsOnCurrent(t *testing.T) {
	// Options must track the canonical list, not a TUI-local copy.
	m := testModel()
	m.ApplyCommand("/variant")
	if len(m.VariantOptions) != len(llm.EffortVariants) {
		t.Fatalf("options=%v want %v", m.VariantOptions, llm.EffortVariants)
	}
	for i, v := range m.VariantOptions {
		if v != llm.EffortVariants[i] {
			t.Fatalf("options=%v want %v", m.VariantOptions, llm.EffortVariants)
		}
	}
	// Cursor opens on the live variant, not a hard-coded top entry.
	for idx, v := range llm.EffortVariants {
		m := testModel()
		if v != "auto" {
			m.ApplyCommand("/variant " + v)
		}
		m.ApplyCommand("/variant")
		if !m.PickingVariant {
			t.Fatalf("variant %q: picker did not open", v)
		}
		if m.PickIdx != idx {
			t.Fatalf("variant %q: cursor=%d want %d", v, m.PickIdx, idx)
		}
		// Enter on the pre-selected row keeps the variant.
		want := llm.NormalizeEffort(v)
		next, _ := m.Update(key("", tea.KeyEnter))
		m = next.(Model)
		if m.Effort != want || m.PickingVariant {
			t.Fatalf("variant %q: after enter effort=%q picking=%v", v, m.Effort, m.PickingVariant)
		}
	}
}
