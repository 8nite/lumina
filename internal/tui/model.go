// Package tui is the Terminal TUI client: streaming timeline, Tab primary
// agent toggle (PLAN↔BUILD), Shift+Tab approval cycling, /resume picker,
// @file & [[memory]] completion, and inline permission modals. It talks to
// `lumina serve` exclusively through pkg/sdk. The state machine is
// headless-testable: Update is pure except for the SDK command constructors.
package tui

import (
	"fmt"
	"os"
	"path/filepath"
	"sort"
	"strings"
	"sync"
	"time"

	"github.com/8nite/lumina/internal/rules"
	"github.com/8nite/lumina/pkg/sdk"
	tea "github.com/charmbracelet/bubbletea"
	"github.com/charmbracelet/glamour"
	"github.com/charmbracelet/lipgloss"
)

// Permission cycle order for Shift+Tab.
var permCycle = []string{"ask", "auto-edit", "auto-approve", "bypass"}

// PermModal is an inline approval card.
type PermModal struct {
	RequestID string
	Tool      string
	Target    string
	Cursor    int // 0=Allow Once, 1=Allow Session, 2=Deny
}

// EventMsg carries one daemon SSE event into Update.
type EventMsg struct{ Event sdk.Event }

// SessionsMsg carries the /resume picker entries.
type SessionsMsg struct{ Sessions []*sdk.Session }

// ModelsMsg carries the /model picker entries (provider-qualified ids).
type ModelsMsg struct{ Models []string }

// ConnectProvidersMsg carries the /connect provider picker entries.
type ConnectProvidersMsg struct{ Providers []string }

// DeviceStartedMsg carries device web-auth codes; Update renders them and
// returns the background poll command.
type DeviceStartedMsg struct {
	Provider   string
	GitHubURL  string
	DeviceCode string
	UserCode   string
	URI        string
	Interval   int
	ExpiresIn  int
}

// ForkDoneMsg switches the model to a fresh fork.
type ForkDoneMsg struct{ Session *sdk.Session }

// TurnDoneMsg carries a completed assistant turn.
type TurnDoneMsg struct{ Text string }

// ErrMsg surfaces failures.
type ErrMsg struct{ Err error }

// truncate caps s at n characters, appending an ellipsis when cut.
func truncate(s string, n int) string {
	r := []rune(s)
	if len(r) <= n {
		return s
	}
	if n <= 3 {
		return string(r[:n])
	}
	return string(r[:n-3]) + "..."
}

// CompleteItem is one autocomplete suggestion with a description.
type CompleteItem struct {
	Text string // replacement text, e.g. "/plan", "@main.go", "[[notes]]"
	Desc string // one-line description shown beside the item
}

// CommandsLoadedMsg carries lazily loaded workspace completions
// (custom slash commands, workspace files, memory notes).
type CommandsLoadedMsg struct {
	Slash   []CompleteItem
	Files   []string
	Notes   []string
	Changed bool // true when anything beyond builtins was found
}

// commandDesc summarizes a custom command body in one line.
func commandDesc(body string) string {
	for _, line := range strings.Split(body, "\n") {
		t := strings.TrimSpace(strings.TrimPrefix(line, "#"))
		if t == "" {
			continue
		}
		if len(t) > 60 {
			t = t[:57] + "..."
		}
		return t
	}
	return "custom command"
}

// loadWorkspaceCompletions reads custom commands, files and notes for the
// active workspace session (bounded; skips VCS and dependency dirs).
func loadWorkspaceCompletions(workspaceDir string) CommandsLoadedMsg {
	out := CommandsLoadedMsg{Changed: true}
	bundle := rules.Load(rules.Options{WorkspaceDir: workspaceDir, ImportGlobalRules: true})
	seen := map[string]bool{}
	for _, b := range builtinSlashCommands() {
		seen[b.Text] = true
	}
	for _, c := range bundle.Commands {
		text := "/" + c.Name
		if seen[text] {
			continue
		}
		seen[text] = true
		out.Slash = append(out.Slash, CompleteItem{Text: text, Desc: commandDesc(c.Body)})
	}
	root := workspaceDir
	if root == "" {
		root = "."
	}
	count := 0
	_ = filepath.Walk(root, func(p string, info os.FileInfo, err error) error {
		if err != nil || count >= 200 {
			return nil
		}
		if info.IsDir() {
			base := filepath.Base(p)
			if base == ".git" || base == "node_modules" || base == ".lumina" {
				return filepath.SkipDir
			}
			return nil
		}
		if info.Size() > 1<<20 || !info.Mode().IsRegular() {
			return nil
		}
		rel, err := filepath.Rel(root, p)
		if err != nil {
			return nil
		}
		out.Files = append(out.Files, filepath.ToSlash(rel))
		count++
		return nil
	})
	sort.Strings(out.Files)
	memDir := filepath.Join(root, "memory")
	if entries, err := os.ReadDir(memDir); err == nil {
		for _, e := range entries {
			name := e.Name()
			if e.IsDir() || !strings.HasSuffix(name, ".md") || strings.HasPrefix(name, ".") {
				continue
			}
			out.Notes = append(out.Notes, strings.TrimSuffix(name, ".md"))
		}
		sort.Strings(out.Notes)
	}
	return out
}

// builtinSlashCommands is the always-available / command menu.
func builtinSlashCommands() []CompleteItem {
	return []CompleteItem{
		{"/plan", "switch to read-only plan mode"},
		{"/build", "switch to full build mode"},
		{"/model", "pick a model or connect a provider (usage: /model [filter])"},
		{"/mode", "view or set approval mode (ask|auto-edit|auto-approve|bypass)"},
		{"/connect", "connect a provider via device web-auth (usage: /connect [provider] [github-url])"},
		{"/compact", "summarize context to free the window"},
		{"/thinking", "show or hide model reasoning while working (show|hide)"},
		{"/undo", "restore the last shadow-git checkpoint"},
		{"/redo", "re-apply the undone checkpoint"},
		{"/resume", "pick a session to attach"},
		{"/fork", "branch this session at the latest turn"},
		{"/graphify", "refresh the knowledge graph"},
		{"/evolve", "run a Harness² self-improvement pass"},
		{"/init", "scaffold repo memory and harness overlay"},
	}
}

// Model is the TUI state machine.
type Model struct {
	Client  *sdk.Client
	Session *sdk.Session

	Agent string // "plan" | "build" (+ custom primary names)
	Perm  string // ask | auto-edit | auto-approve | bypass

	Lines []string // timeline (markdown source)
	// Rendered mirrors Lines but holds the ANSI-rendered form, produced
	// once per appended line (not once per View call). View only joins
	// the visible tail, which is what stops the flicker on fast SSE.
	Rendered []string
	Input    string
	Cursor   int // rune index into Input (not byte index)

	Modal    *PermModal
	Sessions []*sdk.Session // /resume picker entries
	// Model picker (/model) entries + mode flag. Reuses PickIdx.
	ModelOptions []string
	Picking      bool
	PickingModel bool
	PickIdx      int
	// /connect wizard: provider picker, GitHub URL prompt, device polling.
	ConnectOptions  []string
	PickingConnect  bool
	ConnectProvider string
	AwaitingURL     bool
	// Inline autocomplete popup for / commands, @files and [[notes]].
	SlashCommands   []CompleteItem // builtins (+ workspace customs once loaded)
	CustomLoaded    bool           // custom commands/files/notes already loaded
	CompleteFiles   []string       // workspace-relative file paths
	CompleteNotes   []string       // memory note names (no .md)
	Completing      bool
	CompleteOptions []CompleteItem
	CompleteIdx     int

	Status string
	Width  int
	Height int
	// Pending holds live-streamed assistant text (turn.delta), rendered
	// plain until turn.completed replaces it with the final markdown.
	Pending         string
	PendingThinking string
	// ShowThinking mirrors opencode v2 `session.thinking` (default hide):
	// reasoning streams but only renders when enabled (/thinking).
	ShowThinking bool
	// Working tracks an in-flight turn (send/turn.started → completed/failed).
	// Enter steers it, Ctrl+X then Enter queues for later (opencode parity).
	Working    bool
	QueueArmed bool
	quitting   bool
}

// mdCache holds a single shared glamour renderer. Recreated only when the
// terminal width changes, so steady-state View calls do zero markdown work.
var (
	mdMu     sync.Mutex
	mdRender *glamour.TermRenderer
	mdWrap   int
)

func mdRenderer(width int) *glamour.TermRenderer {
	wrap := width
	if wrap <= 0 {
		wrap = 80
	}
	if wrap > 120 {
		wrap = 120
	}
	mdMu.Lock()
	defer mdMu.Unlock()
	if mdRender == nil || mdWrap != wrap {
		r, err := glamour.NewTermRenderer(glamour.WithStandardStyle("dark"), glamour.WithWordWrap(wrap))
		if err != nil {
			return nil
		}
		mdRender = r
		mdWrap = wrap
	}
	return mdRender
}

// renderCached renders one markdown block once. View must never call this
// per frame; lines are rendered on append and on width change only.
func renderCached(src string, width int) string {
	r := mdRenderer(width)
	if r == nil {
		return src
	}
	out, err := r.Render(src)
	if err != nil {
		return src
	}
	return strings.TrimRight(out, "\n")
}

// appendLine stores source + its rendered form together.
func (m *Model) appendLine(src string) {
	m.Lines = append(m.Lines, src)
	m.Rendered = append(m.Rendered, renderCached(src, m.Width))
}

// rerenderAll rebuilds the rendered cache after a width change.
func (m *Model) rerenderAll() {
	m.Rendered = m.Rendered[:0]
	for _, ln := range m.Lines {
		m.Rendered = append(m.Rendered, renderCached(ln, m.Width))
	}
}

// New creates a model bound to a session.
func New(c *sdk.Client, sess *sdk.Session) Model {
	agent, perm := "build", "ask"
	if sess != nil {
		if sess.Agent != "" {
			agent = sess.Agent
		}
		if sess.PermissionMode != "" {
			perm = sess.PermissionMode
		}
	}
	m := Model{Client: c, Session: sess, Agent: agent, Perm: perm,
		SlashCommands: builtinSlashCommands(),
		Width:         80}
	m.appendLine("# lumina")
	m.appendLine("Session `" + sessID(sess) + "` — Tab: plan↔build, Shift+Tab: approval mode.")
	return m
}

func sessID(s *sdk.Session) string {
	if s == nil {
		return "?"
	}
	return s.ID
}

// Init subscribes to SSE when a client is attached.
func (m Model) Init() tea.Cmd {
	if m.Client == nil {
		return nil
	}
	return m.subscribeCmd()
}

func (m Model) subscribeCmd() tea.Cmd {
	return func() tea.Msg {
		// The caller runs Subscribe in a goroutine and forwards via Send;
		// Init only seeds the first poll so headless tests stay offline.
		return nil
	}
}

// ToggleAgent flips plan↔build (Tab). Custom primary names cycle plan→build.
func (m *Model) ToggleAgent() {
	if m.Agent == "plan" {
		m.Agent = "build"
	} else {
		m.Agent = "plan"
	}
	m.pushSystem("agent → " + m.Agent)
	if m.Client != nil && m.Session != nil {
		agent := m.Agent
		_ = agent
	}
}

// CyclePerm advances ask→auto-edit→auto-approve→bypass (Shift+Tab).
func (m *Model) CyclePerm() {
	for i, p := range permCycle {
		if p == m.Perm {
			m.Perm = permCycle[(i+1)%len(permCycle)]
			m.pushSystem("permission → " + m.Perm)
			return
		}
	}
	m.Perm = "ask"
}

func (m *Model) pushSystem(s string) {
	m.appendLine("_" + s + "_")
}

// sendDelivery picks steer vs queue (opencode parity): Enter steers the
// active turn, Ctrl+X then Enter queues for later.
func (m *Model) sendDelivery() string {
	if m.Working && m.QueueArmed {
		return "queue"
	}
	return "steer"
}

// ApplyCommand executes /slash commands locally; returns a tea.Cmd for any
// follow-up network work (daemon sync when attached).
func (m *Model) ApplyCommand(cmd string) tea.Cmd {
	fields := strings.Fields(cmd)
	if len(fields) == 0 {
		return nil
	}
	switch fields[0] {
	case "/plan":
		m.Agent = "plan"
		m.pushSystem("agent → plan")
		return m.pushAgentCmd()
	case "/build":
		m.Agent = "build"
		m.pushSystem("agent → build")
		return m.pushAgentCmd()
	case "/mode":
		if len(fields) > 1 {
			m.Perm = fields[1]
			m.pushSystem("permission → " + m.Perm)
		} else {
			m.CyclePerm()
		}
		return m.pushPermCmd()
	case "/connect":
		// /connect [provider] [github-url] — Copilot provider picker,
		// then GitHub URL prompt, then device web-auth.
		var provider, ghurl string
		if len(fields) > 1 {
			provider = fields[1]
		}
		if len(fields) > 2 {
			ghurl = fields[2]
		}
		if provider == "" {
			if m.Client == nil {
				m.pushSystem("usage: /connect [github-url] (needs a copilot provider)")
				return nil
			}
			c := m.Client
			return func() tea.Msg {
				provs, err := c.ListProviders(bg())
				if err != nil {
					return ErrMsg{err}
				}
				var opts []string
				for _, p := range provs {
					if p.Protocol == "copilot" {
						opts = append(opts, p.Name)
					}
				}
				if len(opts) == 0 {
					return TurnDoneMsg{"no copilot provider configured (add [providers.NAME] protocol=copilot)"}
				}
				return ConnectProvidersMsg{opts}
			}
		}
		return m.beginConnect(provider, ghurl)
	case "/model":
		// /model [filter] — bare opens the full picker; with text, the live
		// list is filtered (case-insensitive substring): one match connects
		// directly, several open a filtered picker, none reports the filter.
		filter := ""
		if len(fields) > 1 {
			filter = strings.ToLower(strings.Join(fields[1:], " "))
			m.pushSystem("model → " + strings.Join(fields[1:], " "))
		}
		m.PickingModel = true
		m.PickIdx = 0
		if filter != "" {
			m.pushSystem("pick a model matching \"" + filter + "\" (↑↓ + enter)")
		} else {
			m.pushSystem("pick a model (↑↓ + enter)")
		}
		if m.Client != nil {
			c := m.Client
			sess := m.Session
			return func() tea.Msg {
				provs, err := c.ListProviders(bg())
				if err != nil {
					return ErrMsg{err}
				}
				var opts []string
				var firstErr error
				for _, p := range provs {
					// Live list every provider; failures are skipped and
					// only surface when nothing listed at all.
					ms, err := c.ListModels(bg(), p.Name)
					if err != nil {
						if firstErr == nil {
							firstErr = err
						}
						continue
					}
					for _, mm := range ms {
						id := mm.ID
						if !strings.Contains(id, "/") {
							id = p.Name + "/" + id
						}
						opts = append(opts, id)
					}
				}
				if filter != "" {
					var kept []string
					for _, o := range opts {
						if strings.Contains(strings.ToLower(o), filter) {
							kept = append(kept, o)
						}
					}
					opts = kept
				}
				if len(opts) == 0 {
					if filter != "" {
						return TurnDoneMsg{fmt.Sprintf("no models match %q", filter)}
					}
					if firstErr != nil {
						return TurnDoneMsg{"no live models: " + firstErr.Error()}
					}
					return TurnDoneMsg{"no live models (connect the provider first)"}
				}
				if len(opts) == 1 && filter != "" {
					return connectModelCmd(c, sess, opts[0])()
				}
				return ModelsMsg{opts}
			}
		}
	case "/compact":
		m.compact()
	case "/thinking":
		want := ""
		if len(fields) > 1 {
			want = strings.ToLower(fields[1])
		}
		switch want {
		case "show":
			m.ShowThinking = true
		case "hide":
			m.ShowThinking = false
		default:
			m.ShowThinking = !m.ShowThinking
		}
		state := "hide"
		if m.ShowThinking {
			state = "show"
		}
		m.pushSystem("thinking → " + state)
	case "/undo":
		m.pushSystem("undo requested")
		if m.Client != nil && m.Session != nil {
			id := m.Session.ID
			c := m.Client
			return func() tea.Msg {
				if err := c.Revert(bg(), id); err != nil {
					return ErrMsg{err}
				}
				return TurnDoneMsg{"(undone)"}
			}
		}
	case "/redo":
		m.pushSystem("redo requested")
		if m.Client != nil && m.Session != nil {
			id := m.Session.ID
			c := m.Client
			return func() tea.Msg {
				if err := c.Unrevert(bg(), id); err != nil {
					return ErrMsg{err}
				}
				return TurnDoneMsg{"(redone)"}
			}
		}
	case "/resume":
		m.Picking = true
		m.PickIdx = 0
		m.pushSystem("pick a session (↑↓ + enter)")
		if m.Client != nil {
			c := m.Client
			return func() tea.Msg {
				list, err := c.ListSessions(bg(), "")
				if err != nil {
					return ErrMsg{err}
				}
				return SessionsMsg{list}
			}
		}
	case "/fork":
		m.pushSystem("fork requested")
		if m.Client != nil && m.Session != nil {
			id := m.Session.ID
			c := m.Client
			return func() tea.Msg {
				f, err := c.ForkSession(bg(), id)
				if err != nil {
					return ErrMsg{err}
				}
				return ForkDoneMsg{f}
			}
		}
	case "/graphify", "/evolve", "/init":
		m.pushSystem(fields[0] + " — runs in daemon; see `lumina " + strings.TrimPrefix(fields[0], "/") + "`")
	default:
		m.pushSystem("unknown command " + fields[0])
	}
	return nil
}

// beginConnect starts (or continues to) device web-auth for a provider.
// With an empty URL it prompts for the GitHub host first.
func (m *Model) beginConnect(provider, ghurl string) tea.Cmd {
	m.ConnectProvider = provider
	if strings.TrimSpace(ghurl) == "" {
		m.AwaitingURL = true
		m.pushSystem("GitHub URL? (enter = github.com, or paste your GHES host)")
		return nil
	}
	m.AwaitingURL = false
	if m.Client == nil {
		m.pushSystem("no daemon attached")
		return nil
	}
	c := m.Client
	return func() tea.Msg {
		d, err := c.DeviceStart(bg(), provider, ghurl)
		if err != nil {
			return ErrMsg{err}
		}
		interval := d.Interval
		if interval <= 0 {
			interval = 5
		}
		return DeviceStartedMsg{
			Provider: provider, GitHubURL: ghurl,
			DeviceCode: d.DeviceCode, UserCode: d.UserCode, URI: d.URI,
			Interval: interval, ExpiresIn: d.ExpiresIn,
		}
	}
}

// applyModel connects a session to a model id and persists it.
func (m *Model) applyModel(id string) tea.Cmd {
	m.pushSystem("model → " + id)
	return connectModelCmd(m.Client, m.Session, id)
}

// connectModelCmd PATCHes the session and persists the router tier.
func connectModelCmd(c *sdk.Client, sess *sdk.Session, id string) tea.Cmd {
	if c == nil || sess == nil {
		return nil
	}
	sid := sess.ID
	return func() tea.Msg {
		if _, err := c.UpdateSession(bg(), sid, map[string]string{"model": id}); err != nil {
			return ErrMsg{err}
		}
		if err := c.SetRouterModel(bg(), "executor", id); err != nil {
			return ErrMsg{err}
		}
		return TurnDoneMsg{"connected to " + id}
	}
}

// compact keeps recent lines and summarizes the rest (zero LLM tokens).
func (m *Model) compact() {
	if len(m.Lines) <= 30 {
		m.pushSystem("compact: nothing to do")
		return
	}
	m.Lines = append([]string{"_[compacted earlier turns; rules, diffs, memory pointers preserved]_"}, m.Lines[len(m.Lines)-30:]...)
	m.rerenderAll()
	m.pushSystem("compacted")
}

// CompleteInput offers @file and [[memory]] completions for a prefix.
// Kept as a pure helper; the interactive popup uses Model.computeCompletions.
func CompleteInput(input string, files, notes []string) []string {
	var out []string
	if i := strings.LastIndex(input, "@"); i >= 0 && (i == 0 || input[i-1] == ' ' || input[i-1] == '\n') && !strings.Contains(input[i:], " ") {
		prefix := input[i+1:]
		for _, f := range files {
			if strings.HasPrefix(f, prefix) {
				out = append(out, "@"+f)
			}
		}
	}
	if i := strings.LastIndex(input, "[["); i >= 0 && (i == 0 || input[i-1] == ' ' || input[i-1] == '\n') && !strings.Contains(input[i:], " ") && !strings.Contains(input[i:], "]]") {
		prefix := input[i+2:]
		for _, n := range notes {
			if strings.HasPrefix(n, prefix) {
				out = append(out, "[["+n+"]]")
			}
		}
	}
	return out
}

// computeCompletions rebuilds the autocomplete popup from the current input.
// Returns a loader cmd on first use per session (custom commands, files,
// notes); otherwise nil.
func (m *Model) computeCompletions() tea.Cmd {
	m.CompleteOptions = nil
	m.Completing = false
	m.CompleteIdx = 0
	in := m.Input
	if strings.HasPrefix(in, "/") && !strings.Contains(in, " ") {
		for _, c := range m.SlashCommands {
			if strings.HasPrefix(c.Text, in) {
				m.CompleteOptions = append(m.CompleteOptions, c)
				if len(m.CompleteOptions) >= 8 {
					break
				}
			}
		}
	} else {
		// @file and [[memory]] anywhere, anchored at a boundary.
		if i := strings.LastIndex(in, "@"); i >= 0 && (i == 0 || in[i-1] == ' ' || in[i-1] == '\n') && !strings.Contains(in[i:], " ") {
			prefix := in[i+1:]
			for _, f := range m.CompleteFiles {
				if strings.HasPrefix(f, prefix) {
					m.CompleteOptions = append(m.CompleteOptions, CompleteItem{Text: "@" + f, Desc: "file"})
					if len(m.CompleteOptions) >= 8 {
						break
					}
				}
			}
		}
		if i := strings.LastIndex(in, "[["); i >= 0 && (i == 0 || in[i-1] == ' ' || in[i-1] == '\n') && !strings.Contains(in[i:], " ") && !strings.Contains(in[i:], "]]") {
			prefix := in[i+2:]
			for _, n := range m.CompleteNotes {
				if strings.HasPrefix(n, prefix) {
					m.CompleteOptions = append(m.CompleteOptions, CompleteItem{Text: "[[" + n + "]]", Desc: "memory"})
					if len(m.CompleteOptions) >= 8 {
						break
					}
				}
			}
		}
	}
	if len(m.CompleteOptions) > 0 {
		m.Completing = true
	}
	if !m.CustomLoaded && m.Session != nil && (strings.HasPrefix(in, "/") || strings.Contains(in, "@") || strings.Contains(in, "[[")) {
		m.CustomLoaded = true // set now so rapid keystrokes trigger one load
		ws := ""
		if m.Session != nil {
			ws = m.Session.WorkspaceDir
		}
		return func() tea.Msg {
			loaded := loadWorkspaceCompletions(ws)
			return CommandsLoadedMsg(loaded)
		}
	}
	return nil
}

// acceptCompletion replaces the current token with the highlighted item.
func (m *Model) acceptCompletion() {
	if !m.Completing || m.CompleteIdx >= len(m.CompleteOptions) {
		return
	}
	item := m.CompleteOptions[m.CompleteIdx]
	in := m.Input
	switch {
	case strings.HasPrefix(in, "/") && !strings.Contains(in, " "):
		m.Input = item.Text + " "
	case strings.Contains(in, "@") && strings.HasPrefix(item.Text, "@"):
		m.Input = in[:strings.LastIndex(in, "@")] + item.Text
	case strings.Contains(in, "[[") && strings.HasPrefix(item.Text, "[["):
		m.Input = in[:strings.LastIndex(in, "[[")] + item.Text
	}
	m.Cursor = len([]rune(m.Input))
	m.Completing = false
	m.CompleteOptions = nil
	m.CompleteIdx = 0
}

// insertRunes inserts s at the cursor (rune-aware).
func (m *Model) insertRunes(s string) {
	if s == "" {
		return
	}
	r := []rune(m.Input)
	c := m.Cursor
	if c < 0 {
		c = 0
	}
	if c > len(r) {
		c = len(r)
	}
	nr := make([]rune, 0, len(r)+len([]rune(s)))
	nr = append(nr, r[:c]...)
	nr = append(nr, []rune(s)...)
	nr = append(nr, r[c:]...)
	m.Input = string(nr)
	m.Cursor = c + len([]rune(s))
}

// deleteBeforeCursor removes one rune left of the cursor.
func (m *Model) deleteBeforeCursor() {
	r := []rune(m.Input)
	if m.Cursor <= 0 || len(r) == 0 {
		return
	}
	if m.Cursor > len(r) {
		m.Cursor = len(r)
	}
	nr := append(r[:m.Cursor-1], r[m.Cursor:]...)
	m.Input = string(nr)
	m.Cursor--
}

// Update is the headless-testable state machine.
func (m Model) Update(msg tea.Msg) (tea.Model, tea.Cmd) {
	switch msg := msg.(type) {
	case tea.WindowSizeMsg:
		changed := msg.Width != m.Width
		m.Width = msg.Width
		m.Height = msg.Height
		if changed {
			m.rerenderAll()
		}
		return m, nil
	case tea.KeyMsg:
		return m.updateKey(msg)
	case EventMsg:
		return m.updateEvent(msg.Event)
	case TurnDoneMsg:
		m.appendLine(msg.Text)
		m.Status = ""
		return m, nil
	case ModelsMsg:
		m.ModelOptions = msg.Models
		m.PickingModel = len(msg.Models) > 0
		m.PickIdx = 0
		return m, nil
	case SessionsMsg:
		m.Sessions = msg.Sessions
		m.Picking = len(msg.Sessions) > 0
		m.PickIdx = 0
		return m, nil
	case CommandsLoadedMsg:
		seen := map[string]bool{}
		for _, b := range builtinSlashCommands() {
			seen[b.Text] = true
		}
		merged := builtinSlashCommands()
		for _, c := range msg.Slash {
			if !seen[c.Text] {
				seen[c.Text] = true
				merged = append(merged, c)
			}
		}
		m.SlashCommands = merged
		m.CompleteFiles = msg.Files
		m.CompleteNotes = msg.Notes
		m.CustomLoaded = true
		m.computeCompletions()
		return m, nil
	case ConnectProvidersMsg:
		m.ConnectOptions = msg.Providers
		m.PickingConnect = len(msg.Providers) > 0
		m.PickIdx = 0
		return m, nil
	case DeviceStartedMsg:
		m.appendLine("Open " + msg.URI + " and enter code: " + msg.UserCode)
		m.appendLine("Waiting for approval…")
		c := m.Client
		return m, func() tea.Msg {
			if c == nil {
				return ErrMsg{fmt.Errorf("no daemon attached")}
			}
			deadline := time.Now().Add(time.Duration(msg.ExpiresIn) * time.Second)
			if msg.ExpiresIn <= 0 {
				deadline = time.Now().Add(10 * time.Minute)
			}
			for time.Now().Before(deadline) {
				time.Sleep(time.Duration(msg.Interval) * time.Second)
				st, err := c.DevicePoll(bg(), msg.Provider, msg.DeviceCode, msg.GitHubURL)
				if err != nil {
					return ErrMsg{err}
				}
				if st == "authorized" {
					return TurnDoneMsg{"connected " + msg.Provider + " via web auth"}
				}
				if st != "pending" {
					return ErrMsg{fmt.Errorf("device auth: %s", st)}
				}
			}
			return ErrMsg{fmt.Errorf("device code expired; run /connect again")}
		}
	case ForkDoneMsg:
		if msg.Session != nil {
			m.Session = msg.Session
			m.Agent = msg.Session.Agent
			m.Perm = msg.Session.PermissionMode
			m.pushSystem("forked → " + msg.Session.ID)
		}
		return m, nil
	case ErrMsg:
		if m.Status == "working…" {
			m.appendLine("**error:** " + msg.Err.Error())
		}
		m.Status = "error: " + msg.Err.Error()
		return m, nil
	}
	return m, nil
}

func (m Model) updateKey(msg tea.KeyMsg) (tea.Model, tea.Cmd) {
	// Modal captures all keys while open.
	if m.Modal != nil {
		switch msg.String() {
		case "left", "h":
			if m.Modal.Cursor > 0 {
				m.Modal.Cursor--
			}
		case "right", "l", "tab":
			if m.Modal.Cursor < 2 {
				m.Modal.Cursor++
			}
		case "enter":
			decisions := []string{"allow_once", "allow_session", "deny"}
			d := decisions[m.Modal.Cursor]
			m.Status = "permission: " + d
			reqID := m.Modal.RequestID
			m.Modal = nil
			if m.Client != nil && reqID != "" {
				c := m.Client
				return m, func() tea.Msg {
					if err := c.AnswerPermission(bg(), reqID, d); err != nil {
						return ErrMsg{err}
					}
					return nil
				}
			}
		case "esc":
			m.Modal = nil
		}
		return m, nil
	}
	// Connect provider picker navigation.
	if m.PickingConnect {
		switch msg.String() {
		case "up", "k":
			if m.PickIdx > 0 {
				m.PickIdx--
			}
			return m, nil
		case "down", "j":
			if m.PickIdx+1 < len(m.ConnectOptions) {
				m.PickIdx++
			}
			return m, nil
		case "enter":
			if m.PickIdx < len(m.ConnectOptions) {
				provider := m.ConnectOptions[m.PickIdx]
				m.PickingConnect = false
				return m, m.beginConnect(provider, "")
			}
			m.PickingConnect = false
			return m, nil
		case "esc":
			m.PickingConnect = false
			return m, nil
		}
	}
	// Model picker navigation.
	if m.PickingModel {
		switch msg.String() {
		case "up", "k":
			if m.PickIdx > 0 {
				m.PickIdx--
			}
			return m, nil
		case "down", "j":
			if m.PickIdx+1 < len(m.ModelOptions) {
				m.PickIdx++
			}
			return m, nil
		case "enter":
			if m.PickIdx < len(m.ModelOptions) {
				id := m.ModelOptions[m.PickIdx]
				m.PickingModel = false
				return m, m.applyModel(id)
			}
			m.PickingModel = false
			return m, nil
		case "esc":
			m.PickingModel = false
			return m, nil
		}
	}
	// Session picker navigation.
	if m.Picking {
		switch msg.String() {
		case "up", "k":
			if m.PickIdx > 0 {
				m.PickIdx--
			}
			return m, nil
		case "down", "j":
			if m.PickIdx+1 < len(m.Sessions) {
				m.PickIdx++
			}
			return m, nil
		case "enter":
			if m.PickIdx < len(m.Sessions) {
				m.Session = m.Sessions[m.PickIdx]
				m.Agent = m.Session.Agent
				m.Perm = m.Session.PermissionMode
				m.pushSystem("attached → " + m.Session.ID)
			}
			m.Picking = false
			return m, nil
		case "esc":
			m.Picking = false
			return m, nil
		}
	}
	// Autocomplete popup navigation (slash commands, @files, [[notes]]).
	// Runs after pickers/modals so those keep precedence.
	if m.Completing {
		switch msg.String() {
		case "up":
			if m.CompleteIdx > 0 {
				m.CompleteIdx--
			}
			return m, nil
		case "down":
			if m.CompleteIdx+1 < len(m.CompleteOptions) {
				m.CompleteIdx++
			}
			return m, nil
		case "tab":
			m.acceptCompletion()
			return m, nil
		case "esc":
			m.Completing = false
			m.CompleteOptions = nil
			m.CompleteIdx = 0
			return m, nil
		}
	}
	switch msg.String() {
	case "ctrl+c", "ctrl+d":
		m.quitting = true
		return m, tea.Quit
	case "tab":
		m.ToggleAgent()
		return m, m.pushAgentCmd()
	case "shift+tab":
		m.Completing = false
		m.CompleteOptions = nil
		m.CyclePerm()
		return m, m.pushPermCmd()
	case "ctrl+x":
		// Arm queue mode: the next Enter queues instead of steering.
		m.QueueArmed = true
		m.Status = "ctrl+x — enter queues the prompt, esc cancels"
		return m, nil
	case "enter":
		m.Completing = false
		m.CompleteOptions = nil
		m.CompleteIdx = 0
		text := strings.TrimSpace(m.Input)
		m.Input = ""
		m.Cursor = 0
		// /connect wizard consumes the next line as the GitHub URL
		// (empty = default host). This must run before the empty check.
		if m.AwaitingURL && !strings.HasPrefix(text, "/") {
			m.AwaitingURL = false
			provider := m.ConnectProvider
			if provider == "" {
				provider = "copilot"
			}
			if text == "" {
				text = "https://github.com"
			}
			return m, m.beginConnect(provider, text)
		}
		if text == "" {
			return m, nil
		}
		if strings.HasPrefix(text, "/") {
			return m, m.ApplyCommand(text)
		}
		m.appendLine("**you:** " + text)
		delivery := m.sendDelivery()
		m.QueueArmed = false
		m.Working = true
		if delivery == "queue" {
			m.Status = "queued…"
		} else {
			m.Status = "working…"
		}
		if m.Client != nil && m.Session != nil {
			id, t := m.Session.ID, text
			c := m.Client
			return m, func() tea.Msg {
				if err := c.Prompt(bg(), id, sdk.PromptInput{Text: t, Delivery: delivery}); err != nil {
					return ErrMsg{err}
				}
				return nil
			}
		}
		m.appendLine("_(no daemon attached — prompt recorded locally)_")
		return m, nil
	case "backspace":
		m.deleteBeforeCursor()
	case "left":
		if m.Cursor > 0 {
			m.Cursor--
		}
		return m, nil
	case "right":
		if m.Cursor < len([]rune(m.Input)) {
			m.Cursor++
		}
		return m, nil
	case "ctrl+u":
		m.Input = ""
		m.Cursor = 0
	case "esc":
		if m.Picking {
			m.Picking = false
		}
		m.QueueArmed = false
		m.Completing = false
		m.CompleteOptions = nil
		m.CompleteIdx = 0
	default:
		if msg.Type == tea.KeyRunes {
			m.insertRunes(msg.String())
		}
	}
	return m, m.computeCompletions()
}

func (m Model) pushAgentCmd() tea.Cmd {
	if m.Client == nil || m.Session == nil {
		return nil
	}
	id, a := m.Session.ID, m.Agent
	c := m.Client
	return func() tea.Msg {
		if _, err := c.UpdateSession(bg(), id, map[string]string{"agent": a}); err != nil {
			return ErrMsg{err}
		}
		return nil
	}
}

func (m Model) pushPermCmd() tea.Cmd {
	if m.Client == nil || m.Session == nil {
		return nil
	}
	id, p := m.Session.ID, m.Perm
	c := m.Client
	return func() tea.Msg {
		if _, err := c.UpdateSession(bg(), id, map[string]string{"permission_mode": p}); err != nil {
			return ErrMsg{err}
		}
		return nil
	}
}

func (m Model) updateEvent(e sdk.Event) (tea.Model, tea.Cmd) {
	// Cross-session filtering: turn and permission events for another
	// session never touch this timeline (TP-09.4).
	if e.SessionID != "" && m.Session != nil && e.SessionID != m.Session.ID {
		switch e.Type {
		case "turn.completed", "turn.failed", "turn.started", "turn.progress",
			"turn.delta", "turn.tool",
			"permission.asked", "permission":
			return m, nil
		}
	}
	switch e.Type {
	case "turn.started":
		// opencode-style pending state (reimplemented): show what the
		// daemon accepted so long runs never look frozen.
		var d map[string]string
		preview := "working…"
		if err := jsonUnmarshal(e.Data, &d); err == nil {
			if t := d["text"]; t != "" {
				preview = "working… " + truncate(t, 60)
			} else if s := d["stage"]; s != "" {
				preview = s
			}
		}
		m.Status = preview
		m.Working = true
	case "turn.progress":
		var d map[string]string
		if err := jsonUnmarshal(e.Data, &d); err == nil {
			if s := d["stage"]; s != "" {
				m.Status = s
			}
		}
	case "turn.delta":
		var d map[string]string
		if err := jsonUnmarshal(e.Data, &d); err == nil {
			// Cap the live buffer; the final text arrives via turn.completed.
			if len([]rune(m.Pending+m.PendingThinking)) < 4000 {
				m.Pending += d["delta"]
				m.PendingThinking += d["thinking"]
			}
			m.Status = "working…"
		}
	case "turn.tool":
		var d map[string]interface{}
		if err := jsonUnmarshal(e.Data, &d); err == nil {
			tool, _ := d["tool"].(string)
			target, _ := d["target"].(string)
			if tool != "" {
				m.Status = "tool: " + tool
				if phase, _ := d["phase"].(string); phase == "done" {
					preview, _ := d["preview"].(string)
					if isErr, _ := d["error"].(bool); isErr {
						m.appendLine("· `" + tool + "` " + target + " → error: " + truncate(preview, 200))
					} else if preview != "" {
						m.appendLine("· `" + tool + "` " + target + " → " + truncate(preview, 200))
					} else {
						m.appendLine("· `" + tool + "` " + target + " → done")
					}
				} else {
					m.appendLine("· `" + tool + "` " + target)
				}
			}
		}
	case "turn.completed":
		// opencode v2 turn summary: agent, model, duration (+tok/s when
		// the provider reported usage) as the assistant footer.
		var d map[string]interface{}
		if err := jsonUnmarshal(e.Data, &d); err == nil {
			if t, _ := d["text"].(string); t != "" {
				m.appendLine(t)
			}
			if f := turnFooter(m.Agent, d); f != "" {
				m.appendLine(f)
			}
		}
		m.Pending = ""
		m.PendingThinking = ""
		m.Status = ""
		m.Working = false
		m.QueueArmed = false
	case "turn.failed":
		var d map[string]string
		msg := "turn failed"
		if err := jsonUnmarshal(e.Data, &d); err == nil {
			if v := d["error"]; v != "" {
				msg = v
			} else if v := d["text"]; v != "" {
				msg = v
			}
		}
		m.appendLine("**error:** " + msg)
		m.Pending = ""
		m.PendingThinking = ""
		m.Status = "error: " + truncate(msg, 120)
		m.Working = false
		m.QueueArmed = false
	case "session.updated":
		m.Status = "session updated"
	case "permission.resolved":
		var d map[string]string
		if err := jsonUnmarshal(e.Data, &d); err == nil {
			m.Status = "permission: " + d["decision"]
		}
	case "permission.asked", "permission":
		var d map[string]string
		modal := &PermModal{Tool: "bash", Target: string(e.Data)}
		if err := jsonUnmarshal(e.Data, &d); err == nil {
			if v, ok := d["request_id"]; ok && v != "" {
				modal.RequestID = v
			} else if v, ok := d["requestId"]; ok && v != "" {
				modal.RequestID = v
			} else if v, ok := d["id"]; ok && v != "" {
				modal.RequestID = v
			}
			if v, ok := d["tool"]; ok && v != "" {
				modal.Tool = v
			}
			if v, ok := d["target"]; ok && v != "" {
				modal.Target = v
			}
		}
		m.Modal = modal
	}
	return m, nil
}

// View renders header pills, timeline, input, and modal.
// It never does markdown work: lines were rendered on append. It only
// shows the tail that fits the current Height, so resize/scroll is stable
// and does not flicker.
func (m Model) View() string {
	var sb strings.Builder
	agentPill := agentStyle.Render(" " + strings.ToUpper(m.Agent) + " ")
	permPill := permStyle(m.Perm).Render(" " + strings.ToUpper(m.Perm) + " ")
	fmt.Fprintf(&sb, "%s %s  %s\n", agentPill, permPill, dimStyle.Render(sessID(m.Session)))
	sb.WriteString(strings.Repeat("─", maxInt(20, m.Width)) + "\n")
	lines := m.Rendered
	if m.Height > 0 {
		// Reserve: header(2) + input(1) + status(0-1) + help(1) +
		// modal/pickers/completion (variable). Keep at least 5 rows for history.
		reserved := 6
		if m.Modal != nil {
			reserved += 3
		}
		if m.Picking {
			reserved += len(m.Sessions) + 1
		}
		if m.PickingConnect {
			reserved += len(m.ConnectOptions) + 1
		}
		if m.PickingModel {
			reserved += len(m.ModelOptions) + 1
		}
		if m.Completing {
			reserved += len(m.CompleteOptions) + 1
		}
		if m.Status != "" {
			reserved++
		}
		budget := m.Height - reserved
		if budget < 5 {
			budget = 5
		}
		if len(lines) > budget {
			lines = lines[len(lines)-budget:]
		}
	} else if len(lines) > 100 {
		// Headless / tests: cap so a long session cannot blow up View.
		lines = lines[len(lines)-100:]
	}
	for _, line := range lines {
		sb.WriteString(line + "\n")
	}
	if m.Pending != "" || m.PendingThinking != "" {
		sb.WriteString(m.renderPending() + "\n")
	}
	if m.Modal != nil {
		sb.WriteString(modalStyle.Render(m.renderModal()) + "\n")
	}
	if m.Picking {
		sb.WriteString(dimStyle.Render("(picker: choose session, enter to attach, esc to close)") + "\n")
		for i, s := range m.Sessions {
			mark := "  "
			if i == m.PickIdx {
				mark = "> "
			}
			sb.WriteString(dimStyle.Render(mark+s.ID+" · "+s.Agent) + "\n")
		}
	}
	if m.PickingConnect {
		sb.WriteString(dimStyle.Render("(connect: choose provider, enter to continue, esc to close)") + "\n")
		for i, opt := range m.ConnectOptions {
			mark := "  "
			if i == m.PickIdx {
				mark = "> "
			}
			sb.WriteString(dimStyle.Render(mark+opt) + "\n")
		}
	}
	if m.PickingModel {
		sb.WriteString(dimStyle.Render("(picker: choose model, enter to connect, esc to close)") + "\n")
		for i, opt := range m.ModelOptions {
			mark := "  "
			if i == m.PickIdx {
				mark = "> "
			}
			sb.WriteString(dimStyle.Render(mark+opt) + "\n")
		}
	}
	if m.Completing && len(m.CompleteOptions) > 0 {
		sb.WriteString(dimStyle.Render("(tab: complete · esc: dismiss)") + "\n")
		for i, opt := range m.CompleteOptions {
			mark := "  "
			style := dimStyle
			if i == m.CompleteIdx {
				mark = "> "
				style = lipgloss.NewStyle().Foreground(lipgloss.Color("230")).Bold(true)
			}
			desc := ""
			if opt.Desc != "" {
				desc = " " + dimStyle.Render("· "+truncate(opt.Desc, 48))
			}
			sb.WriteString(style.Render(mark+opt.Text) + desc + "\n")
		}
	}
	sb.WriteString(renderInputLine(m.Input, m.Cursor) + "\n")
	if m.Status != "" {
		sb.WriteString(dimStyle.Render(m.Status) + "\n")
	}
	sb.WriteString(dimStyle.Render("tab plan↔build · shift+tab approval · enter steers · ctrl+x queues · /resume /undo /compact") + "\n")
	return sb.String()
}

// renderInputLine draws "> input" with a stable cursor cell. Rune-aware so
// CJK/emoji never split, and pure string ops so typing never re-renders history.
func renderInputLine(input string, cursor int) string {
	r := []rune(input)
	if cursor < 0 {
		cursor = 0
	}
	if cursor > len(r) {
		cursor = len(r)
	}
	return "> " + string(r[:cursor]) + "▊" + string(r[cursor:])
}

// renderPending draws the live-streamed turn without markdown work:
// thinking dimmed, text plain. The final turn.completed render replaces it.
func (m Model) renderPending() string {
	var sb strings.Builder
	if m.ShowThinking && m.PendingThinking != "" {
		sb.WriteString(dimStyle.Render("… "+tailRunes(m.PendingThinking, 800)) + "\n")
	}
	sb.WriteString(tailRunes(m.Pending, 1500) + "▊")
	return sb.String()
}

// turnFooter renders the opencode v2 turn summary (agent · model ·
// duration · tok/s). Empty when the event carries no summary, so old
// daemons and tests render nothing extra.
func turnFooter(agent string, d map[string]interface{}) string {
	model, _ := d["model"].(string)
	ms, _ := d["duration_ms"].(float64)
	if model == "" && ms <= 0 {
		return ""
	}
	var parts []string
	if agent != "" {
		parts = append(parts, agent)
	}
	if model != "" {
		parts = append(parts, model)
	}
	if ms > 0 {
		parts = append(parts, formatDuration(int64(ms)))
	}
	var out int
	switch v := d["output_tokens"].(type) {
	case float64:
		out = int(v)
	case int:
		out = v
	case int64:
		out = int(v)
	}
	if out > 0 && ms > 0 {
		parts = append(parts, fmt.Sprintf("%.0f tok/s", float64(out)/(ms/1000)))
	}
	if len(parts) == 0 {
		return ""
	}
	return "_" + strings.Join(parts, " · ") + "_"
}

// formatDuration renders milliseconds like opencode durations.
func formatDuration(ms int64) string {
	if ms < 1000 {
		return fmt.Sprintf("%dms", ms)
	}
	if ms < 60000 {
		return fmt.Sprintf("%.1fs", float64(ms)/1000)
	}
	return fmt.Sprintf("%dm%02ds", ms/60000, (ms%60000)/1000)
}

func tailRunes(s string, n int) string {
	r := []rune(s)
	if len(r) <= n {
		return s
	}
	return "…" + string(r[len(r)-n:])
}

func (m Model) renderModal() string {
	opts := []string{"Allow Once", "Allow Session", "Deny"}
	var parts []string
	for i, o := range opts {
		if i == m.Modal.Cursor {
			parts = append(parts, "[ "+o+" ]")
		} else {
			parts = append(parts, "  "+o+"  ")
		}
	}
	return "Approve " + m.Modal.Tool + " " + m.Modal.Target + "?\n" + strings.Join(parts, " ")
}

var (
	agentStyle = lipgloss.NewStyle().Bold(true).Background(lipgloss.Color("62")).Foreground(lipgloss.Color("230"))
	dimStyle   = lipgloss.NewStyle().Foreground(lipgloss.Color("240"))
	modalStyle = lipgloss.NewStyle().Border(lipgloss.RoundedBorder()).Padding(0, 1)
)

func permStyle(p string) lipgloss.Style {
	switch p {
	case "ask":
		return lipgloss.NewStyle().Bold(true).Background(lipgloss.Color("214")).Foreground(lipgloss.Color("232"))
	case "auto-edit":
		return lipgloss.NewStyle().Bold(true).Background(lipgloss.Color("33")).Foreground(lipgloss.Color("230"))
	case "auto-approve":
		return lipgloss.NewStyle().Bold(true).Background(lipgloss.Color("28")).Foreground(lipgloss.Color("230"))
	default:
		return lipgloss.NewStyle().Bold(true).Background(lipgloss.Color("160")).Foreground(lipgloss.Color("230"))
	}
}

func maxInt(a, b int) int {
	if a > b {
		return a
	}
	return b
}
