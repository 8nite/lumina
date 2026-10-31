// Package tui is the Terminal TUI client: streaming timeline, Tab primary
// agent toggle (PLAN↔BUILD), Shift+Tab approval cycling, /resume + /continue
// same-workspace session attach, @file & [[memory]] completion, and inline permission modals. It talks to
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

	"github.com/8nite/lumina/internal/llm"
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

// QuestionModal is an inline question card: one model question at a time,
// answered by typing free text or a comma-separated option number list
// ("1,3"), enter to submit, esc to dismiss the whole prompt.
type QuestionModal struct {
	RequestID string
	Questions []QuestionItem
	Idx       int // current question
	Cursor    int // highlighted option
	Typed     string
	Answers   [][]string
}

// QuestionItem mirrors tools.Question over the SSE boundary.
type QuestionItem struct {
	Question string
	Header   string
	Options  []QuestionOption
	Multiple bool
}

// QuestionOption mirrors tools.QuestionOption over the SSE boundary.
type QuestionOption struct {
	Label       string
	Description string
}

// EventMsg carries one daemon SSE event into Update.
type EventMsg struct{ Event sdk.Event }

// SessionsMsg carries the /resume picker entries (same-workspace only).
type SessionsMsg struct{ Sessions []*sdk.Session }

// AttachedMsg carries a direct /continue attach (latest previous session
// in the same workspace, current session excluded).
type AttachedMsg struct{ Session *sdk.Session }

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

// RequestResyncMsg asks the model to refetch authoritative session state
// (transcript + running flag + pending prompts). Sent on startup and after
// every SSE reconnect so missed events reconcile instead of leaving a dead
// spinner and an empty timeline.
type RequestResyncMsg struct{}

// ResyncMsg carries the reconciled timeline. Lines holds the full rebuilt
// transcript in display form; Working/Status restore the spinner; the
// modals restore approval/question prompts missed while disconnected.
type ResyncMsg struct {
	Lines    []string
	Working  bool
	Status   string
	CtxUsed  int
	CtxMax   int
	Modal    *PermModal
	QModal   *QuestionModal
	HadState bool // false when the daemon was unreachable (keep local view)
}

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
		{"/variant", "pick the reasoning-effort variant (usage: /variant [" + strings.Join(llm.EffortVariants, "|") + "])"},
		{"/mode", "view or set approval mode (ask|auto-edit|auto-approve|bypass)"},
		{"/connect", "connect a provider via device web-auth (usage: /connect [provider] [github-url])"},
		{"/compact", "summarize context to free the window"},
		{"/thinking", "show or hide model reasoning while working (show|hide)"},
		{"/undo", "restore the last shadow-git checkpoint"},
		{"/redo", "re-apply the undone checkpoint"},
		{"/resume", "pick a previous session in this workspace"},
		{"/continue", "attach the latest session in this workspace"},
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
	QModal   *QuestionModal
	Sessions []*sdk.Session // /resume picker entries
	// Model picker (/model) entries + mode flag. Reuses PickIdx.
	ModelOptions []string
	Picking      bool
	PickingModel bool
	// Variant picker (/variant) entries + mode flag. Reuses PickIdx.
	// Effort holds the current reasoning-effort variant ("" = auto).
	Effort          string
	VariantOptions  []string
	PickingVariant  bool
	PickIdx         int
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
	// Scroll is the history offset from the live tail (0 = follow).
	// PgUp/Shift+Up (or Up with an empty picker/completion stack) scrolls
	// up; PgDown/Shift+Down/End snaps back. Any appended timeline line
	// resets it to 0 so new output is never hidden.
	Scroll int
	// Pending holds live-streamed assistant text (turn.delta), rendered
	// plain until turn.completed replaces it with the final markdown.
	Pending         string
	PendingThinking string
	// ShowThinking mirrors `session.thinking` (default hide):
	// reasoning streams but only renders when enabled (/thinking).
	ShowThinking bool
	// Working tracks an in-flight turn (send/turn.started → completed/failed).
	// Enter steers it, Ctrl+X then Enter queues for later.
	Working    bool
	QueueArmed bool
	// CtxUsed/CtxMax is the live context gauge (estimated prompt tokens vs
	// window, from turn.context SSE); TurnStart feeds the elapsed working
	// indicator.
	CtxUsed   int
	CtxMax    int
	TurnStart time.Time
	quitting  bool
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

// appendLine stores source + its rendered form together. Tool and error
// lines are colored by group (see renderLine) so thinking, reads, edits,
// exec/bash, web, and failures scan apart; everything else renders as
// plain markdown. Lines stays plain text (tests + copy-paste).
func (m *Model) appendLine(src string) {
	m.Lines = append(m.Lines, src)
	m.Rendered = append(m.Rendered, renderLine(src, m.Width))
	m.Scroll = 0 // new output snaps back to the live tail
}

// renderLine renders one markdown block once. Tool and error lines skip
// glamour and render directly: glamour's dark style paints every `code`
// span the same red, which would hide the per-group color on the tool
// name. View must never call this per frame; lines are rendered on append
// and on width change only.
func renderLine(src string, width int) string {
	if strings.HasPrefix(src, "· `") {
		return renderToolLine(src)
	}
	if strings.HasPrefix(src, "**error:**") {
		return errToolStyle.Render("error: " + strings.TrimSpace(strings.TrimPrefix(src, "**error:**")))
	}
	return renderCached(src, width)
}

// renderToolLine colors one `· `tool` target → preview` row by group:
// the tool name carries the lane color, target/preview stay dim (red on
// error) so the lane scans at a glance.
func renderToolLine(src string) string {
	rest := strings.TrimPrefix(src, "· `")
	tool := rest
	after := ""
	if i := strings.Index(rest, "`"); i >= 0 {
		tool = rest[:i]
		after = rest[i+1:]
	}
	target, preview := after, ""
	if i := strings.Index(after, "→"); i >= 0 {
		target = strings.TrimSpace(after[:i])
		preview = strings.TrimSpace(after[i+len("→"):])
	} else {
		target = strings.TrimSpace(after)
	}
	isErr := strings.HasPrefix(preview, "error:")
	st := toolLineStyle(tool, isErr)
	var sb strings.Builder
	sb.WriteString(dimStyle.Render("· "))
	sb.WriteString(st.Render(tool))
	if target != "" {
		sb.WriteString(" " + dimStyle.Render(target))
	}
	if preview != "" || strings.Contains(after, "→") {
		sb.WriteString(" " + dimStyle.Render("→") + " ")
		if isErr {
			sb.WriteString(errToolStyle.Render(preview))
		} else {
			sb.WriteString(dimStyle.Render(preview))
		}
	}
	return sb.String()
}

// flatLines expands rendered blocks (which contain embedded newlines) into
// individual visual rows for paging and scroll math.
func (m Model) flatLines() []string {
	var out []string
	for _, block := range m.Rendered {
		out = append(out, strings.Split(block, "\n")...)
	}
	return out
}

// historyBudget returns how many visual history rows fit the current Height,
// mirroring the reserved rows accounted for in View (header, input, status,
// help, modals, pickers, completion).
func (m Model) historyBudget() int {
	if m.Height <= 0 {
		return 100
	}
	reserved := 6
	if m.Modal != nil {
		reserved += 3
	}
	if m.QModal != nil {
		reserved += len(m.QModal.Questions) + 5
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
	if m.PickingVariant {
		reserved += len(m.VariantOptions) + 1
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
	return budget
}

// maxScroll clamps the offset so the window never scrolls past the oldest row.
func (m Model) maxScroll() int {
	total := len(m.flatLines())
	max := total - m.historyBudget()
	if max < 0 {
		return 0
	}
	return max
}

// scrollUp moves the viewport toward older history, clamping at the top.
func (m *Model) scrollUp(n int) {
	if n <= 0 {
		n = 1
	}
	m.Scroll += n
	if max := m.maxScroll(); m.Scroll > max {
		m.Scroll = max
	}
}

// scrollDown moves the viewport toward the live tail, clamping at 0.
func (m *Model) scrollDown(n int) {
	if n <= 0 {
		n = 1
	}
	m.Scroll -= n
	if m.Scroll < 0 {
		m.Scroll = 0
	}
}

// rerenderAll rebuilds the rendered cache after a width change.
func (m *Model) rerenderAll() {
	m.Rendered = m.Rendered[:0]
	for _, ln := range m.Lines {
		m.Rendered = append(m.Rendered, renderLine(ln, m.Width))
	}
	if max := m.maxScroll(); m.Scroll > max {
		m.Scroll = max
	}
}

// New creates a model bound to a session.
func New(c *sdk.Client, sess *sdk.Session) Model {
	agent, perm := "build", "bypass"
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

// transcriptToLines maps persisted transcript rows onto the same timeline
// shapes the live event path appends: user turns as `**you:** …`,
// assistant turns verbatim, tool/tool_error turns as `· …` rows.
func transcriptToLines(turns []sdk.TranscriptTurn) []string {
	var out []string
	for _, t := range turns {
		switch t.Role {
		case "user":
			out = append(out, "**you:** "+t.Text)
		case "assistant":
			out = append(out, t.Text)
		case "tool", "tool_error":
			text := strings.TrimSpace(t.Text)
			if text == "" {
				text = "(no output)"
			}
			if t.Role == "tool_error" {
				out = append(out, "· error: "+text)
			} else {
				out = append(out, "· "+text)
			}
		}
	}
	return out
}

// resyncCmd fetches authoritative session state (running flag + pending
// prompts) plus the persisted transcript, and returns a ResyncMsg. The
// daemon stores the user turn at turn start and tool turns incrementally,
// so a mid-turn resync already shows partial history.
func (m Model) resyncCmd() tea.Cmd {
	c, sess := m.Client, m.Session
	if c == nil || sess == nil {
		return nil
	}
	id := sess.ID
	return func() tea.Msg {
		st, err := c.SessionState(bg(), id)
		if err != nil {
			return ResyncMsg{HadState: false}
		}
		turns, err := c.ExportTranscript(bg(), id)
		if err != nil {
			return ResyncMsg{HadState: false}
		}
		msg := ResyncMsg{HadState: true, Working: st.Running}
		msg.Lines = transcriptToLines(turns)
		if st.Running {
			msg.Status = "working… (reconnected — catching up…)"
		}
		if st.Permission != nil && st.Permission.RequestID != "" {
			msg.Modal = &PermModal{Tool: st.Permission.Tool, Target: st.Permission.Target, RequestID: st.Permission.RequestID}
			if msg.Modal.Tool == "" {
				msg.Modal.Tool = "tool"
			}
			msg.Status = "waiting approval: " + strings.TrimSpace(msg.Modal.Tool+" "+msg.Modal.Target)
			msg.Working = true
		}
		if st.Question != nil && st.Question.RequestID != "" && len(st.Question.Questions) > 0 {
			qm := &QuestionModal{RequestID: st.Question.RequestID}
			for _, q := range st.Question.Questions {
				item := QuestionItem{Question: q.Question, Header: q.Header, Multiple: q.Multiple}
				for _, o := range q.Options {
					item.Options = append(item.Options, QuestionOption{Label: o.Label, Description: o.Description})
				}
				qm.Questions = append(qm.Questions, item)
			}
			msg.QModal = qm
			msg.Status = "question: awaiting answer"
			msg.Working = true
		}
		return msg
	}
}

// applyResync rebuilds the timeline from a ResyncMsg. The two header lines
// ("# lumina" + session line) are always kept; transcript lines replace
// everything after them, except a locally-appended trailing `**you:**`
// row the export raced ahead of (POST returns before the async runner
// persists) is preserved so a just-sent prompt never vanishes.
func (m *Model) applyResync(msg ResyncMsg) {
	if !msg.HadState {
		return
	}
	head := 0
	if len(m.Lines) > 0 && m.Lines[0] == "# lumina" {
		head = 1
		if len(m.Lines) > 1 {
			head = 2
		}
	}
	trailing := ""
	if n := len(m.Lines); n > head {
		if last := m.Lines[n-1]; strings.HasPrefix(last, "**you:** ") {
			found := false
			for _, l := range msg.Lines {
				if l == last {
					found = true
					break
				}
			}
			if !found {
				trailing = last
			}
		}
	}
	m.Lines = append(m.Lines[:head], msg.Lines...)
	m.Rendered = m.Rendered[:0]
	for _, ln := range m.Lines {
		m.Rendered = append(m.Rendered, renderLine(ln, m.Width))
	}
	if trailing != "" {
		m.Lines = append(m.Lines, trailing)
		m.Rendered = append(m.Rendered, renderLine(trailing, m.Width))
	}
	m.Scroll = 0
	m.Working = msg.Working
	if msg.Working {
		m.TurnStart = time.Now()
	} else {
		m.Pending = ""
		m.PendingThinking = ""
	}
	m.Status = msg.Status
	m.Modal = msg.Modal
	m.QModal = msg.QModal
}

// closeEmptyPickers closes optimistically opened pickers whose options never
// loaded (fetch error, direct-connect/no-match short-circuit). Pickers with
// options stay open so background turns never steal an active selection.
func (m *Model) closeEmptyPickers() {
	if m.PickingModel && len(m.ModelOptions) == 0 {
		m.PickingModel = false
	}
	if m.Picking && len(m.Sessions) == 0 {
		m.Picking = false
	}
}

// Init loads authoritative history when attached, so `-c`/`-s` sessions
// render their transcript (and a mid-turn spinner) instead of an empty
// timeline. The SSE fan-in lives in the caller (tui_client.go), which also
// sends RequestResyncMsg after every reconnect.
func (m Model) Init() tea.Cmd {
	if m.Client == nil || m.Session == nil {
		return m.subscribeCmd()
	}
	return tea.Batch(m.subscribeCmd(), m.resyncCmd())
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
	m.Perm = "bypass"
}

func (m *Model) pushSystem(s string) {
	m.appendLine("_" + s + "_")
}

// sendDelivery picks steer vs queue: Enter steers the
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
	case "/variant", "/effort", "/variants":
		// /variant — bare opens the picker on the current variant; with
		// text, an exact variant applies directly, a substring filters
		// the picker, unknown reports the choices.
		arg := ""
		if len(fields) > 1 {
			arg = strings.Join(fields[1:], " ")
		}
		m.applyVariant(arg)
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
	case "/resume", "/continue":
		ws, cur := "", ""
		if m.Session != nil {
			ws = m.Session.WorkspaceDir
			cur = m.Session.ID
		}
		if fields[0] == "/continue" {
			// CLI parity (-c): attach the latest previous session in the
			// same workspace without opening the picker.
			m.pushSystem("continuing latest session…")
			if m.Client == nil {
				m.pushSystem("no daemon attached")
				return nil
			}
			c := m.Client
			return func() tea.Msg {
				list, err := c.ListSessions(bg(), ws)
				if err != nil {
					return ErrMsg{err}
				}
				for _, s := range list {
					if s.ID != cur {
						return AttachedMsg{Session: s}
					}
				}
				return TurnDoneMsg{"no previous sessions for this workspace"}
			}
		}
		m.Picking = true
		m.PickIdx = 0
		m.pushSystem("pick a session (↑↓ + enter)")
		if m.Client != nil {
			c := m.Client
			return func() tea.Msg {
				list, err := c.ListSessions(bg(), ws)
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

// variantIndex returns the cursor position of the current effort inside opts
// (matched normalized, so "" hits "auto"), or 0 when absent. The picker
// always opens on the live variant instead of a hard-coded top entry.
func (m *Model) variantIndex(opts []string) int {
	cur := llm.NormalizeEffort(m.Effort)
	for i, v := range opts {
		if llm.NormalizeEffort(v) == cur {
			return i
		}
	}
	return 0
}

// applyVariant sets the current reasoning-effort variant ("", low, medium,
// high, xhigh, max; "" renders as auto). Bare arg opens the picker; an
// exact variant applies directly; a substring filters the picker.
func (m *Model) applyVariant(arg string) {
	t := strings.ToLower(strings.TrimSpace(arg))
	if t == "" {
		m.VariantOptions = append([]string(nil), llm.EffortVariants...)
		m.PickingVariant = true
		m.PickIdx = m.variantIndex(m.VariantOptions)
		m.pushSystem("pick a variant (↑↓ + enter) — current: " + llm.EffortLabel(m.Effort))
		return
	}
	if llm.ValidEffort(t) {
		m.Effort = llm.NormalizeEffort(t)
		m.PickingVariant = false
		m.pushSystem("variant → " + llm.EffortLabel(m.Effort))
		return
	}
	var kept []string
	for _, v := range llm.EffortVariants {
		if strings.Contains(strings.ToLower(v), t) {
			kept = append(kept, v)
		}
	}
	if len(kept) == 1 {
		m.Effort = llm.NormalizeEffort(kept[0])
		m.PickingVariant = false
		m.pushSystem("variant → " + llm.EffortLabel(m.Effort))
		return
	}
	if len(kept) > 1 {
		m.VariantOptions = kept
		m.PickingVariant = true
		m.PickIdx = m.variantIndex(kept)
		m.pushSystem("pick a variant matching \"" + t + "\" (↑↓ + enter)")
		return
	}
	m.pushSystem("unknown variant " + arg + " (want " + strings.Join(llm.EffortVariants, "|") + ")")
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
		if max := m.maxScroll(); m.Scroll > max {
			m.Scroll = max
		}
		return m, nil
	case tea.KeyMsg:
		return m.updateKey(msg)
	case EventMsg:
		return m.updateEvent(msg.Event)
	case RequestResyncMsg:
		if cmd := m.resyncCmd(); cmd != nil {
			return m, cmd
		}
		return m, nil
	case ResyncMsg:
		m.applyResync(msg)
		return m, nil
	case TurnDoneMsg:
		m.appendLine(msg.Text)
		m.Status = ""
		// A command that finished without loading picker options (direct
		// single-match /model connect, no-match/filter reports) must not
		// leave the optimistically opened picker on screen. Loaded pickers
		// (options present) stay open: background turns must not steal them.
		m.closeEmptyPickers()
		return m, nil
	case ModelsMsg:
		m.ModelOptions = msg.Models
		m.PickingModel = len(msg.Models) > 0
		m.PickIdx = 0
		return m, nil
	case SessionsMsg:
		// Picker lists *previous* sessions: drop the current session so
		// enter always attaches somewhere else. The daemon already scoped
		// the list to this workspace via ?directory=.
		cur := ""
		if m.Session != nil {
			cur = m.Session.ID
		}
		var kept []*sdk.Session
		for _, s := range msg.Sessions {
			if s == nil || s.ID == cur {
				continue
			}
			kept = append(kept, s)
		}
		m.Sessions = kept
		m.Picking = len(kept) > 0
		m.PickIdx = 0
		if len(kept) == 0 {
			m.Picking = false
			m.pushSystem("no previous sessions for this workspace")
		}
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
	case AttachedMsg:
		if msg.Session != nil {
			m.Session = msg.Session
			if msg.Session.Agent != "" {
				m.Agent = msg.Session.Agent
			}
			if msg.Session.PermissionMode != "" {
				m.Perm = msg.Session.PermissionMode
			}
			m.pushSystem("attached → " + msg.Session.ID)
			// Drop the previous session's live state; the resync below
			// rebuilds the timeline (history + spinner + modals) for the
			// newly attached session.
			m.Pending = ""
			m.PendingThinking = ""
			m.Working = false
			m.Status = ""
			m.Modal = nil
			m.QModal = nil
			if cmd := m.resyncCmd(); cmd != nil {
				return m, cmd
			}
		}
		return m, nil
	case ErrMsg:
		if m.Status == "working…" {
			m.appendLine("**error:** " + msg.Err.Error())
		}
		m.Status = "error: " + msg.Err.Error()
		// A failed fetch must not leave an empty picker open either.
		m.closeEmptyPickers()
		return m, nil
	}
	return m, nil
}

func (m Model) updateKey(msg tea.KeyMsg) (tea.Model, tea.Cmd) {
	// Question card captures all keys while open.
	if m.QModal != nil {
		return m.updateQuestionKey(msg)
	}
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
	// Variant picker navigation.
	if m.PickingVariant {
		switch msg.String() {
		case "up", "k":
			if m.PickIdx > 0 {
				m.PickIdx--
			}
			return m, nil
		case "down", "j":
			if m.PickIdx+1 < len(m.VariantOptions) {
				m.PickIdx++
			}
			return m, nil
		case "enter":
			if m.PickIdx < len(m.VariantOptions) {
				m.Effort = llm.NormalizeEffort(m.VariantOptions[m.PickIdx])
				m.PickingVariant = false
				m.pushSystem("variant → " + llm.EffortLabel(m.Effort))
				return m, nil
			}
			m.PickingVariant = false
			return m, nil
		case "esc":
			m.PickingVariant = false
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
				if m.Session.Agent != "" {
					m.Agent = m.Session.Agent
				}
				if m.Session.PermissionMode != "" {
					m.Perm = m.Session.PermissionMode
				}
				m.pushSystem("attached → " + m.Session.ID)
				m.Picking = false
				// See AttachedMsg: rebuild the timeline for the new
				// session instead of showing the previous one's rows.
				m.Pending = ""
				m.PendingThinking = ""
				m.Working = false
				m.Status = ""
				m.Modal = nil
				m.QModal = nil
				if cmd := m.resyncCmd(); cmd != nil {
					return m, cmd
				}
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
	case "pgup", "shift+up", "ctrl+b":
		m.scrollUp(m.historyBudget()/2 + 1)
		return m, nil
	case "pgdown", "shift+down", "ctrl+f":
		m.scrollDown(m.historyBudget()/2 + 1)
		return m, nil
	case "home":
		m.Scroll = m.maxScroll()
		return m, nil
	case "end":
		m.Scroll = 0
		return m, nil
	case "up":
		m.scrollUp(3)
		return m, nil
	case "down":
		m.scrollDown(3)
		return m, nil
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
			effort := m.Effort
			return m, func() tea.Msg {
				if err := c.Prompt(bg(), id, sdk.PromptInput{Text: t, Delivery: delivery, Effort: effort}); err != nil {
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
		m.Scroll = 0 // esc also snaps back to the live tail
	default:
		// Printable runes batch as KeyRunes, except a bare space which
		// arrives as KeySpace (bubbletea maps it to " " for compat).
		if msg.Type == tea.KeyRunes || msg.Type == tea.KeySpace {
			m.insertRunes(msg.String())
		}
	}
	return m, m.computeCompletions()
}

// Question-card key handling (updateQuestionKey and helpers).
func (m Model) updateQuestionKey(msg tea.KeyMsg) (tea.Model, tea.Cmd) {
	qm := m.QModal
	if qm == nil || qm.Idx >= len(qm.Questions) {
		m.QModal = nil
		return m, nil
	}
	q := qm.Questions[qm.Idx]
	switch msg.String() {
	case "up", "k":
		if qm.Cursor > 0 {
			qm.Cursor--
		}
	case "down", "j":
		if qm.Cursor+1 < len(q.Options) {
			qm.Cursor++
		}
	case "backspace":
		if qm.Typed != "" {
			qm.Typed = qm.Typed[:len(qm.Typed)-1]
		}
	case "enter":
		ans := parseQuestionAnswer(q, qm.Typed, qm.Cursor)
		qm.Answers = append(qm.Answers, ans)
		qm.Idx++
		qm.Cursor = 0
		qm.Typed = ""
		if qm.Idx >= len(qm.Questions) {
			return m.submitQuestion()
		}
	case "esc":
		m.QModal = nil
		m.Status = "question: dismissed"
		if m.Client != nil && qm.RequestID != "" {
			c, id := m.Client, qm.RequestID
			return m, func() tea.Msg {
				if err := c.DismissQuestion(bg(), id); err != nil {
					return ErrMsg{err}
				}
				return nil
			}
		}
	default:
		if s := msg.String(); len(s) == 1 {
			qm.Typed += s
		}
	}
	return m, nil
}

// submitQuestion posts the collected answers and closes the card.
func (m Model) submitQuestion() (tea.Model, tea.Cmd) {
	qm := m.QModal
	m.QModal = nil
	m.Status = "question: answered"
	if m.Client == nil || qm == nil || qm.RequestID == "" {
		return m, nil
	}
	c, id, answers := m.Client, qm.RequestID, qm.Answers
	return m, func() tea.Msg {
		if err := c.AnswerQuestion(bg(), id, answers); err != nil {
			return ErrMsg{err}
		}
		return nil
	}
}

// decodeQuestionItem parses one daemon question payload (question/header/
// options/multiple) into modal state; unknown shapes degrade to plain text.
func decodeQuestionItem(v interface{}) QuestionItem {
	m, _ := v.(map[string]interface{})
	if m == nil {
		return QuestionItem{Question: ""}
	}
	str := func(key string) string {
		s, _ := m[key].(string)
		return s
	}
	qi := QuestionItem{Question: str("question"), Header: str("header")}
	if b, _ := m["multiple"].(bool); b {
		qi.Multiple = true
	}
	if opts, _ := m["options"].([]interface{}); opts != nil {
		for _, o := range opts {
			om, _ := o.(map[string]interface{})
			if om == nil {
				continue
			}
			label, _ := om["label"].(string)
			desc, _ := om["description"].(string)
			if label != "" {
				qi.Options = append(qi.Options, QuestionOption{Label: label, Description: desc})
			}
		}
	}
	return qi
}

// parseQuestionAnswer maps typed input to option labels: a comma-separated
// number list ("1,3") selects those options, anything else is a free-text
// answer, and empty input takes the highlighted option.
func parseQuestionAnswer(q QuestionItem, typed string, cursor int) []string {
	if t := strings.TrimSpace(typed); t != "" {
		var picks []string
		numbered := true
		for _, part := range strings.Split(t, ",") {
			n := 0
			for _, r := range strings.TrimSpace(part) {
				if r < '0' || r > '9' {
					numbered = false
					break
				}
				n = n*10 + int(r-'0')
			}
			if !numbered || n < 1 || n > len(q.Options) {
				numbered = false
				break
			}
			picks = append(picks, q.Options[n-1].Label)
		}
		if numbered && len(picks) > 0 {
			if !q.Multiple {
				return picks[:1]
			}
			return picks
		}
		return []string{t}
	}
	if cursor >= 0 && cursor < len(q.Options) {
		return []string{q.Options[cursor].Label}
	}
	return []string{""}
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
			"permission.asked", "permission",
			"question.asked":
			return m, nil
		}
	}
	switch e.Type {
	case "turn.started":
		// Pending state: show what the
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
		m.TurnStart = time.Now()
	case "turn.progress":
		var d map[string]string
		if err := jsonUnmarshal(e.Data, &d); err == nil {
			if s := d["stage"]; s != "" {
				m.Status = s
				// A progress row for an apparently-idle session means we
				// missed turn.started while disconnected: recover the
				// spinner instead of looking stuck.
				if !m.Working && !strings.HasPrefix(s, "queued #") {
					m.Working = true
					m.TurnStart = time.Now()
				}
			}
		}
	case "turn.delta":
		var d map[string]string
		if err := jsonUnmarshal(e.Data, &d); err == nil {
			if d["delta"] != "" || d["thinking"] != "" {
				// Same missed-started recovery as progress: a delta
				// implies a live turn even without a prior started.
				if !m.Working {
					m.Working = true
					m.TurnStart = time.Now()
				}
			}
			// Cap the live buffer; the final text arrives via turn.completed.
			if len([]rune(m.Pending+m.PendingThinking)) < 4000 {
				m.Pending += d["delta"]
				m.PendingThinking += d["thinking"]
			}
			m.Status = "working…"
		}
	case "turn.context":
		var d map[string]interface{}
		if err := jsonUnmarshal(e.Data, &d); err == nil {
			m.CtxUsed = numField(d, "context_tokens")
			m.CtxMax = numField(d, "max_tokens")
		}
	case "turn.tool":
		var d map[string]interface{}
		if err := jsonUnmarshal(e.Data, &d); err == nil {
			tool, _ := d["tool"].(string)
			target, _ := d["target"].(string)
			if tool != "" {
				// Missed-started recovery (see progress/delta above).
				if !m.Working {
					m.Working = true
					m.TurnStart = time.Now()
				}
				st := "tool: " + tool
				if target != "" {
					st += " " + truncate(target, 60)
				}
				m.Status = st
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
		// Turn summary: agent, model, duration (+tok/s when
		// the provider reported usage) as the assistant footer.
		var d map[string]interface{}
		if err := jsonUnmarshal(e.Data, &d); err == nil {
			if t, _ := d["text"].(string); t != "" {
				m.appendLine(t)
			}
			if u := numField(d, "context_tokens"); u > 0 {
				m.CtxUsed = u
			}
			if mx := numField(d, "max_tokens"); mx > 0 {
				m.CtxMax = mx
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
	case "question.resolved":
		var d map[string]string
		if err := jsonUnmarshal(e.Data, &d); err == nil {
			m.Status = "question: answered"
		}
		if m.QModal != nil {
			m.QModal = nil
		}
	case "question.asked", "question":
		var d map[string]interface{}
		qm := &QuestionModal{}
		if err := jsonUnmarshal(e.Data, &d); err == nil {
			if v, _ := d["request_id"].(string); v != "" {
				qm.RequestID = v
			}
			if qs, _ := d["questions"].([]interface{}); qs != nil {
				for _, qi := range qs {
					qm.Questions = append(qm.Questions, decodeQuestionItem(qi))
				}
			}
		} else if s := string(e.Data); s != "" {
			qm.Questions = []QuestionItem{{Question: s}}
		}
		if len(qm.Questions) == 0 {
			qm.Questions = []QuestionItem{{Question: string(e.Data)}}
		}
		m.QModal = qm
		m.Status = "question: awaiting answer"
		// A blocked turn is still in-flight: keep the spinner so the wait
		// never looks like a stall.
		if !m.Working {
			m.Working = true
			m.TurnStart = time.Now()
		}
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
		m.Status = "waiting approval: " + strings.TrimSpace(modal.Tool+" "+modal.Target)
		// A blocked turn is still in-flight (see question.asked above).
		if !m.Working {
			m.Working = true
			m.TurnStart = time.Now()
		}
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
	fmt.Fprintf(&sb, "%s %s %s %s%s\n", agentPill, permPill, variantPill(m.Effort), dimStyle.Render(sessID(m.Session)), ctxPill(m.CtxUsed, m.CtxMax))
	sb.WriteString(strings.Repeat("─", maxInt(20, m.Width)) + "\n")
	lines := m.flatLines()
	if m.Height > 0 {
		budget := m.historyBudget()
		// Scroll banner occupies one row when active.
		if m.Scroll > 0 {
			budget--
			if budget < 1 {
				budget = 1
			}
		}
		scroll := m.Scroll
		if max := m.maxScroll(); scroll > max {
			scroll = max
		}
		if len(lines) > budget {
			end := len(lines) - scroll
			if end > len(lines) {
				end = len(lines)
			}
			if end < 0 {
				end = 0
			}
			start := end - budget
			if start < 0 {
				start = 0
			}
			lines = lines[start:end]
		}
		if m.Scroll > 0 {
			fmt.Fprintf(&sb, "%s\n", dimStyle.Render(fmt.Sprintf("… scroll %d↑ · pgdn/end to follow …", m.Scroll)))
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
		sb.WriteString(permModalStyle.Render(m.renderModal()) + "\n")
	}
	if m.QModal != nil {
		sb.WriteString(questionModalStyle.Render(m.renderQuestionModal()) + "\n")
	}
	if m.Picking {
		sb.WriteString(dimStyle.Render("(picker: previous sessions in this workspace, enter to attach, esc to close)") + "\n")
		for i, s := range m.Sessions {
			mark := "  "
			if i == m.PickIdx {
				mark = "> "
			}
			label := s.ID + " · " + s.Agent
			if s.Title != "" {
				label += " · " + truncate(s.Title, 40)
			}
			sb.WriteString(dimStyle.Render(mark+label) + "\n")
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
	if m.PickingVariant {
		sb.WriteString(dimStyle.Render("(picker: choose variant "+strings.Join(llm.EffortVariants, "|")+", enter to select, esc to close)") + "\n")
		for i, opt := range m.VariantOptions {
			mark := "  "
			if i == m.PickIdx {
				mark = "> "
			}
			cur := ""
			if llm.NormalizeEffort(opt) == llm.NormalizeEffort(m.Effort) {
				cur = " ●"
			}
			sb.WriteString(dimStyle.Render(mark+opt+cur) + "\n")
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
	sb.WriteString(renderInputLine(m.Input, m.Cursor, m.Width) + "\n")
	if m.Status != "" {
		sb.WriteString(statusStyle(m.Status).Render(m.Status) + "\n")
	}
	sb.WriteString(dimStyle.Render("tab plan↔build · shift+tab approval · pgup/pgdn scroll · enter steers · ctrl+x queues · /model /variant /resume /undo /compact") + "\n")
	return sb.String()
}

// renderInputLine draws "> input" with a stable cursor cell. Rune-aware so
// CJK/emoji never split, and pure string ops so typing never re-renders history.
// The line is always exactly one visual row: overlong input is windowed
// around the cursor (with … markers) so it can never wrap and push the
// status/help lines out of place.
func renderInputLine(input string, cursor, width int) string {
	r := []rune(input)
	if cursor < 0 {
		cursor = 0
	}
	if cursor > len(r) {
		cursor = len(r)
	}
	if width <= 0 || len(r)+3 <= width {
		return "> " + string(r[:cursor]) + "▊" + string(r[cursor:])
	}
	avail := width - 3 // "> " prefix + cursor cell
	if avail < 10 {
		avail = 10
	}
	// Window [start,end) of avail runes containing the cursor.
	start := cursor - avail/2
	if start < 0 {
		start = 0
	}
	if start+avail > len(r) {
		start = len(r) - avail
	}
	end := start + avail
	// Reserve a cell per truncated side for the … marker.
	lo, hi := "", ""
	if start > 0 {
		start++
		lo = "…"
	}
	if end < len(r) {
		end--
		hi = "…"
	}
	rel := cursor - start
	win := r[start:end]
	if rel < 0 {
		rel = 0
	}
	if rel > len(win) {
		rel = len(win)
	}
	return "> " + lo + string(win[:rel]) + "▊" + string(win[rel:]) + hi
}

// renderPending draws the live-streamed turn without markdown work:
// thinking in purple, text plain. The final turn.completed render replaces it.
func (m Model) renderPending() string {
	var sb strings.Builder
	if m.ShowThinking && m.PendingThinking != "" {
		sb.WriteString(thinkStyle.Render("… "+tailRunes(m.PendingThinking, 800)) + "\n")
	}
	sb.WriteString(tailRunes(m.Pending, 1500) + "▊")
	return sb.String()
}

// numField reads a JSON number (float64/int/int64) as int, else 0.
func numField(d map[string]interface{}, k string) int {
	switch v := d[k].(type) {
	case float64:
		return int(v)
	case int:
		return v
	case int64:
		return int(v)
	}
	return 0
}

// turnFooter renders the turn summary (agent · model ·
// duration · tok/s · ctx ~used/max). Empty when the event carries no
// summary, so old daemons and tests render nothing extra.
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
	if u := numField(d, "context_tokens"); u > 0 {
		mx := numField(d, "max_tokens")
		if mx > 0 {
			parts = append(parts, fmt.Sprintf("ctx ~%s/%s", formatTokens(u), formatTokens(mx)))
		} else {
			parts = append(parts, fmt.Sprintf("ctx ~%s", formatTokens(u)))
		}
	}
	if len(parts) == 0 {
		return ""
	}
	return "_" + strings.Join(parts, " · ") + "_"
}

// formatTokens renders 12345 as "12.3k".
func formatTokens(n int) string {
	if n < 1000 {
		return fmt.Sprintf("%d", n)
	}
	return fmt.Sprintf("%.1fk", float64(n)/1000)
}

// formatDuration renders milliseconds as short durations.
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
			parts = append(parts, selectedStyle.Render("[ "+o+" ]"))
		} else {
			parts = append(parts, "  "+o+"  ")
		}
	}
	return "Approve " + m.Modal.Tool + " " + m.Modal.Target + "?\n" + strings.Join(parts, " ")
}

func (m Model) renderQuestionModal() string {
	qm := m.QModal
	if qm == nil || len(qm.Questions) == 0 {
		return ""
	}
	var sb strings.Builder
	if len(qm.Questions) > 1 {
		fmt.Fprintf(&sb, "Question %d/%d\n", qm.Idx+1, len(qm.Questions))
	}
	q := qm.Questions[qm.Idx]
	if q.Header != "" {
		sb.WriteString(q.Header + "\n")
	}
	sb.WriteString(q.Question + "\n")
	for i, o := range q.Options {
		mark := "  "
		label := fmt.Sprintf("%d. %s", i+1, o.Label)
		if i == qm.Cursor {
			mark = "> "
			label = selectedStyle.Render(label)
		}
		fmt.Fprintf(&sb, "%s%s", mark, label)
		if o.Description != "" {
			sb.WriteString(" — " + o.Description)
		}
		sb.WriteString("\n")
	}
	sb.WriteString("answer (number, 1,3, or text; enter submits, esc dismisses): " + qm.Typed)
	return sb.String()
}

var (
	agentStyle = lipgloss.NewStyle().Bold(true).Background(lipgloss.Color("62")).Foreground(lipgloss.Color("230"))
	dimStyle   = lipgloss.NewStyle().Foreground(lipgloss.Color("240"))
	modalStyle = lipgloss.NewStyle().Border(lipgloss.RoundedBorder()).Padding(0, 1)
	// Approval modal: yellow border so a blocked turn scans apart.
	permModalStyle = lipgloss.NewStyle().Border(lipgloss.RoundedBorder()).BorderForeground(lipgloss.Color("220")).Padding(0, 1)
	// Question modal: cyan border, distinct from approvals and errors.
	questionModalStyle = lipgloss.NewStyle().Border(lipgloss.RoundedBorder()).BorderForeground(lipgloss.Color("86")).Padding(0, 1)
	ctxStyle           = lipgloss.NewStyle().Foreground(lipgloss.Color("244"))

	// Timeline group colors, derived from the daemon event log:
	// turn.delta thinking + thinking…/summarizing… stages → purple;
	// turn.tool names grouped like the headless ▸ log + web READ_TOOLS:
	// reads → blue, edits → green, bash/exec → orange, web → teal,
	// plans/questions → yellow, failures → red.
	thinkStyle    = lipgloss.NewStyle().Foreground(lipgloss.Color("141")).Italic(true)
	readToolStyle = lipgloss.NewStyle().Foreground(lipgloss.Color("39"))
	editToolStyle = lipgloss.NewStyle().Foreground(lipgloss.Color("114"))
	execToolStyle = lipgloss.NewStyle().Foreground(lipgloss.Color("208")).Bold(true)
	webToolStyle  = lipgloss.NewStyle().Foreground(lipgloss.Color("86"))
	planToolStyle = lipgloss.NewStyle().Foreground(lipgloss.Color("220"))
	errToolStyle  = lipgloss.NewStyle().Foreground(lipgloss.Color("203")).Bold(true)
	selectedStyle = lipgloss.NewStyle().Foreground(lipgloss.Color("230")).Bold(true)
)

// toolGroup buckets a turn.tool name the way the logs group it: read-only
// inspection (web READ_TOOLS parity), file mutation, command execution
// (bash/task_output/subagent/mcp — the "command running" lane), web fetch,
// and planning/question plumbing.
func toolGroup(tool string) string {
	switch strings.ToLower(strings.TrimSpace(tool)) {
	case "read", "grep", "glob", "lsp", "kb_query", "skill":
		return "read"
	case "edit", "write", "patch", "memory_write":
		return "edit"
	case "bash", "task_output", "subagent", "mcp":
		return "exec"
	case "webfetch", "websearch":
		return "web"
	case "question", "todowrite":
		return "plan"
	default:
		return "other"
	}
}

// toolLineStyle maps one tool to its timeline color; errors always win red.
func toolLineStyle(tool string, isErr bool) lipgloss.Style {
	if isErr {
		return errToolStyle
	}
	switch toolGroup(tool) {
	case "read":
		return readToolStyle
	case "edit":
		return editToolStyle
	case "exec":
		return execToolStyle
	case "web":
		return webToolStyle
	case "plan":
		return planToolStyle
	default:
		return dimStyle
	}
}

// statusStyle colors the one-line status by lane: thinking purple, tool
// steps by group, approvals yellow, questions cyan, failures red.
func statusStyle(s string) lipgloss.Style {
	lower := strings.ToLower(s)
	switch {
	case strings.Contains(lower, "error") || strings.Contains(lower, "failed") || strings.Contains(lower, "denied"):
		return errToolStyle
	case strings.HasPrefix(lower, "tool:"):
		fields := strings.Fields(s)
		tool := ""
		if len(fields) >= 2 {
			tool = fields[1]
		}
		return toolLineStyle(tool, false)
	case strings.Contains(lower, "thinking") || strings.Contains(lower, "working") || strings.Contains(lower, "summarizing"):
		return thinkStyle
	case strings.Contains(lower, "approval") || strings.Contains(lower, "permission"):
		return planToolStyle
	case strings.Contains(lower, "question"):
		return webToolStyle
	case strings.Contains(lower, "queue") || strings.Contains(lower, "steer"):
		return webToolStyle
	case strings.Contains(lower, "background done") || strings.Contains(lower, "subagent"):
		return execToolStyle
	default:
		return dimStyle
	}
}

// ctxPill renders the live context gauge (" ctx ~12.3k/128k"), or "" when
// no estimate has arrived yet (old daemons).
func ctxPill(used, max int) string {
	if used <= 0 {
		return ""
	}
	if max > 0 {
		return ctxStyle.Render(fmt.Sprintf("  ctx ~%s/%s", formatTokens(used), formatTokens(max)))
	}
	return ctxStyle.Render(fmt.Sprintf("  ctx ~%s", formatTokens(used)))
}

// variantPill renders the current reasoning-effort variant (" variant:xhigh",
// auto when empty) so the header always shows what /variant selected.
func variantPill(effort string) string {
	return ctxStyle.Render(" variant:" + llm.EffortLabel(effort))
}

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
