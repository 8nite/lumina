package agent

import (
	"context"
	"fmt"
	"os"
	"runtime"
	"strings"
	"sync"
	"time"

	"github.com/8nite/lumina/internal/llm"
	"github.com/8nite/lumina/internal/rules"
	"github.com/8nite/lumina/internal/security"
	"github.com/8nite/lumina/internal/tools"
)

// Runner executes the turn loop: system prompt assembly, model call,
// security-gated tool execution, checkpointing, and compaction.
//
// Progress + approval lifecycle: the runner
// emits stage updates (thinking → tool → done) and pauses on
// permission.asked until the UI answers allow_once / allow_session /
// deny. Reimplemented here.
type Runner struct {
	store   *Store
	router  *llm.Router
	tools   *tools.Registry
	rulesFn func(workspace string) *rules.Bundle
	// OnProgress, when set, receives non-fatal stage updates for one
	// session (the daemon publishes them as turn.progress SSE events).
	OnProgress func(sessionID, stage string)
	// AskPermission blocks until the UI answers a permission.asked
	// request. It returns "allow_once", "allow_session", "allow",
	// "allow_always", or "deny". Nil preserves the legacy behavior
	// (tool call fails with an ask error fed back to the model).
	AskPermission func(ctx context.Context, sessionID, tool, target string) (string, error)
	// AskQuestion blocks until the UI answers a question.asked request,
	// returning one string list per question (multiple:true may yield
	// several). Nil keeps headless behavior: the question tool answers
	// with proceed-with-best-guess guidance instead of hanging the turn.
	AskQuestion func(ctx context.Context, sessionID string, questions []tools.Question) ([][]string, error)
	// OnDelta streams one model chunk (text and/or reasoning) for the
	// session. The daemon publishes it as a turn.delta SSE event.
	// Reasoning display follows `session.thinking`: hidden
	// by default, toggleable per client. Nil disables streaming.
	OnDelta func(sessionID, delta, thinking string)
	// OnToolStart/OnToolDone bracket one executed tool call (approval
	// waits excluded). Preview is a truncated output excerpt for display.
	OnToolStart func(sessionID, tool, target string)
	OnToolDone  func(sessionID, tool, target, preview string, isErr bool, durationMs int64)
	// OnTurnEnd reports one finished turn (agent/model/duration/tokens,
	// turn-summary style). Tokens are zero when the provider
	// does not report usage; tok/s is then omitted, never estimated.
	OnTurnEnd func(sessionID string, s TurnStats)
	// OnContext reports estimated context usage (prompt tokens vs window)
	// at turn start and as the agentic loop grows the message list. The
	// daemon publishes it as a turn.context SSE event; estimates use a
	// ~4 chars/token heuristic (ponytail: no tokenizer dependency).
	OnContext func(sessionID string, used, max int)

	mu      sync.Mutex
	granted map[string]bool // sessionID|tool-prefix → allow_session

	// inboxMu guards steered/queued prompts for running turns
	// (steer/queue inbox): steered text joins the active turn at the next
	// round boundary; queued turns chain after it completes.
	inboxMu sync.Mutex
	steered map[string][]string
	queued  map[string][]TurnInput
}

// NewRunner wires dependencies (router/tools may be nil in storage-only use).
func NewRunner(store *Store, router *llm.Router, reg *tools.Registry) *Runner {
	r := &Runner{store: store, router: router, tools: reg,
		rulesFn: func(ws string) *rules.Bundle {
			return rules.Load(rules.Options{WorkspaceDir: ws, ImportGlobalRules: true, ImportGlobalSkills: true})
		}}
	if reg != nil {
		reg.Subagent = r.runSubagentText
		// Lazy closure: AskQuestion may be assigned after NewRunner (the
		// daemon wires it once serve starts). A nil hook surfaces as
		// ErrQuestionUnwired so headless runs never block.
		reg.AskQuestion = func(ctx context.Context, sc tools.SecCtx, qs []tools.Question) ([][]string, error) {
			if r.AskQuestion == nil {
				return nil, tools.ErrQuestionUnwired
			}
			return r.AskQuestion(ctx, sc.SessionID, qs)
		}
		reg.OnBackgroundDone = func(sessionID, taskID, kind, label string) {
			r.progress(sessionID, "background done: "+taskID+" ("+kind+")")
		}
	}
	return r
}

// toolGuidanceBase is the static backbone of the backend system prompt. It
// mirrors opencode v2 `packages/core/src/session/runner/prompt/system.txt`.
// Per-tool lines are appended by renderToolGuidance, mirroring opencode v2
// `session/system-prompt.ts:render()` (shell/write/edit lines only when the
// tool is in the model-visible snapshot) plus Lumina's builtin (# Basic
// tools) and Lumina-only guidance. Lines marked [lumina] cover Lumina-only
// tools and rules.
const toolGuidanceBase = `# Harness
- Responses are rendered as GitHub-flavored Markdown.
- ` + "`<system-reminder>`" + ` blocks are harness instructions, not user-authored content. Read and follow them.
- Prefer parallelizing independent tool calls (batch them in one block).
# Communication
- Use clear file paths when referring to files.
- Keep responses clear and concise, and avoid unnecessary technical jargon.
# Working in codebases
- Keep changes consistent with the structure, naming, style, and patterns of the surrounding code.
- Treat unfamiliar files or changes as potential user work and investigate before deleting or overwriting them.`

// renderToolGuidance builds the generated per-tool section for one model
// snapshot: only lines for tools the model can actually call.
func renderToolGuidance(specs []llm.ToolSpec) string {
	active := map[string]bool{}
	for _, s := range specs {
		active[strings.ToLower(s.Name)] = true
	}
	var sb strings.Builder
	if active["bash"] || active["shell"] {
		sb.WriteString("- Prefer dedicated tools over shell commands; fall back to the shell when a tool cannot do what you need.\n")
		sb.WriteString("- Do not chain shell commands with separators like `echo \"====\";` or `printf '---'`; the output becomes noisy in a way that makes the user's side of the conversation worse.\n")
	}
	if active["write"] {
		sb.WriteString("- Use the write tool to create files or completely replace their content. Prefer using the edit tool for targeted changes.\n")
	}
	if active["patch"] {
		sb.WriteString("- Use the patch tool for multi-file or diff-style edits expressed as *** Begin Patch *** (Add/Update/Delete File); prefer edit for single targeted changes.\n")
	}
	if active["edit"] {
		sb.WriteString("- Use the edit tool for targeted changes to existing text files. It replaces the exact text in `old_string` with `new_string`, and the values must differ. By default, `old_string` must occur exactly once; include more surrounding context to make it unique, or set `replaceAll` to replace every occurrence.\n")
	}
	if active["read"] || active["grep"] || active["glob"] {
		sb.WriteString("# Basic tools\n")
		if active["read"] {
			sb.WriteString("- Read a file or directory with the read tool; text lines come back as 1-based `line: content` (line numbers are for reference only, never include them in edits). Use offset/limit for large files; prefer one larger read over many small slices, and use grep to find content in large files.\n")
		}
		if active["grep"] || active["glob"] {
			sb.WriteString("- Search file contents with grep (regex or literal, narrow with path/include) and find files with glob (e.g. `**/*.ts`, `src/**/*.tsx`).\n")
		}
	}
	if active["webfetch"] || active["websearch"] {
		sb.WriteString("- Fetch one HTTP(S) URL with webfetch (markdown default, text/html available); it is read-only and truncates past the cap. Use it for direct URLs or public JSON APIs.")
		if active["websearch"] {
			sb.WriteString(" websearch needs a provider key; when it reports no provider available, fall back to webfetch.")
		}
		sb.WriteString("\n")
	}
	lumina := false
	if active["websearch"] {
		sb.WriteString("# Lumina tools [lumina]\n")
		lumina = true
		sb.WriteString("- Web: `websearch` needs a provider key (TAVILY_API_KEY, EXA_API_KEY, FIRECRAWL_API_KEY, PARALLEL_API_KEY, or TINYFISH_API_KEY); when it reports no provider available, use `webfetch` on a direct URL or public JSON API instead (no key needed).\n")
	}
	if active["subagent"] {
		if !lumina {
			sb.WriteString("# Lumina tools [lumina]\n")
			lumina = true
		}
		sb.WriteString("- Delegation: `subagent` launches a scoped child (general for multi-step work, explore for read-only research); pass {agent, description, prompt} plus optional model override. Children cannot launch subagents.\n")
	}
	if active["bash"] && active["subagent"] {
		if !lumina {
			sb.WriteString("# Lumina tools [lumina]\n")
			lumina = true
		}
		sb.WriteString("- Background work: `bash` with background:true detaches long jobs (no timeout unless supplied); `subagent` with background:true detaches child sessions. Poll either with `task_output` until done — never leave a started task unpolled.\n")
	}
	if active["kb_query"] || active["memory_write"] {
		if !lumina {
			sb.WriteString("# Lumina tools [lumina]\n")
			lumina = true
		}
		sb.WriteString("- Memory: `kb_query` searches Markdown+graphify memory before grepping the codebase for project questions; `memory_write` saves durable facts to ./memory/<topic>.md.\n")
	}
	if active["skill"] {
		if !lumina {
			sb.WriteString("# Lumina tools [lumina]\n")
			lumina = true
		}
		sb.WriteString("- Skills: load a `skill` playbook body by name before doing the task it describes.\n")
	}
	sb.WriteString("- After tool results arrive, continue the task; call more tools if needed, else reply substantively, gathering every result (web, subagent, background) into the final answer.")
	return sb.String()
}

// envBlock renders the opencode-parity environment block (builtins.ts):
// today's date plus working directory, git presence, platform, and tmp
// preference. Pure string surgery — zero LLM tokens.
func envBlock(workspaceDir string) string {
	isGit := "no"
	if st, err := os.Stat(workspaceDir + "/.git"); err == nil && st != nil {
		isGit = "yes"
	} else if workspaceDir == "" {
		isGit = "unknown"
	}
	return "Today's date: " + time.Now().Format("Mon Jan 02 2006") + "\n" +
		"Here is some useful information about the environment you are running in:\n" +
		"<env>\n" +
		"  Working directory: " + workspaceDir + "\n" +
		"  Is directory a git repo: " + isGit + "\n" +
		"  Platform: " + runtime.GOOS + "\n" +
		"  Prefer " + os.TempDir() + " over generic system temporary directories such as /tmp; it is pre-created and approved for external access.\n" +
		"</env>"
}

// Steer appends text to the running turn's inbox. The loop picks it up
// at the next round boundary; leftovers chain as a queued turn so steering
// input is never silently dropped.
func (r *Runner) Steer(sessionID, text string) {
	if strings.TrimSpace(text) == "" {
		return
	}
	r.inboxMu.Lock()
	defer r.inboxMu.Unlock()
	if r.steered == nil {
		r.steered = map[string][]string{}
	}
	r.steered[sessionID] = append(r.steered[sessionID], text)
}

// Queue appends a full turn to run after the active one finishes.
// It returns the 1-based queue position.
func (r *Runner) Queue(sessionID string, in TurnInput) int {
	r.inboxMu.Lock()
	defer r.inboxMu.Unlock()
	if r.queued == nil {
		r.queued = map[string][]TurnInput{}
	}
	r.queued[sessionID] = append(r.queued[sessionID], in)
	return len(r.queued[sessionID])
}

// TakeQueued pops the next chained turn, if any.
func (r *Runner) TakeQueued(sessionID string) (TurnInput, bool) {
	r.inboxMu.Lock()
	defer r.inboxMu.Unlock()
	q := r.queued[sessionID]
	if len(q) == 0 {
		return TurnInput{}, false
	}
	next := q[0]
	r.queued[sessionID] = q[1:]
	return next, true
}

// takeSteered drains buffered steer text for one turn.
func (r *Runner) takeSteered(sessionID string) string {
	r.inboxMu.Lock()
	defer r.inboxMu.Unlock()
	parts := r.steered[sessionID]
	delete(r.steered, sessionID)
	return strings.TrimSpace(strings.Join(parts, "\n\n"))
}

// TurnInput is one user turn: text plus optional image attachments and a
// reasoning-effort variant ("", low, medium, high, xhigh, max).
type TurnInput struct {
	Text   string
	Images []llm.ImageAttachment
	Effort string
}

// RunPrompt records a user turn, streams one assistant turn, executes any
// requested tools under the session's agent/permission envelope, checkpoints,
// and returns the assistant text.
func (r *Runner) RunPrompt(ctx context.Context, sessionID string, in TurnInput) (string, error) {
	text := in.Text
	sess, err := r.store.GetSession(sessionID)
	if err != nil {
		return "", err
	}
	recorded := text
	for _, im := range in.Images {
		name := im.Name
		if name == "" {
			name = "image"
		}
		recorded += "\n[attached image: " + name + "]"
	}
	if _, err := r.store.AddTurn(sessionID, "", "user", recorded); err != nil {
		return "", err
	}
	if r.router == nil {
		// Storage/headless mode without a model: echo acknowledgement.
		reply := "(no model configured; turn recorded)"
		_, _ = r.store.AddTurn(sessionID, "", "assistant", reply)
		return reply, nil
	}
	bundle := r.rulesFn(sess.WorkspaceDir)
	specs := toolSpecs()
	if r.tools != nil && r.tools.WebSearchDisabled {
		specs = withoutTool(specs, "websearch")
	}
	turns, _ := r.store.ListTurns(sessionID)
	// Provider-advertised window for the gauge AND the compaction budget
	// (0 = unknown, shown as used-only with the legacy message ceiling).
	// Looked up once per turn; providers cache, and the cap keeps a slow
	// gateway from stalling the turn over metadata.
	modelName := sess.Model
	if modelName == "" && r.router != nil {
		modelName = r.router.ModelFor("executor", sess.Agent)
	}
	maxTokens := 0
	if r.router != nil && modelName != "" {
		lctx, cancel := context.WithTimeout(ctx, 15*time.Second)
		maxTokens = r.router.ContextWindow(lctx, modelName)
		cancel()
	}
	msgs := []llm.Message{{
		Role: "system",
		Content: bundle.SystemPrompt + "\n\n# Session\n" +
			"- workspace_dir: " + sess.WorkspaceDir + "\n" +
			"- agent: " + sess.Agent + " (plan=read-only except plan.md/.lumina/plans; build=full)\n" +
			"- permission_mode: " + sess.PermissionMode + "\n\n" +
			envBlock(sess.WorkspaceDir) + "\n\n" +
			toolGuidanceBase + "\n" + renderToolGuidance(specs) + "\n" + vaultGuidance + "\n",
	}}
	for _, t := range turns {
		if t.Role == "user" || t.Role == "assistant" {
			// Vault boundary: the provider sees refs, never values.
			sub, err := r.store.substituteSecrets(sessionID, t.Text)
			if err != nil {
				return "", err
			}
			msgs = append(msgs, llm.Message{Role: t.Role, Content: sub})
		}
	}
	// Provider-aware compaction: keep the newest turns under 80% of the
	// advertised window (or the newest 40 messages when unknown), so a long
	// session degrades to a smaller window instead of failing — or silently
	// dropping the current prompt, as the old oldest-first cap did.
	msgs = compactPrompt(msgs, specs, maxTokens)
	r.progress(sessionID, "thinking…")
	stream := r.deltaFunc(sessionID)
	turnStart := time.Now()
	var inTok, outTok int
	noteUsage := func(u *llm.Usage) {
		if u != nil {
			inTok, outTok = u.Input, u.Output
		}
	}
	ctxUsed := EstimateContextTokens(msgs, specs)
	r.emitContext(sessionID, ctxUsed, maxTokens)
	finishTurn := func() {
		if r.OnTurnEnd != nil {
			model := sess.Model
			if model == "" {
				model = r.router.ModelFor("executor", sess.Agent)
			}
			r.OnTurnEnd(sessionID, TurnStats{
				Model: model, DurationMs: time.Since(turnStart).Milliseconds(),
				InputTokens: inTok, OutputTokens: outTok,
				ContextTokens: ctxUsed, MaxTokens: maxTokens,
			})
		}
	}
	// Pre-flight: when the provider advertises a window and even the
	// compacted prompt cannot fit (e.g. a 19k system prompt vs a 12k
	// model), fail fast instead of sending a doomed request.
	if maxTokens > 0 && ctxUsed > maxTokens {
		err := fmt.Errorf("model %q advertises a ~%d-token window but this turn needs ~%d tokens: /compact history or /model to a larger window", modelName, maxTokens, ctxUsed)
		finishTurn()
		return "", err
	}
	text2, calls, usage, err := completeStream(ctx, mustProvider(r, sess), llm.Request{
		Model: sess.Model, Messages: msgs, Tools: specs,
		Images: in.Images, ReasoningEffort: in.Effort,
	}, stream)
	noteUsage(usage)
	if err != nil {
		finishTurn()
		return "", err
	}
	// Images ride the first request only; follow-up rounds are text.
	in.Images = nil
	// Agentic loop: feed tool results back until the model stops calling
	// tools (cap rounds so a confused model can't spin forever).
	const maxRounds = 8
	for round := 0; round < maxRounds && len(calls) > 0; round++ {
		var feedback strings.Builder
		for _, c := range calls {
			if r.tools == nil {
				break
			}
			sc := tools.SecCtx{
				Agent: security.AgentMode(sess.Agent), Permission: security.PermissionMode(sess.PermissionMode),
				WorkspaceDir: sess.WorkspaceDir, SessionID: sessionID,
			}
			r.progress(sessionID, "tool: "+c.Name)
			res, terr := r.callWithApproval(ctx, sessionID, sc, c.Name, c.Args)
			if terr != nil {
				_, _ = r.store.AddTurn(sessionID, "", "tool_error", fmt.Sprintf("%s: %v", c.Name, terr))
				fmt.Fprintf(&feedback, "Tool %s error:\n%v\n\n", c.Name, terr)
				continue
			}
			_, _ = r.store.AddTurn(sessionID, "", "tool", c.Name+": "+res.Output)
			fmt.Fprintf(&feedback, "Tool %s result:\n%s\n\n", c.Name, res.Output)
		}
		if err := ctx.Err(); err != nil {
			finishTurn()
			return "", err
		}
		userMsg := "Tool results:\n" + feedback.String() + "Continue the task; call more tools if needed, else give the final answer."
		if s := r.takeSteered(sessionID); s != "" {
			// Steered input joins the active turn.
			_, _ = r.store.AddTurn(sessionID, "", "user", s)
			r.progress(sessionID, "steered ✓")
			userMsg += "\n\nSteered input (answer this too):\n" + s
		}
		// Vault boundary: tool outputs and steered text may carry values.
		userMsg, err = r.store.substituteSecrets(sessionID, userMsg)
		if err != nil {
			finishTurn()
			return "", err
		}
		text2sub, err := r.store.substituteSecrets(sessionID, text2)
		if err != nil {
			finishTurn()
			return "", err
		}
		msgs = append(msgs,
			llm.Message{Role: "assistant", Content: text2sub},
			llm.Message{Role: "user", Content: userMsg},
		)
		// Re-compact every round: tool feedback can balloon past the window
		// mid-loop. Newest-first eviction keeps the latest exchange alive
		// (guard below always preserves system + the last round).
		msgs = compactPrompt(msgs, specs, maxTokens)
		ctxUsed = EstimateContextTokens(msgs, specs)
		r.emitContext(sessionID, ctxUsed, maxTokens)
		text2, calls, usage, err = completeStream(ctx, mustProvider(r, sess), llm.Request{
			Model: sess.Model, Messages: msgs, Tools: specs,
			Images: in.Images, ReasoningEffort: in.Effort,
		}, stream)
		noteUsage(usage)
		if err != nil {
			finishTurn()
			return "", err
		}
	}
	// Step-cap: on the final step the tools are removed and
	// the model is asked to summarize in text, so a turn that hits the
	// round cap still ends with a substantive reply gathering all results.
	if len(calls) > 0 {
		r.progress(sessionID, "summarizing…")
		text2sub, serr := r.store.substituteSecrets(sessionID, text2)
		if serr != nil {
			finishTurn()
			return "", serr
		}
		summary, _, _, serr := completeStream(ctx, mustProvider(r, sess), llm.Request{
			Model: sess.Model,
			Messages: append(msgs,
				llm.Message{Role: "assistant", Content: text2sub},
				llm.Message{Role: "user", Content: "Tool budget reached. Summarize in text: what was accomplished, what each tool found, and what is still running. Do not call tools."},
			),
		}, stream)
		if serr == nil && strings.TrimSpace(summary) != "" {
			text2 = summary
		}
	}
	// Leftover steer (arrived after the last round) chains as a queued
	// turn so it is answered next instead of sitting in the transcript.
	if rest := r.takeSteered(sessionID); rest != "" {
		r.Queue(sessionID, TurnInput{Text: rest})
	}
	// User transparency: the stored transcript and returned text carry
	// values; refs only ever faced the provider.
	text2, err = r.store.resolveRefs(sessionID, text2)
	if err != nil {
		finishTurn()
		return "", err
	}
	assistantTurn, err := r.store.AddTurn(sessionID, "", "assistant", text2)
	if err != nil {
		finishTurn()
		return "", err
	}
	finishTurn()
	// Best-effort per-turn checkpoint; failures must not fail the turn.
	_, _ = Snapshot(sess.WorkspaceDir, sessionID, assistantTurn.ID)
	return text2, nil
}

func (r *Runner) progress(sessionID, stage string) {
	if r.OnProgress != nil && sessionID != "" && stage != "" {
		r.OnProgress(sessionID, stage)
	}
}

// emitContext publishes the estimated context gauge (used vs window;
// max 0 = provider does not advertise one, shown as used-only).
func (r *Runner) emitContext(sessionID string, used, max int) {
	if r.OnContext != nil && sessionID != "" && used > 0 {
		r.OnContext(sessionID, used, max)
	}
}

// deltaFunc adapts OnDelta to a per-chunk callback bound to one session.
func (r *Runner) deltaFunc(sessionID string) func(delta, thinking string) {
	if r.OnDelta == nil {
		return nil
	}
	return func(delta, thinking string) {
		r.OnDelta(sessionID, delta, thinking)
	}
}

// toolPreview condenses tool output for live display (300 runes, single line).
func toolPreview(out string) string {
	out = strings.ReplaceAll(out, "\n", " ")
	r := []rune(out)
	if len(r) > 300 {
		return string(r[:300]) + "..."
	}
	return out
}

// execTool runs one approved tool call bracketed by OnToolStart/OnToolDone.
func (r *Runner) execTool(ctx context.Context, sessionID string, sc tools.SecCtx, name string, args map[string]interface{}, target string) (tools.Result, error) {
	start := time.Now()
	if r.OnToolStart != nil {
		r.OnToolStart(sessionID, name, target)
	}
	res, err := r.tools.Call(ctx, sc, name, args)
	if r.OnToolDone != nil {
		preview := toolPreview(res.Output)
		if err != nil {
			preview = toolPreview(err.Error())
		}
		r.OnToolDone(sessionID, name, target, preview, err != nil, time.Since(start).Milliseconds())
	}
	return res, err
}

// grantKey scopes an allow_session grant to one session + tool target.
func grantKey(sessionID, tool, target string) string {
	return sessionID + "|" + tool + "|" + target
}

func (r *Runner) isGranted(sessionID, tool, target string) bool {
	r.mu.Lock()
	defer r.mu.Unlock()
	if r.granted == nil {
		return false
	}
	return r.granted[grantKey(sessionID, tool, target)]
}

func (r *Runner) setGranted(sessionID, tool, target string) {
	r.mu.Lock()
	defer r.mu.Unlock()
	if r.granted == nil {
		r.granted = map[string]bool{}
	}
	r.granted[grantKey(sessionID, tool, target)] = true
}

// callWithApproval runs one tool, pausing for UI approval when the broker
// verdict is Ask. Deny (credentials, plan fence, explicit deny rules) never
// prompts — it fails fast like before. An approved call retries once with a
// single-shot bypass envelope; the plan structural fence still holds inside
// Authorize because the agent mode is unchanged.
func (r *Runner) callWithApproval(ctx context.Context, sessionID string, sc tools.SecCtx, name string, args map[string]interface{}) (tools.Result, error) {
	// Resolve direction: the model only ever saw refs, so its args carry
	// them. Resolve before the broker prompt (user sees values) and before
	// execution (tools get values). Fail closed on vault errors.
	if r.store != nil {
		rv, err := r.store.resolveArgRefs(sessionID, args)
		if err != nil {
			return tools.Result{}, err
		}
		if m, ok := rv.(map[string]interface{}); ok {
			args = m
		}
	}
	target := approvalTarget(name, args)
	d := security.Authorize(sc.Agent, sc.Permission, name, target, sc.WorkspaceDir, nil, false)
	if d == security.Allow {
		return r.execTool(ctx, sessionID, sc, name, args, target)
	}
	if d == security.Deny || r.AskPermission == nil {
		// Legacy path: surface the broker verdict as a tool error.
		return r.execTool(ctx, sessionID, sc, name, args, target)
	}
	// Ask: honor an earlier allow_session grant without re-prompting.
	if r.isGranted(sessionID, name, target) {
		sc.Permission = security.PermBypass
		return r.execTool(ctx, sessionID, sc, name, args, target)
	}
	r.progress(sessionID, "approval: "+name+" "+target)
	decision, err := r.AskPermission(ctx, sessionID, name, target)
	if err != nil {
		return tools.Result{}, err
	}
	switch decision {
	case "allow_once", "allow", "allow_always":
		sc.Permission = security.PermBypass
		return r.execTool(ctx, sessionID, sc, name, args, target)
	case "allow_session":
		r.setGranted(sessionID, name, target)
		sc.Permission = security.PermBypass
		return r.execTool(ctx, sessionID, sc, name, args, target)
	default:
		return tools.Result{}, fmt.Errorf("tool %q target %q: denied by user (permission_mode=%s agent=%s)", name, target, sc.Permission, sc.Agent)
	}
}

// approvalTarget mirrors tools.toolTarget (unexported there) so the broker
// prompt shows the same command/path the gate evaluated.
func approvalTarget(name string, args map[string]interface{}) string {
	switch strings.ToLower(name) {
	case "bash", "shell":
		if s, _ := args["command"].(string); s != "" {
			return s
		}
	case "webfetch":
		if s, _ := args["url"].(string); s != "" {
			return s
		}
	case "websearch":
		if s, _ := args["query"].(string); s != "" {
			return s
		}
	case "subagent":
		if s, _ := args["agent"].(string); s != "" {
			return s
		}
	case "task_output":
		if s, _ := args["task_id"].(string); s != "" {
			return "task:" + s
		}
	}
	if s, _ := args["path"].(string); s != "" {
		return s
	}
	if s, _ := args["file"].(string); s != "" {
		return s
	}
	return name
}

// EstimateContextTokens approximates prompt tokens as ~4 chars/token over
// roles + contents + thinking, plus a per-tool schema allowance.
// ponytail: deliberately dependency-free; providers that report real
// usage still win for in/out, this only feeds the context gauge.
func EstimateContextTokens(msgs []llm.Message, specs []llm.ToolSpec) int {
	total := len(specs) * 220
	for _, m := range msgs {
		total += len(m.Role) + len(m.Content) + len(m.Thinking)
	}
	return total/4 + 1
}

// FormatTokens renders 12345 as "12.3k".
func FormatTokens(n int) string {
	if n < 1000 {
		return fmt.Sprintf("%d", n)
	}
	return fmt.Sprintf("%.1fk", float64(n)/1000)
}

// TurnStats summarizes a finished turn for footers and turn summaries
// (agent, model, duration; tok/s when usage is reported).
type TurnStats struct {
	Model        string
	DurationMs   int64
	InputTokens  int
	OutputTokens int
	// ContextTokens is the estimated prompt size (chars/4 heuristic) and
	// MaxTokens the provider-advertised window (0 = provider does not
	// report one; the gauge then shows used-only).
	ContextTokens int
	MaxTokens     int
}

// TokPerSec returns output tokens/sec, or 0 when usage is unknown.
func (s TurnStats) TokPerSec() float64 {
	if s.OutputTokens <= 0 || s.DurationMs <= 0 {
		return 0
	}
	return float64(s.OutputTokens) / (float64(s.DurationMs) / 1000)
}

// Summary renders the turn footer: model · duration · tok/s,
// omitting whatever is unknown. Context usage appends as ctx ~used/max,
// or ctx ~used when the provider advertises no window.
func (s TurnStats) Summary() string {
	var parts []string
	if s.Model != "" {
		parts = append(parts, s.Model)
	}
	if s.DurationMs > 0 {
		parts = append(parts, FormatDuration(s.DurationMs))
	}
	if tps := s.TokPerSec(); tps > 0 {
		parts = append(parts, fmt.Sprintf("%.0f tok/s", tps))
	}
	if s.ContextTokens > 0 {
		if s.MaxTokens > 0 {
			parts = append(parts, fmt.Sprintf("ctx ~%s/%s", FormatTokens(s.ContextTokens), FormatTokens(s.MaxTokens)))
		} else {
			parts = append(parts, fmt.Sprintf("ctx ~%s", FormatTokens(s.ContextTokens)))
		}
	}
	return strings.Join(parts, " · ")
}

// FormatDuration renders milliseconds as short durations.
func FormatDuration(ms int64) string {
	if ms < 1000 {
		return fmt.Sprintf("%dms", ms)
	}
	if ms < 60000 {
		return fmt.Sprintf("%.1fs", float64(ms)/1000)
	}
	return fmt.Sprintf("%dm%02ds", ms/60000, (ms%60000)/1000)
}

// completeStream collects one model round while forwarding every chunk
// to onChunk (nil = collect silently, like llm.Complete). It also returns
// the last usage report (nil when the provider sends none).
func completeStream(ctx context.Context, p llm.Provider, req llm.Request, onChunk func(delta, thinking string)) (string, []llm.ToolCallRequest, *llm.Usage, error) {
	ch, err := p.Chat(ctx, req)
	if err != nil {
		return "", nil, nil, err
	}
	var sb strings.Builder
	var calls []llm.ToolCallRequest
	var usage *llm.Usage
	for c := range ch {
		sb.WriteString(c.Delta)
		if onChunk != nil && (c.Delta != "" || c.Thinking != "") {
			onChunk(c.Delta, c.Thinking)
		}
		calls = append(calls, c.ToolCalls...)
		if c.Usage != nil {
			usage = c.Usage
		}
		if c.Done {
			break
		}
		select {
		case <-ctx.Done():
			return sb.String(), calls, usage, ctx.Err()
		default:
		}
	}
	return sb.String(), calls, usage, nil
}

func mustProvider(r *Runner, sess *Session) llm.Provider {
	model := sess.Model
	if model == "" && r.router != nil {
		model = r.router.ModelFor("executor", sess.Agent)
	}
	if model == "" {
		return cannedProvider{text: "(no model configured; turn recorded without model call)"}
	}
	p, mid := r.router.ProviderFor(model)
	if p == nil {
		return cannedProvider{text: "(no provider for " + model + "; turn recorded without model call)"}
	}
	return &modelBound{Provider: p, model: mid}
}

type modelBound struct {
	llm.Provider
	model string
}

// Chat rewrites the fully-qualified "provider/model" into the bare model id.
func (m *modelBound) Chat(ctx context.Context, req llm.Request) (<-chan llm.Chunk, error) {
	req.Model = m.model
	return m.Provider.Chat(ctx, req)
}

// cannedProvider yields a single canned reply (offline / unknown provider).
type cannedProvider struct{ text string }

func (c cannedProvider) Name() string { return "canned" }
func (c cannedProvider) Chat(_ context.Context, _ llm.Request) (<-chan llm.Chunk, error) {
	out := make(chan llm.Chunk, 1)
	out <- llm.Chunk{Delta: c.text, Done: true}
	close(out)
	return out, nil
}

func toolSpecs() []llm.ToolSpec {
	str := map[string]interface{}{"type": "string"}
	boolT := map[string]interface{}{"type": "boolean"}
	obj := func(props map[string]interface{}, req ...string) map[string]interface{} {
		return map[string]interface{}{"type": "object", "properties": props, "required": req}
	}
	return []llm.ToolSpec{
		{Name: "read", Description: "Read a file or directory. Text lines return as 1-based line: content (line numbers for reference only, never include them in edits). Use offset/limit for large files; prefer one larger read over many small slices, and use grep to find content in large files.",
			Schema: obj(map[string]interface{}{"path": str, "offset": map[string]interface{}{"type": "integer"}, "limit": map[string]interface{}{"type": "integer"}}, "path")},
		{Name: "edit", Description: "Edit a file by finding and replacing exact text. Preserves indentation; omit read line-number prefixes in old_string/new_string. old_string must be unique unless replaceAll is true.",
			Schema: obj(map[string]interface{}{"path": str, "old_string": str, "new_string": str, "replaceAll": boolT}, "path", "old_string", "new_string")},
		{Name: "write", Description: "Write a file to the filesystem, overwriting if one exists. Missing parent directories are created automatically. Use this to create new files or overwrite existing files. For partial changes, use edit instead.",
			Schema: obj(map[string]interface{}{"path": str, "content": str}, "path", "content")},
		{Name: "patch", Description: "Apply a *** Begin Patch *** multi-file diff (*** Add/Update/Delete File *** with @@ hunks). Use for multi-file or diff-style edits; prefer edit for single targeted changes.",
			Schema: obj(map[string]interface{}{"patchText": str}, "patchText")},
		{Name: "bash", Description: "Execute a shell command and return its output. Prefer dedicated tools over shell commands when possible. Set workdir to run in a subdirectory; background:true runs detached; poll with task_output.",
			Schema: obj(map[string]interface{}{"command": str, "workdir": str, "background": boolT, "timeout": map[string]interface{}{"type": "integer"}}, "command")},
		{Name: "grep", Description: "Search file contents using regex or literal text. Use it to locate code, symbols, or text; narrow with path or include. Returns file paths, line numbers, and previews. Skips credential files.",
			Schema: obj(map[string]interface{}{"pattern": str, "path": str, "include": str, "literal": boolT, "caseSensitive": boolT, "limit": map[string]interface{}{"type": "integer"}}, "pattern")},
		{Name: "glob", Description: "Search file paths using a glob pattern (examples: \"**/*.ts\", \"src/**/*.tsx\").",
			Schema: obj(map[string]interface{}{"pattern": str, "path": str, "limit": map[string]interface{}{"type": "integer"}}, "pattern")},
		{Name: "lsp", Description: "Code intelligence: symbols/refs (grep-backed)",
			Schema: obj(map[string]interface{}{"pattern": str, "path": str}, "pattern")},
		{Name: "skill", Description: "Load a specialized skill's instructions into the conversation when the task matches its description.",
			Schema: obj(map[string]interface{}{"name": str}, "name")},
		{Name: "kb_query", Description: "Query Markdown+graphify memory",
			Schema: obj(map[string]interface{}{"query": str, "budget": map[string]interface{}{"type": "integer"}}, "query")},
		{Name: "memory_write", Description: "Append an atomic note to ./memory/<topic>.md",
			Schema: obj(map[string]interface{}{"topic": str, "content": str}, "topic", "content")},
		{Name: "mcp", Description: "Call a local MCP server tool (see .lumina/mcp.json)",
			Schema: obj(map[string]interface{}{"server": str, "tool": str}, "server", "tool")},
		{Name: "webfetch", Description: "Fetch an HTTP(S) URL as markdown (default), text, or HTML. Read-only; large results truncate past the cap. Use for direct URLs or public JSON APIs.",
			Schema: obj(map[string]interface{}{"url": str, "format": str}, "url")},
		{Name: "websearch", Description: fmt.Sprintf("Search the web for current info beyond knowledge cutoff (needs a provider key). The current year is %d. Use this year when searching for recent information.", time.Now().Year()),
			Schema: obj(map[string]interface{}{"query": str}, "query")},
		{Name: "question", Description: "Ask the user questions during execution to gather preferences, clarify ambiguity, or decide direction. Answers return inline; continue with them in mind.",
			Schema: obj(map[string]interface{}{"questions": map[string]interface{}{"type": "array"}}, "questions")},
		{Name: "todowrite", Description: "Track task progress as a todo list. Send the full updated list on each call.",
			Schema: obj(map[string]interface{}{"todos": map[string]interface{}{"type": "array"}}, "todos")},
		{Name: "subagent", Description: "Launch a scoped subagent child (general/explore/custom); optional model override (provider/model). background:true detaches",
			Schema: obj(map[string]interface{}{"agent": str, "description": str, "prompt": str, "model": str, "background": boolT}, "agent", "prompt")},
		{Name: "task_output", Description: "Poll a background bash/subagent task by ID",
			Schema: obj(map[string]interface{}{"task_id": str}, "task_id")},
	}
}

// RunSubagent runs an isolated helper turn with a restricted tool set and
// returns its final text without polluting the parent turn tree.
func (r *Runner) RunSubagent(ctx context.Context, sessionID, task string, allowedTools []string) (string, error) {
	sess, err := r.store.GetSession(sessionID)
	if err != nil {
		return "", err
	}
	if r.router == nil {
		return "(no model configured)", nil
	}
	allow := map[string]bool{}
	for _, t := range allowedTools {
		allow[strings.ToLower(t)] = true
	}
	var specs []llm.ToolSpec
	for _, s := range toolSpecs() {
		if s.Name == "subagent" {
			continue // depth 1: children cannot delegate further
		}
		if allow[strings.ToLower(s.Name)] {
			specs = append(specs, s)
		}
	}
	childSc := tools.SecCtx{
		Agent: security.AgentMode(sess.Agent), Permission: security.PermissionMode(sess.PermissionMode),
		WorkspaceDir: sess.WorkspaceDir, SessionID: sessionID, SubagentDepth: 1,
	}
	return r.runChildLoop(ctx, childSc, sess.Model, "You are a scoped subagent. Use only the provided tools.", task, specs)
}

// runSubagentText is the tools.Registry.Subagent backend: it resolves a
// configured subagent (general/explore/custom, mode subagent|all), runs a
// full agentic tool loop with its restricted tool set, and returns the final
// text. The child inherits the parent's workspace, model default, and
// permission mode; nesting is capped by the registry's max depth. An explicit
// model override (provider/model) wins over the agent default.
func (r *Runner) runSubagentText(ctx context.Context, sc tools.SecCtx, req tools.SubagentRequest) (string, error) {
	if r.router == nil {
		return "", fmt.Errorf("subagent: no model backend configured")
	}
	if r.tools == nil {
		return "", fmt.Errorf("subagent: no tool registry")
	}
	bundle := r.rulesFn(sc.WorkspaceDir)
	def, ok := findAgent(bundle, req.Agent)
	if !ok {
		return "", fmt.Errorf("subagent: unknown agent %q (available: %s)", req.Agent, agentNames(bundle))
	}
	if mode := strings.ToLower(def.Mode); mode != "subagent" && mode != "all" {
		return "", fmt.Errorf("subagent: agent %q is primary-only (mode=%q)", req.Agent, def.Mode)
	}
	model := req.Model
	if model == "" {
		model = def.Model
	}
	if model == "" {
		if sess, err := r.store.GetSession(sc.SessionID); err == nil {
			model = sess.Model
			if model == "" {
				model = r.router.ModelFor("executor", sess.Agent)
			}
		} else {
			model = r.router.ModelFor("executor", "build")
		}
	} else if prov, _ := r.router.ProviderFor(model); prov == nil {
		return "", fmt.Errorf("subagent: no provider for model %q (use provider/model)", model)
	}
	// Guard the child context window: a model-drafted delegation prompt can
	// balloon (pasted transcripts, dumps). Truncate with a note instead of
	// letting one call blow the turn's token budget.
	prompt := req.Prompt
	if len(prompt) > 6000 {
		prompt = prompt[:6000] + "\n…[delegation prompt truncated at 6000 chars]"
	}
	description := req.Description
	if len(description) > 2000 {
		description = description[:2000] + "…[truncated]"
	}
	specs := subagentSpecs(def.Name)
	system := "You are the " + def.Name + " subagent. " + def.Body
	if description != "" {
		system += "\n\nTask context: " + description
	}
	system += "\n\nScope: use only the provided tools; you cannot launch subagents. When done, summarize findings with file references."
	system += "\n\n" + toolGuidanceBase + "\n" + renderToolGuidance(specs)
	r.progress(sc.SessionID, "subagent: "+def.Name)
	childSc := sc
	childSc.SubagentDepth = sc.SubagentDepth + 1
	return r.runChildLoop(ctx, childSc, model, system, prompt, specs)
}

// findAgent resolves a subagent by name against custom agents plus builtins.
func findAgent(bundle *rules.Bundle, name string) (rules.AgentDef, bool) {
	want := strings.ToLower(strings.TrimSpace(name))
	for _, a := range bundle.Agents {
		if strings.ToLower(a.Name) == want {
			return a, true
		}
	}
	for _, a := range rules.BuiltinAgents() {
		if strings.ToLower(a.Name) == want {
			return a, true
		}
	}
	return rules.AgentDef{}, false
}

func agentNames(bundle *rules.Bundle) string {
	seen := map[string]bool{}
	var out []string
	for _, a := range bundle.Agents {
		if !seen[strings.ToLower(a.Name)] {
			seen[strings.ToLower(a.Name)] = true
			out = append(out, a.Name)
		}
	}
	for _, a := range rules.BuiltinAgents() {
		if !seen[strings.ToLower(a.Name)] {
			seen[strings.ToLower(a.Name)] = true
			out = append(out, a.Name)
		}
	}
	if len(out) == 0 {
		return "(none)"
	}
	return strings.Join(out, ", ")
}

// subagentSpecs restricts the child tool envelope:
// explore is read-only research incl. web; general gets everything but
// subagent delegation; custom agents default to the general envelope.
func subagentSpecs(agentName string) []llm.ToolSpec {
	var allow map[string]bool
	if strings.ToLower(agentName) == "explore" {
		allow = map[string]bool{
			"read": true, "glob": true, "grep": true, "lsp": true,
			"skill": true, "kb_query": true, "webfetch": true,
			"websearch": true, "bash": true, "task_output": true,
		}
	}
	var specs []llm.ToolSpec
	for _, sp := range toolSpecs() {
		if sp.Name == "subagent" {
			continue
		}
		if allow != nil && !allow[sp.Name] {
			continue
		}
		specs = append(specs, sp)
	}
	return specs
}

// withoutTool drops one tool from a spec list (websearch:false parity).
func withoutTool(specs []llm.ToolSpec, name string) []llm.ToolSpec {
	out := specs[:0]
	for _, sp := range specs {
		if sp.Name == name {
			continue
		}
		out = append(out, sp)
	}
	return out
}

// runChildLoop runs a bounded agentic tool loop for a subagent without
// touching the parent turn tree.
func (r *Runner) runChildLoop(ctx context.Context, sc tools.SecCtx, model, system, task string, specs []llm.ToolSpec) (string, error) {
	if r.tools != nil && r.tools.WebSearchDisabled {
		specs = withoutTool(specs, "websearch")
	}
	prov, mid := r.router.ProviderFor(model)
	if prov == nil {
		return "", fmt.Errorf("subagent: no provider for model %q", model)
	}
	p := &modelBound{Provider: prov, model: mid}
	vaultedTask, err := r.store.substituteSecrets(sc.SessionID, task)
	if err != nil {
		return "", err
	}
	msgs := []llm.Message{
		{Role: "system", Content: system + "\n\n- workspace_dir: " + sc.WorkspaceDir + "\n\n" + vaultGuidance},
		{Role: "user", Content: vaultedTask},
	}
	stream := r.deltaFunc(sc.SessionID)
	text, calls, _, err := completeStream(ctx, p, llm.Request{Model: mid, Messages: msgs, Tools: specs}, stream)
	if err != nil {
		return "", err
	}
	const maxRounds = 8
	for round := 0; round < maxRounds && len(calls) > 0; round++ {
		var feedback strings.Builder
		for _, c := range calls {
			if r.tools == nil {
				break
			}
			r.progress(sc.SessionID, "tool: "+c.Name)
			resolvedArgs := c.Args
			if r.store != nil {
				rv, rerr := r.store.resolveArgRefs(sc.SessionID, c.Args)
				if rerr != nil {
					return "", rerr
				}
				if m, ok := rv.(map[string]interface{}); ok {
					resolvedArgs = m
				}
			}
			res, terr := r.execTool(ctx, sc.SessionID, sc, c.Name, resolvedArgs, approvalTarget(c.Name, resolvedArgs))
			if terr != nil {
				fmt.Fprintf(&feedback, "Tool %s error:\n%v\n\n", c.Name, terr)
				continue
			}
			fmt.Fprintf(&feedback, "Tool %s result:\n%s\n\n", c.Name, res.Output)
		}
		if err := ctx.Err(); err != nil {
			return "", err
		}
		amsg, serr := r.store.substituteSecrets(sc.SessionID, text)
		if serr != nil {
			return "", serr
		}
		umsg, serr := r.store.substituteSecrets(sc.SessionID, "Tool results:\n"+feedback.String()+"Continue the task; call more tools if needed, else give the final answer.")
		if serr != nil {
			return "", serr
		}
		msgs = append(msgs,
			llm.Message{Role: "assistant", Content: amsg},
			llm.Message{Role: "user", Content: umsg},
		)
		text, calls, _, err = completeStream(ctx, p, llm.Request{Model: mid, Messages: msgs, Tools: specs}, stream)
		if err != nil {
			return "", err
		}
	}
	// User transparency: resolve refs before the text leaves the child.
	return r.store.resolveRefs(sc.SessionID, text)
}

// Compact summarizes stale tool-output turns, keeping rules, diffs, and
// memory pointers intact. Pure string surgery — zero LLM tokens.
// (Offline/persisted-history API; the live per-prompt budget is
// compactPrompt below, which RunPrompt applies every turn and every round.)
func Compact(turns []*Turn, keepRecent int) []*Turn {
	if len(turns) <= keepRecent {
		return turns
	}
	cut := len(turns) - keepRecent
	var kept []*Turn
	kept = append(kept, &Turn{Role: "system", Text: fmt.Sprintf("[compacted %d earlier turns; rules, diffs, and ./memory pointers preserved]", cut)})
	for _, t := range turns[cut:] {
		if t.Role == "tool" && len(t.Text) > 2000 {
			cp := *t
			cp.Text = t.Text[:2000] + "\n…[truncated by /compact]"
			kept = append(kept, &cp)
			continue
		}
		kept = append(kept, t)
	}
	return kept
}

// compactPrompt bounds one prompt's history to the model's window: the
// newest turns under 80% of the provider-advertised window, or the newest
// 40 messages when the provider reports none (legacy ceiling). The system
// prompt (msgs[0]) is never dropped; callers fail fast when even it exceeds
// the window (see the RunPrompt pre-flight). Newest-first eviction is
// deliberate: the previous oldest-first cap silently dropped the current
// user prompt once history passed 40 messages.
func compactPrompt(msgs []llm.Message, specs []llm.ToolSpec, window int) []llm.Message {
	if len(msgs) <= 1 {
		return msgs
	}
	if window > 0 {
		budget := window * 8 / 10
		for len(msgs) > 3 && EstimateContextTokens(msgs, specs) > budget {
			msgs = append(msgs[:1], msgs[2:]...) // drop oldest turn
		}
		return msgs
	}
	if len(msgs) > 41 {
		return append(msgs[:1], msgs[len(msgs)-40:]...)
	}
	return msgs
}
