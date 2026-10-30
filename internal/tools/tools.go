// Package tools implements the agent tool envelope: read, edit, write,
// bash, grep, glob, skill, lsp, mcp, kb_query, memory_write, webfetch,
// websearch, subagent, task_output — every call gated by
// internal/security Authorize(). Web + subagent semantics follow
// https://opencode.ai/v2/docs/tools, /websearch, and /agents (reimplemented,
// no opencode sources copied).
package tools

import (
	"bufio"
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"os"
	"os/exec"
	"path/filepath"
	"sort"
	"strings"
	"sync"
	"time"

	"github.com/8nite/lumina/internal/mcp"
	"github.com/8nite/lumina/internal/security"
)

// SecCtx carries the calling session's structural + approval state.
type SecCtx struct {
	Agent             AgentMode
	Permission        PermissionMode
	WorkspaceDir      string
	BashRules         map[string]string
	AllowToolingWrite bool
	// SubagentDepth counts delegation nesting (opencode caps at 1: a
	// subagent may not launch its own subagents).
	SubagentDepth int
	// SessionID routes background-done notifications to the owning session.
	SessionID string
}

// Task is one background unit (bash background:true or subagent
// background:true). Foreground callers get the result inline; background
// callers poll it via task_output. Full output is retained in memory.
type Task struct {
	ID        string
	Kind      string // "bash" | "subagent"
	Label     string
	Status    string // "running" | "done" | "error"
	Output    string
	StartedAt time.Time
	DoneAt    time.Time
}

// AgentMode / PermissionMode mirror security types without an import cycle
// in godoc; they are type aliases kept in sync by the compiler.
type AgentMode = security.AgentMode
type PermissionMode = security.PermissionMode

// Result is one tool outcome.
type Result struct {
	Output    string `json:"output"`
	Truncated bool   `json:"truncated,omitempty"`
}

// Tool is a single callable.
type Tool struct {
	Name        string
	Description string
	Execute     func(ctx context.Context, sc SecCtx, args map[string]interface{}) (Result, error)
}

// Registry holds all tools for prompt-schema generation.
type Registry struct {
	tools map[string]*Tool
	// KB and Memory backends are injected to avoid import cycles.
	KBQuery func(ctx context.Context, workspace, query string, budget int) (string, error)
	Skill   func(workspace, name string) (string, error)
	// Subagent runs one scoped child to completion (wired by internal/agent
	// to avoid an import cycle). Always foreground: doSubagent handles
	// background:true itself via the task store. Nil means the subagent
	// tool reports "not wired".
	Subagent func(ctx context.Context, sc SecCtx, agent, description, prompt string) (string, error)
	// WebSearchProvider selects the websearch backend ("", "random", "exa",
	// "firecrawl", "parallel", "tavily", "tinyfish"). Empty = auto-detect
	// from env keys. Mirrors opencode's `websearch.provider` setting.
	WebSearchProvider string
	// WebSearchDisabled removes the websearch tool (opencode `websearch: false`).
	WebSearchDisabled bool
	// OnBackgroundDone, when set, fires when a background task finishes
	// (the daemon maps it to a turn.progress SSE, like opencode's
	// background session notification).
	OnBackgroundDone func(sessionID, taskID, kind, label string)
	// Plugin hooks (JS tool.execute.before/after). BeforeCall may
	// rewrite args or return an error to block with guidance for the model.
	BeforeCall func(ctx context.Context, sc SecCtx, tool string, args map[string]interface{}) (map[string]interface{}, error)
	AfterCall  func(ctx context.Context, sc SecCtx, tool string, args map[string]interface{}, res Result, execErr error)

	mu    sync.Mutex
	tasks map[string]*Task
	seq   int
}

// NewRegistry builds the default tool set.
func NewRegistry() *Registry {
	r := &Registry{tools: map[string]*Tool{}, tasks: map[string]*Task{}}
	r.tools["read"] = &Tool{Name: "read", Description: "Read a workspace file (offset/limit).", Execute: r.doRead}
	r.tools["edit"] = &Tool{Name: "edit", Description: "Replace exact text in a file.", Execute: r.doEdit}
	r.tools["write"] = &Tool{Name: "write", Description: "Create/overwrite a file.", Execute: r.doWrite}
	r.tools["bash"] = &Tool{Name: "bash", Description: "Run a shell command in the workspace (background:true runs detached; poll with task_output).", Execute: r.doBash}
	r.tools["grep"] = &Tool{Name: "grep", Description: "Search file contents.", Execute: r.doGrep}
	r.tools["glob"] = &Tool{Name: "glob", Description: "Find files by glob.", Execute: r.doGlob}
	r.tools["skill"] = &Tool{Name: "skill", Description: "Load a SKILL.md body lazily.", Execute: r.doSkill}
	r.tools["lsp"] = &Tool{Name: "lsp", Description: "Code intelligence: symbols/refs (grep-backed).", Execute: r.doGrep}
	r.tools["mcp"] = &Tool{Name: "mcp", Description: "Call a local MCP tool (stdio/SSE).", Execute: r.doMCP}
	r.tools["kb_query"] = &Tool{Name: "kb_query", Description: "Query Markdown+graphify memory.", Execute: r.doKBQuery}
	r.tools["memory_write"] = &Tool{Name: "memory_write", Description: "Write atomic memory note.", Execute: r.doMemoryWrite}
	r.tools["webfetch"] = &Tool{Name: "webfetch", Description: "Fetch one HTTP(S) URL as markdown/text (cap 100KB).", Execute: r.doWebFetch}
	r.tools["websearch"] = &Tool{Name: "websearch", Description: "Search the web for current info (needs a provider key: TAVILY_API_KEY, EXA_API_KEY, FIRECRAWL_API_KEY, PARALLEL_API_KEY, or TINYFISH_API_KEY).", Execute: r.doWebSearch}
	r.tools["subagent"] = &Tool{Name: "subagent", Description: "Launch a scoped subagent child (general/explore/custom). background:true returns a task ID; poll with task_output.", Execute: r.doSubagent}
	r.tools["task_output"] = &Tool{Name: "task_output", Description: "Poll a background bash/subagent task by ID.", Execute: r.doTaskOutput}
	return r
}

// Names lists registered tools (for model schemas).
func (r *Registry) Names() []string {
	out := make([]string, 0, len(r.tools))
	for n := range r.tools {
		out = append(out, n)
	}
	sort.Strings(out)
	return out
}

// Call gates on the security broker, then executes.
func (r *Registry) Call(ctx context.Context, sc SecCtx, name string, args map[string]interface{}) (Result, error) {
	t, ok := r.tools[strings.ToLower(name)]
	if !ok {
		return Result{}, fmt.Errorf("unknown tool %q", name)
	}
	target := toolTarget(name, args)
	if d := security.Authorize(sc.Agent, sc.Permission, name, target, sc.WorkspaceDir, sc.BashRules, sc.AllowToolingWrite); d != security.Allow {
		return Result{}, fmt.Errorf("tool %q target %q: %s (permission_mode=%s agent=%s)", name, target, d, sc.Permission, sc.Agent)
	}
	if sc.WorkspaceDir != "" {
		for _, k := range []string{"path", "file"} {
			p, ok := args[k].(string)
			if !ok || p == "" || !isEditName(name) {
				continue
			}
			resolved := p
			if !filepath.IsAbs(p) {
				resolved = filepath.Join(sc.WorkspaceDir, p)
			}
			if !insideWorkspace(sc.WorkspaceDir, resolved) && !security.IsPlanWritable(sc.WorkspaceDir, p) {
				return Result{}, fmt.Errorf("tool %q: path escapes workspace", name)
			}
		}
	}
	if r.BeforeCall != nil {
		revised, err := r.BeforeCall(ctx, sc, strings.ToLower(name), args)
		if err != nil {
			return Result{}, err // plugin blocked with guidance
		}
		if revised != nil {
			args = revised
		}
	}
	res, execErr := t.Execute(ctx, sc, args)
	if r.AfterCall != nil {
		r.AfterCall(ctx, sc, strings.ToLower(name), args, res, execErr)
	}
	return res, execErr
}

func isEditName(n string) bool {
	switch strings.ToLower(n) {
	case "edit", "write", "read":
		return true
	}
	return false
}

func toolTarget(name string, args map[string]interface{}) string {
	switch strings.ToLower(name) {
	case "bash", "shell":
		if s, _ := args["command"].(string); s != "" {
			return s
		}
	case "grep", "lsp":
		// Authorize on the searched path only; the pattern itself is data.
		if p, _ := args["path"].(string); p != "" {
			return p
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
		if s, _ := args["id"].(string); s != "" {
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

func insideWorkspace(root, p string) bool {
	rel, err := filepath.Rel(root, filepath.Clean(p))
	if err != nil {
		return false
	}
	return rel != ".." && !strings.HasPrefix(rel, ".."+string(filepath.Separator))
}

func strArg(args map[string]interface{}, keys ...string) string {
	for _, k := range keys {
		if s, _ := args[k].(string); s != "" {
			return s
		}
	}
	return ""
}

func resolvePath(sc SecCtx, p string) string {
	if filepath.IsAbs(p) || sc.WorkspaceDir == "" {
		return p
	}
	return filepath.Join(sc.WorkspaceDir, p)
}

func (r *Registry) doRead(_ context.Context, sc SecCtx, args map[string]interface{}) (Result, error) {
	p := resolvePath(sc, strArg(args, "path", "file"))
	if p == "" {
		return Result{}, fmt.Errorf("read: missing path")
	}
	b, err := os.ReadFile(p)
	if err != nil {
		return Result{}, err
	}
	lines := strings.Split(string(b), "\n")
	offset, limit := intArg(args, "offset", 1), intArg(args, "limit", 200)
	if offset < 1 {
		offset = 1
	}
	end := offset - 1 + limit
	if end > len(lines) {
		end = len(lines)
	}
	if offset-1 > len(lines) {
		return Result{Output: ""}, nil
	}
	out := strings.Join(lines[offset-1:end], "\n")
	return Result{Output: out, Truncated: end < len(lines)}, nil
}

func (r *Registry) doEdit(_ context.Context, sc SecCtx, args map[string]interface{}) (Result, error) {
	p := resolvePath(sc, strArg(args, "path", "file"))
	oldS, newS := strArg(args, "old_string", "old"), strArg(args, "new_string", "new")
	if p == "" || oldS == "" {
		return Result{}, fmt.Errorf("edit: missing path/old_string")
	}
	b, err := os.ReadFile(p)
	if err != nil {
		return Result{}, err
	}
	if !strings.Contains(string(b), oldS) {
		return Result{}, fmt.Errorf("edit: old_string not found")
	}
	updated := strings.Replace(string(b), oldS, newS, 1)
	if err := os.WriteFile(p, []byte(updated), 0o644); err != nil {
		return Result{}, err
	}
	return Result{Output: "edited " + p}, nil
}

func (r *Registry) doWrite(_ context.Context, sc SecCtx, args map[string]interface{}) (Result, error) {
	p := resolvePath(sc, strArg(args, "path", "file"))
	content := strArg(args, "content")
	if p == "" {
		return Result{}, fmt.Errorf("write: missing path")
	}
	if err := os.MkdirAll(filepath.Dir(p), 0o755); err != nil {
		return Result{}, err
	}
	if err := os.WriteFile(p, []byte(content), 0o644); err != nil {
		return Result{}, err
	}
	return Result{Output: "wrote " + p}, nil
}

func (r *Registry) doBash(ctx context.Context, sc SecCtx, args map[string]interface{}) (Result, error) {
	cmdStr := strArg(args, "command")
	if cmdStr == "" {
		return Result{}, fmt.Errorf("bash: missing command")
	}
	// opencode parity: background:true detaches (no timeout unless one is
	// supplied); poll with task_output. Foreground keeps the 2-minute cap.
	if boolArg(args, "background") {
		timeout := timeoutArg(args)
		label := cmdStr
		if len(label) > 80 {
			label = label[:80] + "…"
		}
		id := r.newTask("bash", label)
		sessionID := sc.SessionID
		go func() {
			var runCtx context.Context = context.Background()
			var cancel context.CancelFunc = func() {}
			if timeout > 0 {
				runCtx, cancel = context.WithTimeout(runCtx, timeout)
				defer cancel()
			}
			cmd := exec.CommandContext(runCtx, "sh", "-c", cmdStr)
			if sc.WorkspaceDir != "" {
				cmd.Dir = sc.WorkspaceDir
			}
			cmd.Env = append(os.Environ(), "TMPDIR="+security.SessionTMPDIR("tool"))
			out, err := cmd.CombinedOutput()
			if err != nil && len(out) == 0 {
				r.finishTask(id, "", "error: "+err.Error())
				r.notifyDone(sessionID, id, "bash", label)
				return
			}
			r.finishTask(id, string(out), "")
			r.notifyDone(sessionID, id, "bash", label)
		}()
		return Result{Output: "started background task " + id + " (poll with task_output)"}, nil
	}
	timeout := timeoutArg(args)
	if timeout <= 0 {
		timeout = 120 * time.Second
	}
	ctx, cancel := context.WithTimeout(ctx, timeout)
	defer cancel()
	cmd := exec.CommandContext(ctx, "sh", "-c", cmdStr)
	if sc.WorkspaceDir != "" {
		cmd.Dir = sc.WorkspaceDir
	}
	cmd.Env = append(os.Environ(), "TMPDIR="+security.SessionTMPDIR("tool"))
	out, err := cmd.CombinedOutput()
	if err != nil && len(out) == 0 {
		return Result{}, fmt.Errorf("bash: %v", err)
	}
	return Result{Output: string(out)}, nil
}

func (r *Registry) doGrep(_ context.Context, sc SecCtx, args map[string]interface{}) (Result, error) {
	pattern := strArg(args, "pattern")
	root := sc.WorkspaceDir
	if p := strArg(args, "path"); p != "" {
		root = resolvePath(sc, p)
	}
	if pattern == "" || root == "" {
		return Result{}, fmt.Errorf("grep: missing pattern/path")
	}
	var out []string
	_ = filepath.Walk(root, func(p string, info os.FileInfo, err error) error {
		if err != nil || info.IsDir() || len(out) > 100 {
			return nil
		}
		if strings.Contains(p, "/.git/") || strings.Contains(p, "/node_modules/") {
			return nil
		}
		if security.IsCredentialPath(p) {
			return nil // never index secret-shaped files
		}
		if info.Size() > 1<<20 {
			return nil
		}
		b, err := os.ReadFile(p)
		if err != nil {
			return nil
		}
		for i, line := range strings.Split(string(b), "\n") {
			if strings.Contains(line, pattern) {
				rel := p
				if sc.WorkspaceDir != "" {
					rel, _ = filepath.Rel(sc.WorkspaceDir, p)
				}
				out = append(out, fmt.Sprintf("%s:%d:%s", rel, i+1, line))
				if len(out) >= 100 {
					break
				}
			}
		}
		return nil
	})
	return Result{Output: strings.Join(out, "\n"), Truncated: len(out) >= 100}, nil
}

func (r *Registry) doGlob(_ context.Context, sc SecCtx, args map[string]interface{}) (Result, error) {
	pattern := strArg(args, "pattern", "glob")
	if pattern == "" {
		return Result{}, fmt.Errorf("glob: missing pattern")
	}
	root := sc.WorkspaceDir
	matches, _ := filepath.Glob(filepath.Join(root, pattern))
	var rel []string
	for _, m := range matches {
		r, _ := filepath.Rel(root, m)
		rel = append(rel, r)
	}
	sort.Strings(rel)
	return Result{Output: strings.Join(rel, "\n")}, nil
}

func (r *Registry) doSkill(_ context.Context, sc SecCtx, args map[string]interface{}) (Result, error) {
	name := strArg(args, "name")
	if name == "" {
		return Result{}, fmt.Errorf("skill: missing name")
	}
	if r.Skill == nil {
		return Result{}, fmt.Errorf("skill: no loader configured")
	}
	body, err := r.Skill(sc.WorkspaceDir, name)
	if err != nil {
		return Result{}, err
	}
	return Result{Output: body}, nil
}

func (r *Registry) doMCP(ctx context.Context, sc SecCtx, args map[string]interface{}) (Result, error) {
	serverName := strArg(args, "server")
	toolName := strArg(args, "tool", "name")
	if serverName == "" || toolName == "" {
		return Result{}, fmt.Errorf("mcp: missing server/tool (see .lumina/mcp.json)")
	}
	cfg := mcp.LoadConfig(sc.WorkspaceDir)
	conf, ok := cfg.Servers[serverName]
	if !ok {
		return Result{}, fmt.Errorf("mcp: no server %q configured (see .lumina/mcp.json)", serverName)
	}
	c, err := mcp.Dial(conf)
	if err != nil {
		return Result{}, err
	}
	defer c.Close()
	callArgs := map[string]interface{}{}
	if a, ok := args["args"].(map[string]interface{}); ok {
		callArgs = a
	}
	out, err := c.CallTool(ctx, toolName, callArgs)
	if err != nil {
		return Result{}, err
	}
	return Result{Output: out}, nil
}

func (r *Registry) doKBQuery(ctx context.Context, sc SecCtx, args map[string]interface{}) (Result, error) {
	if r.KBQuery == nil {
		return Result{}, fmt.Errorf("kb_query: engine not attached")
	}
	q := strArg(args, "query", "q")
	out, err := r.KBQuery(ctx, sc.WorkspaceDir, q, intArg(args, "budget", 4000))
	if err != nil {
		return Result{}, err
	}
	return Result{Output: out}, nil
}

func (r *Registry) doMemoryWrite(_ context.Context, sc SecCtx, args map[string]interface{}) (Result, error) {
	topic := strArg(args, "topic", "name")
	content := strArg(args, "content", "body")
	if topic == "" || content == "" {
		return Result{}, fmt.Errorf("memory_write: missing topic/content")
	}
	if !insideWorkspace(sc.WorkspaceDir, filepath.Join(sc.WorkspaceDir, "memory", topic)) && sc.WorkspaceDir != "" {
		// Memory writes stay inside ./memory.
	}
	dir := filepath.Join(sc.WorkspaceDir, "memory")
	if err := os.MkdirAll(dir, 0o755); err != nil {
		return Result{}, err
	}
	name := topic
	if !strings.HasSuffix(name, ".md") {
		name += ".md"
	}
	p := filepath.Join(dir, filepath.Base(name)) // Base prevents traversal
	existing, _ := os.ReadFile(p)
	if strings.Contains(string(existing), content) {
		return Result{Output: "dedup: already present in " + p}, nil
	}
	f, err := os.OpenFile(p, os.O_APPEND|os.O_CREATE|os.O_WRONLY, 0o644)
	if err != nil {
		return Result{}, err
	}
	defer f.Close()
	w := bufio.NewWriter(f)
	if len(existing) > 0 {
		w.WriteString("\n")
	}
	w.WriteString(content)
	if !strings.HasSuffix(content, "\n") {
		w.WriteString("\n")
	}
	w.Flush()
	return Result{Output: "wrote " + p}, nil
}

func intArg(args map[string]interface{}, key string, d int) int {
	switch v := args[key].(type) {
	case int:
		return v
	case int64:
		return int(v)
	case float64:
		return int(v)
	}
	return d
}

func boolArg(args map[string]interface{}, key string) bool {
	switch v := args[key].(type) {
	case bool:
		return v
	case string:
		lc := strings.ToLower(strings.TrimSpace(v))
		return lc == "true" || lc == "1" || lc == "yes"
	}
	return false
}

// timeoutArg reads an optional millisecond timeout (opencode `timeout`).
func timeoutArg(args map[string]interface{}) time.Duration {
	for _, k := range []string{"timeout", "timeout_ms", "timeoutMs"} {
		switch v := args[k].(type) {
		case int:
			if v > 0 {
				return time.Duration(v) * time.Millisecond
			}
		case int64:
			if v > 0 {
				return time.Duration(v) * time.Millisecond
			}
		case float64:
			if v > 0 {
				return time.Duration(int(v)) * time.Millisecond
			}
		}
	}
	return 0
}

// --- background tasks (bash background:true, subagent background:true) ---

func (r *Registry) newTask(kind, label string) string {
	r.mu.Lock()
	defer r.mu.Unlock()
	r.seq++
	id := fmt.Sprintf("task-%d", r.seq)
	r.tasks[id] = &Task{ID: id, Kind: kind, Label: label, Status: "running", StartedAt: time.Now()}
	return id
}

func (r *Registry) finishTask(id, output, errStr string) {
	r.mu.Lock()
	defer r.mu.Unlock()
	t, ok := r.tasks[id]
	if !ok {
		return
	}
	if errStr != "" {
		t.Status = "error"
		t.Output = errStr
	} else {
		t.Status = "done"
		t.Output = output
	}
	t.DoneAt = time.Time{}
	t.DoneAt = time.Now()
}

func (r *Registry) notifyDone(sessionID, id, kind, label string) {
	if r.OnBackgroundDone != nil {
		r.OnBackgroundDone(sessionID, id, kind, label)
	}
}

func (r *Registry) doTaskOutput(_ context.Context, _ SecCtx, args map[string]interface{}) (Result, error) {
	id := strArg(args, "task_id", "id", "taskId")
	if id == "" {
		return Result{}, fmt.Errorf("task_output: missing task_id")
	}
	r.mu.Lock()
	t, ok := r.tasks[id]
	r.mu.Unlock()
	if !ok {
		return Result{}, fmt.Errorf("task_output: unknown task %q", id)
	}
	if t.Status == "running" {
		return Result{Output: "task " + id + " (" + t.Kind + ") still running: " + t.Label}, nil
	}
	out := t.Output
	if len(out) > 30000 {
		out = out[:30000] + "\n…[truncated; full output retained in task " + id + "]"
		return Result{Output: out, Truncated: true}, nil
	}
	return Result{Output: out}, nil
}

func (r *Registry) doSubagent(ctx context.Context, sc SecCtx, args map[string]interface{}) (Result, error) {
	if sc.SubagentDepth >= 1 {
		return Result{}, fmt.Errorf("subagent: nesting depth 1 exceeded (subagents cannot launch subagents)")
	}
	if r.Subagent == nil {
		return Result{}, fmt.Errorf("subagent: runner not wired (no model backend)")
	}
	agentName := strArg(args, "agent", "name", "id")
	prompt := strArg(args, "prompt", "task")
	if agentName == "" || prompt == "" {
		return Result{}, fmt.Errorf("subagent: missing agent/prompt (need {agent, description, prompt})")
	}
	description := strArg(args, "description")
	if !boolArg(args, "background") {
		out, err := r.Subagent(ctx, sc, agentName, description, prompt)
		if err != nil {
			return Result{}, err
		}
		return Result{Output: out}, nil
	}
	// background:true detaches like opencode: return the task ID now,
	// poll with task_output, notify on completion.
	label := agentName
	if description != "" {
		label += ": " + description
	}
	id := r.newTask("subagent", label)
	sessionID := sc.SessionID
	childSc := sc
	childSc.SubagentDepth = sc.SubagentDepth + 1
	go func() {
		out, err := r.Subagent(context.Background(), childSc, agentName, description, prompt)
		if err != nil {
			r.finishTask(id, "", "error: "+err.Error())
		} else {
			r.finishTask(id, out, "")
		}
		r.notifyDone(sessionID, id, "subagent", label)
	}()
	return Result{Output: "started background subagent task " + id + " (poll with task_output)"}, nil
}

// --- webfetch (opencode `webfetch` parity, reimplemented) ---

func (r *Registry) doWebFetch(_ context.Context, _ SecCtx, args map[string]interface{}) (Result, error) {
	rawURL := strArg(args, "url")
	if rawURL == "" {
		return Result{}, fmt.Errorf("webfetch: missing url")
	}
	u, err := url.Parse(rawURL)
	if err != nil || (u.Scheme != "http" && u.Scheme != "https") || u.Host == "" {
		return Result{}, fmt.Errorf("webfetch: only http(s) URLs are allowed")
	}
	format := strings.ToLower(strArg(args, "format"))
	if format == "" {
		format = "markdown"
	}
	timeout := timeoutArg(args)
	if timeout <= 0 {
		timeout = 30 * time.Second
	}
	if timeout > 120*time.Second {
		timeout = 120 * time.Second
	}
	client := &http.Client{Timeout: timeout}
	req, err := http.NewRequest("GET", rawURL, nil)
	if err != nil {
		return Result{}, err
	}
	req.Header.Set("User-Agent", "lumina/0.1.0")
	req.Header.Set("Accept", "text/html,text/plain,application/json;q=0.9,*/*;q=0.5")
	resp, err := client.Do(req)
	if err != nil {
		return Result{}, fmt.Errorf("webfetch: %v", err)
	}
	defer resp.Body.Close()
	if resp.StatusCode >= 400 {
		return Result{}, fmt.Errorf("webfetch: status %d for %s", resp.StatusCode, rawURL)
	}
	ct := strings.ToLower(resp.Header.Get("Content-Type"))
	if strings.Contains(ct, "image/") || strings.Contains(ct, "video/") ||
		strings.Contains(ct, "audio/") || strings.Contains(ct, "application/octet-stream") {
		return Result{}, fmt.Errorf("webfetch: binary content not supported (%s)", ct)
	}
	const capBytes = 100 << 10
	b, err := io.ReadAll(io.LimitReader(resp.Body, capBytes+1))
	if err != nil {
		return Result{}, err
	}
	truncated := len(b) > capBytes
	if truncated {
		b = b[:capBytes]
	}
	_ = format // markdown is the default rendering; raw text passes through
	out := string(b)
	if truncated {
		out += "\n…[truncated at 100KB]"
	}
	return Result{Output: out, Truncated: truncated}, nil
}

// --- websearch (opencode provider parity, reimplemented) ---

// searchCooldown tracks per-provider 429 backoff (opencode random-selection
// semantics: a rate-limited provider sleeps for Retry-After or 60s).
var searchCooldown = struct {
	sync.Mutex
	until map[string]time.Time
}{until: map[string]time.Time{}}

func searchCooling(provider string) bool {
	searchCooldown.Lock()
	defer searchCooldown.Unlock()
	return time.Now().Before(searchCooldown.until[provider])
}

func searchBackoff(provider string, retryAfter string) {
	delay := 60 * time.Second
	if retryAfter != "" {
		var secs int
		if _, err := fmt.Sscanf(strings.TrimSpace(retryAfter), "%d", &secs); err == nil && secs > 0 && secs < 3600 {
			delay = time.Duration(secs) * time.Second
		}
	}
	searchCooldown.Lock()
	searchCooldown.until[provider] = time.Now().Add(delay)
	searchCooldown.Unlock()
}

func webSearchProviders() []string {
	return []string{"tavily", "exa", "firecrawl", "parallel", "tinyfish"}
}

func webSearchKey(provider string) string {
	switch provider {
	case "tavily":
		return os.Getenv("TAVILY_API_KEY")
	case "exa":
		return os.Getenv("EXA_API_KEY")
	case "firecrawl":
		return os.Getenv("FIRECRAWL_API_KEY")
	case "parallel":
		return os.Getenv("PARALLEL_API_KEY")
	case "tinyfish":
		return os.Getenv("TINYFISH_API_KEY")
	}
	return ""
}

func (r *Registry) doWebSearch(_ context.Context, _ SecCtx, args map[string]interface{}) (Result, error) {
	if r.WebSearchDisabled {
		return Result{}, fmt.Errorf("websearch: disabled by configuration (websearch = false)")
	}
	q := strArg(args, "query", "q")
	if q == "" {
		return Result{}, fmt.Errorf("websearch: missing query")
	}
	want := strings.ToLower(strings.TrimSpace(r.WebSearchProvider))
	var order []string
	switch want {
	case "", "random", "auto":
		for _, p := range webSearchProviders() {
			if webSearchKey(p) != "" && !searchCooling(p) {
				order = append(order, p)
			}
		}
	case "tavily", "exa", "firecrawl", "parallel", "tinyfish":
		order = []string{want}
	default:
		return Result{}, fmt.Errorf("websearch: unknown provider %q", r.WebSearchProvider)
	}
	if len(order) == 0 {
		return Result{}, fmt.Errorf("websearch: no provider available (set TAVILY_API_KEY, EXA_API_KEY, FIRECRAWL_API_KEY, PARALLEL_API_KEY, or TINYFISH_API_KEY; pick with [websearch] provider)")
	}
	var lastErr error
	for _, p := range order {
		if searchCooling(p) {
			continue
		}
		out, retryAfter, limited, err := runWebSearch(p, webSearchKey(p), q)
		if err != nil {
			lastErr = err
			if limited {
				searchBackoff(p, retryAfter)
			}
			// Fixed (non-random) selection surfaces the error like opencode.
			if want != "" && want != "random" && want != "auto" {
				return Result{}, err
			}
			continue
		}
		return Result{Output: out}, nil
	}
	if lastErr != nil {
		return Result{}, lastErr
	}
	return Result{}, fmt.Errorf("websearch: every provider is cooling down after 429; retry shortly")
}

func webPostJSON(url, key string, body interface{}, extraHeaders map[string]string) (map[string]interface{}, string, bool, error) {
	raw, err := json.Marshal(body)
	if err != nil {
		return nil, "", false, err
	}
	req, err := http.NewRequest("POST", url, bytes.NewReader(raw))
	if err != nil {
		return nil, "", false, err
	}
	req.Header.Set("Content-Type", "application/json")
	if key != "" {
		req.Header.Set("Authorization", "Bearer "+key)
	}
	for k, v := range extraHeaders {
		req.Header.Set(k, v)
	}
	req.Header.Set("User-Agent", "lumina/0.1.0")
	resp, err := (&http.Client{Timeout: 30 * time.Second}).Do(req)
	if err != nil {
		return nil, "", false, err
	}
	defer resp.Body.Close()
	b, _ := io.ReadAll(io.LimitReader(resp.Body, 1<<20))
	if resp.StatusCode == 429 {
		return nil, resp.Header.Get("Retry-After"), true, fmt.Errorf("websearch: provider rate limited (429)")
	}
	if resp.StatusCode >= 400 {
		return nil, "", false, fmt.Errorf("websearch: status %d: %s", resp.StatusCode, truncStr(string(b), 300))
	}
	var decoded map[string]interface{}
	if err := json.Unmarshal(b, &decoded); err != nil {
		return nil, "", false, fmt.Errorf("websearch: bad JSON: %v", err)
	}
	return decoded, "", false, nil
}

func truncStr(s string, n int) string {
	if len(s) <= n {
		return s
	}
	return s[:n] + "…"
}

func runWebSearch(provider, key, query string) (string, string, bool, error) {
	if key == "" {
		return "", "", false, fmt.Errorf("websearch: %s key is not set", provider)
	}
	switch provider {
	case "tavily":
		decoded, retryAfter, limited, err := webPostJSON("https://api.tavily.com/search", key,
			map[string]interface{}{"query": query, "search_depth": "basic", "max_results": 8, "include_answer": false}, nil)
		if err != nil {
			return "", retryAfter, limited, err
		}
		return formatTavily(decoded), "", false, nil
	case "exa":
		decoded, retryAfter, limited, err := webPostJSON("https://api.exa.ai/search", key,
			map[string]interface{}{"query": query, "type": "auto", "numResults": 8, "contents": map[string]interface{}{"highlights": true}}, nil)
		if err != nil {
			return "", retryAfter, limited, err
		}
		return formatExa(decoded), "", false, nil
	case "firecrawl":
		decoded, retryAfter, limited, err := webPostJSON("https://api.firecrawl.dev/v2/search", key,
			map[string]interface{}{"query": query, "limit": 8, "sources": []interface{}{map[string]interface{}{"type": "web"}}}, nil)
		if err != nil {
			return "", retryAfter, limited, err
		}
		return formatFirecrawl(decoded), "", false, nil
	case "parallel":
		decoded, retryAfter, limited, err := webPostJSON("https://api.parallel.ai/v1/search", "",
			map[string]interface{}{"objective": query, "search_queries": []string{query}, "mode": "basic"},
			map[string]string{"x-api-key": key})
		if err != nil {
			return "", retryAfter, limited, err
		}
		return formatParallel(decoded), "", false, nil
	case "tinyfish":
		req, _ := http.NewRequest("GET", "https://api.search.tinyfish.ai?query="+url.QueryEscape(query), nil)
		req.Header.Set("X-API-Key", key)
		req.Header.Set("User-Agent", "lumina/0.1.0")
		resp, err := (&http.Client{Timeout: 30 * time.Second}).Do(req)
		if err != nil {
			return "", "", false, err
		}
		defer resp.Body.Close()
		b, _ := io.ReadAll(io.LimitReader(resp.Body, 1<<20))
		if resp.StatusCode == 429 {
			return "", resp.Header.Get("Retry-After"), true, fmt.Errorf("websearch: provider rate limited (429)")
		}
		if resp.StatusCode >= 400 {
			return "", "", false, fmt.Errorf("websearch: status %d: %s", resp.StatusCode, truncStr(string(b), 300))
		}
		var decoded map[string]interface{}
		if err := json.Unmarshal(b, &decoded); err != nil {
			return "", "", false, fmt.Errorf("websearch: bad JSON: %v", err)
		}
		return formatTinyfish(decoded), "", false, nil
	}
	return "", "", false, fmt.Errorf("websearch: unknown provider %q", provider)
}

func formatTavily(decoded map[string]interface{}) string {
	var sb strings.Builder
	if arr, _ := decoded["results"].([]interface{}); arr != nil {
		for i, it := range arr {
			m, _ := it.(map[string]interface{})
			if m == nil {
				continue
			}
			title, _ := m["title"].(string)
			u, _ := m["url"].(string)
			content, _ := m["content"].(string)
			fmt.Fprintf(&sb, "%d. %s\n   %s\n   %s\n", i+1, title, u, truncStr(content, 500))
		}
	}
	if ans, _ := decoded["answer"].(string); ans != "" {
		sb.WriteString("\nAnswer: " + truncStr(ans, 1000) + "\n")
	}
	if sb.Len() == 0 {
		sb.WriteString("(no results)")
	}
	return sb.String()
}

func formatExa(decoded map[string]interface{}) string {
	var sb strings.Builder
	if arr, _ := decoded["results"].([]interface{}); arr != nil {
		for i, it := range arr {
			m, _ := it.(map[string]interface{})
			if m == nil {
				continue
			}
			title, _ := m["title"].(string)
			u, _ := m["url"].(string)
			sb.WriteString(fmt.Sprintf("%d. %s\n   %s\n", i+1, title, u))
			if hl, _ := m["highlights"].([]interface{}); hl != nil {
				for _, h := range hl {
					if s, _ := h.(string); s != "" {
						sb.WriteString("   " + truncStr(s, 500) + "\n")
						break
					}
				}
			}
		}
	}
	if sb.Len() == 0 {
		sb.WriteString("(no results)")
	}
	return sb.String()
}

func formatFirecrawl(decoded map[string]interface{}) string {
	var sb strings.Builder
	data, _ := decoded["data"].(map[string]interface{})
	var arr []interface{}
	if data != nil {
		arr, _ = data["web"].([]interface{})
	}
	for i, it := range arr {
		m, _ := it.(map[string]interface{})
		if m == nil {
			continue
		}
		title, _ := m["title"].(string)
		u, _ := m["url"].(string)
		desc, _ := m["description"].(string)
		md, _ := m["markdown"].(string)
		snip := desc
		if md != "" {
			snip = md
		}
		fmt.Fprintf(&sb, "%d. %s\n   %s\n   %s\n", i+1, title, u, truncStr(snip, 500))
	}
	if sb.Len() == 0 {
		sb.WriteString("(no results)")
	}
	return sb.String()
}

func formatParallel(decoded map[string]interface{}) string {
	var sb strings.Builder
	if arr, _ := decoded["results"].([]interface{}); arr != nil {
		for i, it := range arr {
			m, _ := it.(map[string]interface{})
			if m == nil {
				continue
			}
			title, _ := m["title"].(string)
			u, _ := m["url"].(string)
			sb.WriteString(fmt.Sprintf("%d. %s\n   %s\n", i+1, title, u))
			if ex, _ := m["excerpts"].([]interface{}); ex != nil {
				for _, e := range ex {
					if s, _ := e.(string); s != "" {
						sb.WriteString("   " + truncStr(s, 500) + "\n")
						break
					}
				}
			}
		}
	}
	if sb.Len() == 0 {
		sb.WriteString("(no results)")
	}
	return sb.String()
}

func formatTinyfish(decoded map[string]interface{}) string {
	var sb strings.Builder
	if arr, _ := decoded["results"].([]interface{}); arr != nil {
		for i, it := range arr {
			m, _ := it.(map[string]interface{})
			if m == nil {
				continue
			}
			title, _ := m["title"].(string)
			u, _ := m["url"].(string)
			snip, _ := m["snippet"].(string)
			fmt.Fprintf(&sb, "%d. %s\n   %s\n   %s\n", i+1, title, u, truncStr(snip, 500))
		}
	}
	if sb.Len() == 0 {
		sb.WriteString("(no results)")
	}
	return sb.String()
}
