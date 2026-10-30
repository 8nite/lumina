package agent

import (
	"context"
	"fmt"
	"strings"

	"github.com/8nite/lumina/internal/llm"
	"github.com/8nite/lumina/internal/rules"
	"github.com/8nite/lumina/internal/security"
	"github.com/8nite/lumina/internal/tools"
)

// Runner executes the turn loop: system prompt assembly, model call,
// security-gated tool execution, checkpointing, and compaction.
type Runner struct {
	store   *Store
	router  *llm.Router
	tools   *tools.Registry
	rulesFn func(workspace string) *rules.Bundle
}

// NewRunner wires dependencies (router/tools may be nil in storage-only use).
func NewRunner(store *Store, router *llm.Router, reg *tools.Registry) *Runner {
	return &Runner{store: store, router: router, tools: reg,
		rulesFn: func(ws string) *rules.Bundle {
			return rules.Load(rules.Options{WorkspaceDir: ws, ImportGlobalRules: true, ImportGlobalSkills: true})
		}}
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
	turns, _ := r.store.ListTurns(sessionID)
	msgs := []llm.Message{{
		Role: "system",
		Content: bundle.SystemPrompt + "\n\n# Session\n" +
			"- workspace_dir: " + sess.WorkspaceDir + "\n" +
			"- agent: " + sess.Agent + " (plan=read-only except plan.md/.lumina/plans; build=full)\n" +
			"- permission_mode: " + sess.PermissionMode + "\n" +
			"- Use tools for workspace actions (read/edit/write/bash); batch independent calls in one block.\n" +
			"- After tool results arrive, continue the task; reply substantively only when done.\n",
	}}
	for _, t := range turns {
		if len(msgs) > 40 {
			break // compaction window; Compact() shrinks older turns offline
		}
		if t.Role == "user" || t.Role == "assistant" {
			msgs = append(msgs, llm.Message{Role: t.Role, Content: t.Text})
		}
	}
	text2, calls, err := llm.Complete(ctx, mustProvider(r, sess), llm.Request{
		Model: sess.Model, Messages: msgs, Tools: toolSpecs(),
		Images: in.Images, ReasoningEffort: in.Effort,
	})
	if err != nil {
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
				WorkspaceDir: sess.WorkspaceDir,
			}
			res, terr := r.tools.Call(ctx, sc, c.Name, c.Args)
			if terr != nil {
				_, _ = r.store.AddTurn(sessionID, "", "tool_error", fmt.Sprintf("%s: %v", c.Name, terr))
				fmt.Fprintf(&feedback, "Tool %s error:\n%v\n\n", c.Name, terr)
				continue
			}
			_, _ = r.store.AddTurn(sessionID, "", "tool", c.Name+": "+res.Output)
			fmt.Fprintf(&feedback, "Tool %s result:\n%s\n\n", c.Name, res.Output)
		}
		if err := ctx.Err(); err != nil {
			return "", err
		}
		msgs = append(msgs,
			llm.Message{Role: "assistant", Content: text2},
			llm.Message{Role: "user", Content: "Tool results:\n" + feedback.String() + "Continue the task; call more tools if needed, else give the final answer."},
		)
		text2, calls, err = llm.Complete(ctx, mustProvider(r, sess), llm.Request{
			Model: sess.Model, Messages: msgs, Tools: toolSpecs(),
			Images: in.Images, ReasoningEffort: in.Effort,
		})
		if err != nil {
			return "", err
		}
	}
	assistantTurn, err := r.store.AddTurn(sessionID, "", "assistant", text2)
	if err != nil {
		return "", err
	}
	// Best-effort per-turn checkpoint; failures must not fail the turn.
	_, _ = Snapshot(sess.WorkspaceDir, sessionID, assistantTurn.ID)
	return text2, nil
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
	obj := func(props map[string]interface{}, req ...string) map[string]interface{} {
		return map[string]interface{}{"type": "object", "properties": props, "required": req}
	}
	return []llm.ToolSpec{
		{Name: "read", Description: "Read a workspace file (offset/limit lines)",
			Schema: obj(map[string]interface{}{"path": str, "offset": map[string]interface{}{"type": "integer"}, "limit": map[string]interface{}{"type": "integer"}}, "path")},
		{Name: "edit", Description: "Replace exact old_string with new_string in a file",
			Schema: obj(map[string]interface{}{"path": str, "old_string": str, "new_string": str}, "path", "old_string", "new_string")},
		{Name: "write", Description: "Create or overwrite a file with content",
			Schema: obj(map[string]interface{}{"path": str, "content": str}, "path", "content")},
		{Name: "bash", Description: "Run a shell command in the workspace",
			Schema: obj(map[string]interface{}{"command": str}, "command")},
		{Name: "grep", Description: "Search file contents (skips credential files)",
			Schema: obj(map[string]interface{}{"pattern": str, "path": str}, "pattern")},
		{Name: "glob", Description: "Find files by glob pattern",
			Schema: obj(map[string]interface{}{"pattern": str}, "pattern")},
		{Name: "lsp", Description: "Code intelligence: symbols/refs (grep-backed)",
			Schema: obj(map[string]interface{}{"pattern": str, "path": str}, "pattern")},
		{Name: "skill", Description: "Load a SKILL.md playbook body by name",
			Schema: obj(map[string]interface{}{"name": str}, "name")},
		{Name: "kb_query", Description: "Query Markdown+graphify memory",
			Schema: obj(map[string]interface{}{"query": str, "budget": map[string]interface{}{"type": "integer"}}, "query")},
		{Name: "memory_write", Description: "Append an atomic note to ./memory/<topic>.md",
			Schema: obj(map[string]interface{}{"topic": str, "content": str}, "topic", "content")},
		{Name: "mcp", Description: "Call a local MCP server tool (see .lumina/mcp.json)",
			Schema: obj(map[string]interface{}{"server": str, "tool": str}, "server", "tool")},
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
		if allow[strings.ToLower(s.Name)] {
			specs = append(specs, s)
		}
	}
	text, _, err := llm.Complete(ctx, mustProvider(r, sess), llm.Request{
		Model: sess.Model,
		Messages: []llm.Message{
			{Role: "system", Content: "You are a scoped subagent. Use only the provided tools."},
			{Role: "user", Content: task},
		},
		Tools: specs,
	})
	return text, err
}

// Compact summarizes stale tool-output turns, keeping rules, diffs, and
// memory pointers intact. Pure string surgery — zero LLM tokens.
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
