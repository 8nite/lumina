// Package tools implements the agent tool envelope: read, edit, write,
// bash, grep, glob, skill, lsp, mcp, kb_query, memory_write, webfetch,
// websearch, question, todowrite, subagent, task_output — every call gated by
// internal/security Authorize(). Web + subagent semantics are
// reimplemented for Lumina's toolset.
package tools

import (
	"bufio"
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"html"
	"io"
	"net/http"
	"net/url"
	"os"
	"os/exec"
	"path"
	"path/filepath"
	"regexp"
	"sort"
	"strconv"
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
	// SubagentDepth counts delegation nesting. The cap is the registry's
	// maxDepth (default 1): a subagent may not launch its own subagents.
	SubagentDepth int
	// SessionID routes background-done notifications to the owning session.
	SessionID string
}

// SubagentRequest is one scoped child delegation.
type SubagentRequest struct {
	Agent       string
	Description string
	Prompt      string
	// Model optionally overrides the child model (provider/model).
	Model string
}

// Todo is one tracked task for the todowrite tool.
type Todo struct {
	Content string `json:"content"`
	Status  string `json:"status"` // pending | in_progress | completed | cancelled
}

// QuestionOption is one offered answer to a user question.
type QuestionOption struct {
	Label       string `json:"label"`
	Description string `json:"description"`
}

// Question is one model question for the user (opencode Question.Prompt
// parity: question/header/options/multiple).
type Question struct {
	Question string           `json:"question"`
	Header   string           `json:"header"`
	Options  []QuestionOption `json:"options"`
	Multiple bool             `json:"multiple"`
}

// ErrQuestionUnwired reports a question call with no UI attached (headless
// runs, tests). doQuestion maps it to proceed-with-best-guess guidance.
var ErrQuestionUnwired = fmt.Errorf("question UI not wired")

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
	Subagent func(ctx context.Context, sc SecCtx, req SubagentRequest) (string, error)
	// SubagentMaxDepth caps delegation nesting (0 = default 1). Overridable
	// via LUMINA_SUBAGENT_DEPTH. Children never receive the subagent tool,
	// so depth can only grow through this backend.
	SubagentMaxDepth int
	// AskQuestion answers a question-tool call with the user's replies, one
	// string list per question (multiple:true may yield several). Nil means
	// no question UI is wired: doQuestion reports ErrQuestionUnwired so the
	// turn proceeds with best guesses instead of hanging a headless run.
	AskQuestion func(ctx context.Context, sc SecCtx, questions []Question) ([][]string, error)
	// WebSearchProvider selects the websearch backend ("", "random", "exa",
	// "firecrawl", "parallel", "tavily", "tinyfish"). Empty = auto-detect
	// from env keys. Mirrors the `[websearch] provider` setting.
	WebSearchProvider string
	// WebSearchDisabled removes the websearch tool (`websearch: false`).
	WebSearchDisabled bool
	// OnBackgroundDone, when set, fires when a background task finishes
	// (the daemon maps it to a turn.progress SSE background notification).
	OnBackgroundDone func(sessionID, taskID, kind, label string)
	// Plugin hooks (JS tool.execute.before/after). BeforeCall may
	// rewrite args or return an error to block with guidance for the model.
	BeforeCall func(ctx context.Context, sc SecCtx, tool string, args map[string]interface{}) (map[string]interface{}, error)
	AfterCall  func(ctx context.Context, sc SecCtx, tool string, args map[string]interface{}, res Result, execErr error)

	mu    sync.Mutex
	tasks map[string]*Task
	seq   int
	// todos tracks todowrite state per session.
	todos map[string][]Todo
}

// maxDepth returns the configured delegation cap (default 1).
func (r *Registry) maxDepth() int {
	if r.SubagentMaxDepth > 0 {
		return r.SubagentMaxDepth
	}
	return 1
}

// subagentDepthOverride reads LUMINA_SUBAGENT_DEPTH once at startup.
func subagentDepthOverride() int {
	v := strings.TrimSpace(os.Getenv("LUMINA_SUBAGENT_DEPTH"))
	if v == "" {
		return 0
	}
	n, err := strconv.Atoi(v)
	if err != nil || n < 1 {
		return 0
	}
	return n
}

// NewRegistry builds the default tool set.
func NewRegistry() *Registry {
	r := &Registry{tools: map[string]*Tool{}, tasks: map[string]*Task{}, todos: map[string][]Todo{}}
	if d := subagentDepthOverride(); d > 0 {
		r.SubagentMaxDepth = d
	}
	year := time.Now().Year()
	r.tools["read"] = &Tool{Name: "read", Description: "Read a file or directory (line: content, offset/limit).", Execute: r.doRead}
	r.tools["edit"] = &Tool{Name: "edit", Description: "Edit a file by finding and replacing exact text.", Execute: r.doEdit}
	r.tools["write"] = &Tool{Name: "write", Description: "Write a file, overwriting if one exists; creates parent dirs.", Execute: r.doWrite}
	r.tools["bash"] = &Tool{Name: "bash", Description: "Execute a shell command (workdir optional; background:true runs detached; poll with task_output).", Execute: r.doBash}
	r.tools["grep"] = &Tool{Name: "grep", Description: "Search file contents (regex or literal).", Execute: r.doGrep}
	r.tools["glob"] = &Tool{Name: "glob", Description: "Search file paths by glob.", Execute: r.doGlob}
	r.tools["skill"] = &Tool{Name: "skill", Description: "Load a skill's instructions by name.", Execute: r.doSkill}
	r.tools["lsp"] = &Tool{Name: "lsp", Description: "Code intelligence: symbols/refs (grep-backed).", Execute: r.doGrep}
	r.tools["mcp"] = &Tool{Name: "mcp", Description: "Call a local MCP tool (stdio/SSE).", Execute: r.doMCP}
	r.tools["kb_query"] = &Tool{Name: "kb_query", Description: "Query Markdown+graphify memory.", Execute: r.doKBQuery}
	r.tools["memory_write"] = &Tool{Name: "memory_write", Description: "Write atomic memory note.", Execute: r.doMemoryWrite}
	r.tools["webfetch"] = &Tool{Name: "webfetch", Description: "Fetch one HTTP(S) URL as markdown/text/HTML (cap 100KB).", Execute: r.doWebFetch}
	r.tools["websearch"] = &Tool{Name: "websearch", Description: fmt.Sprintf("Search the web for current info (needs a provider key: TAVILY_API_KEY, EXA_API_KEY, FIRECRAWL_API_KEY, PARALLEL_API_KEY, or TINYFISH_API_KEY). The current year is %d.", year), Execute: r.doWebSearch}
	r.tools["patch"] = &Tool{Name: "patch", Description: "Apply a *** Begin Patch *** multi-file diff (add/update/delete).", Execute: r.doPatch}
	r.tools["question"] = &Tool{Name: "question", Description: "Ask the user questions during execution.", Execute: r.doQuestion}
	r.tools["todowrite"] = &Tool{Name: "todowrite", Description: "Track task progress as a todo list.", Execute: r.doTodoWrite}
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
			expanded := security.ExpandHome(p)
			resolved := expanded
			if !filepath.IsAbs(expanded) {
				resolved = filepath.Join(sc.WorkspaceDir, expanded)
			}
			// Layer 0: workspace containment with external whitelist.
			// Reads may touch home/tmp/system; writes stay workspace+tmp.
			// Plan-mode plan.md scope is enforced in Authorize/doPatch.
			allowed := false
			switch strings.ToLower(name) {
			case "read":
				allowed = insideWorkspace(sc.WorkspaceDir, resolved) || security.IsWhitelistedRead(resolved)
			default:
				allowed = insideWorkspace(sc.WorkspaceDir, resolved) || security.IsWhitelistedWrite(resolved)
			}
			if !allowed && !security.IsPlanWritable(sc.WorkspaceDir, p) {
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
	case "edit", "write", "read", "patch":
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
	case "patch":
		// Authorize on the first touched file; doPatch re-gates every
		// file (workspace containment, credential shapes, plan scope).
		if s, _ := args["patchText"].(string); s != "" {
			return patchFirstPath(s)
		}
		if s, _ := args["patch_text"].(string); s != "" {
			return patchFirstPath(s)
		}
		if s, _ := args["patch"].(string); s != "" {
			return patchFirstPath(s)
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
	p = security.ExpandHome(p)
	rel, err := filepath.Rel(root, filepath.Clean(p))
	if err != nil {
		return false
	}
	return rel != ".." && !strings.HasPrefix(rel, ".."+string(filepath.Separator))
}

func isReadAllowed(root, p string) bool {
	return security.IsReadAllowed(root, p)
}

func isWriteAllowed(root, p string) bool {
	return security.IsWriteAllowed(root, p)
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
	p = security.ExpandHome(p)
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
	st, err := os.Stat(p)
	if err != nil {
		return Result{}, err
	}
	offset, limit := intArg(args, "offset", 1), intArg(args, "limit", 2000)
	if offset < 1 {
		offset = 1
	}
	if limit < 1 {
		limit = 2000
	}
	if limit > 2000 {
		limit = 2000
	}
	// Directory: one entry per line, paginated like text.
	// Workspace-internal credential entries are filtered; external whitelisted
	// dirs (home/tmp/system) list all dotfiles per explicit user override.
	if st.IsDir() {
		entries, err := os.ReadDir(p)
		if err != nil {
			return Result{}, err
		}
		var names []string
		for _, e := range entries {
			full := filepath.Join(p, e.Name())
			if security.IsCredentialPath(full) && insideWorkspace(sc.WorkspaceDir, full) {
				continue
			}
			names = append(names, e.Name())
		}
		sort.Strings(names)
		end := offset - 1 + limit
		if end > len(names) {
			end = len(names)
		}
		if offset-1 > len(names) {
			return Result{Output: ""}, nil
		}
		return Result{Output: strings.Join(names[offset-1:end], "\n"), Truncated: end < len(names)}, nil
	}
	b, err := os.ReadFile(p)
	if err != nil {
		return Result{}, err
	}
	if bytes.IndexByte(b, 0) >= 0 {
		return Result{}, fmt.Errorf("read: binary file not supported (%s)", p)
	}
	lines := strings.Split(string(b), "\n")
	end := offset - 1 + limit
	if end > len(lines) {
		end = len(lines)
	}
	if offset-1 > len(lines) {
		return Result{Output: ""}, nil
	}
	// ponytail: 1-based "line: content" matches opencode read.ts so models
	// can cite lines and edits stay prefix-free.
	out := make([]string, 0, end-(offset-1))
	for i := offset - 1; i < end; i++ {
		out = append(out, fmt.Sprintf("%d: %s", i+1, lines[i]))
	}
	return Result{Output: strings.Join(out, "\n"), Truncated: end < len(lines)}, nil
}

func (r *Registry) doEdit(_ context.Context, sc SecCtx, args map[string]interface{}) (Result, error) {
	p := resolvePath(sc, strArg(args, "path", "file"))
	oldS, newS := strArg(args, "old_string", "oldString", "old"), strArg(args, "new_string", "newString", "new")
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
	// ponytail: opencode parity — replaceAll replaces every occurrence.
	updated := string(b)
	if boolArg(args, "replaceAll") || boolArg(args, "replace_all") {
		updated = strings.ReplaceAll(updated, oldS, newS)
	} else {
		if strings.Count(updated, oldS) > 1 {
			return Result{}, fmt.Errorf("edit: old_string matches %d locations; add context or set replaceAll:true", strings.Count(updated, oldS))
		}
		updated = strings.Replace(updated, oldS, newS, 1)
	}
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
	// workdir runs the command in the workspace or a whitelisted external
	// dir (home/tmp/system for inspection; temp for scratch).
	dir := sc.WorkspaceDir
	if w := strArg(args, "workdir", "work_dir", "cwd"); w != "" {
		expanded := security.ExpandHome(w)
		resolved := expanded
		if !filepath.IsAbs(expanded) {
			resolved = filepath.Join(sc.WorkspaceDir, expanded)
		}
		if !insideWorkspace(sc.WorkspaceDir, resolved) && !security.IsWhitelistedRead(resolved) {
			return Result{}, fmt.Errorf("bash: workdir escapes workspace")
		}
		if st, err := os.Stat(resolved); err != nil || !st.IsDir() {
			return Result{}, fmt.Errorf("bash: workdir is not a directory: %s", w)
		}
		dir = resolved
	}
	// background:true detaches (no timeout unless one is
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
			cmd.Dir = dir
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
	cmd.Dir = dir
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
		if sc.WorkspaceDir != "" && !insideWorkspace(sc.WorkspaceDir, root) && !security.IsWhitelistedRead(root) {
			return Result{}, fmt.Errorf("grep: path escapes workspace")
		}
	}
	if pattern == "" || root == "" {
		return Result{}, fmt.Errorf("grep: missing pattern/path")
	}
	// literal:true matches exact text; otherwise pattern is a regexp.
	literal := boolArg(args, "literal")
	caseSensitive := true
	if _, ok := args["caseSensitive"]; ok {
		caseSensitive = boolArg(args, "caseSensitive")
	} else if _, ok := args["case_sensitive"]; ok {
		caseSensitive = boolArg(args, "case_sensitive")
	}
	var re *regexp.Regexp
	if !literal {
		expr := pattern
		if !caseSensitive {
			expr = "(?i)" + expr
		}
		var err error
		if re, err = regexp.Compile(expr); err != nil {
			return Result{}, fmt.Errorf("grep: invalid pattern: %v", err)
		}
	}
	include := strArg(args, "include")
	limit := intArg(args, "limit", 100)
	if limit < 1 {
		limit = 100
	}
	if limit > 500 {
		limit = 500
	}
	needle := pattern
	if !caseSensitive {
		needle = strings.ToLower(pattern)
	}
	var out []string
	_ = filepath.Walk(root, func(p string, info os.FileInfo, err error) error {
		if err != nil || info.IsDir() || len(out) >= limit {
			return nil
		}
		if strings.Contains(p, "/.git/") || strings.Contains(p, "/node_modules/") {
			return nil
		}
		if security.IsCredentialPath(p) && insideWorkspace(sc.WorkspaceDir, p) {
			return nil // workspace secrets never indexed; external whitelist allows dotfiles
		}
		if info.Size() > 1<<20 {
			return nil
		}
		rel := p
		if sc.WorkspaceDir != "" {
			rel, _ = filepath.Rel(sc.WorkspaceDir, p)
		}
		if include != "" {
			// Match against the relative path and the base name so
			// "*.go" works at any depth.
			if ok, _ := path.Match(include, rel); !ok {
				if ok, _ := path.Match(include, filepath.Base(rel)); !ok {
					return nil
				}
			}
		}
		b, err := os.ReadFile(p)
		if err != nil {
			return nil
		}
		if bytes.IndexByte(b, 0) >= 0 {
			return nil // skip binaries
		}
		for i, line := range strings.Split(string(b), "\n") {
			var hit bool
			if literal {
				if caseSensitive {
					hit = strings.Contains(line, pattern)
				} else {
					hit = strings.Contains(strings.ToLower(line), needle)
				}
			} else {
				hit = re.MatchString(line)
			}
			if hit {
				out = append(out, fmt.Sprintf("%s:%d:%s", rel, i+1, line))
				if len(out) >= limit {
					break
				}
			}
		}
		return nil
	})
	return Result{Output: strings.Join(out, "\n"), Truncated: len(out) >= limit}, nil
}

func (r *Registry) doGlob(_ context.Context, sc SecCtx, args map[string]interface{}) (Result, error) {
	pattern := strArg(args, "pattern", "glob")
	if pattern == "" {
		return Result{}, fmt.Errorf("glob: missing pattern")
	}
	root := sc.WorkspaceDir
	if p := strArg(args, "path"); p != "" {
		root = resolvePath(sc, p)
		if !insideWorkspace(sc.WorkspaceDir, root) && !security.IsWhitelistedRead(root) {
			return Result{}, fmt.Errorf("glob: path escapes workspace")
		}
	}
	limit := intArg(args, "limit", 100)
	if limit < 1 {
		limit = 100
	}
	if limit > 1000 {
		limit = 1000
	}
	var rel []string
	if strings.Contains(pattern, "**") {
		re, err := globToRegExp(pattern)
		if err != nil {
			return Result{}, fmt.Errorf("glob: bad pattern: %v", err)
		}
		_ = filepath.Walk(root, func(p string, info os.FileInfo, err error) error {
			if err != nil || len(rel) >= limit {
				return nil
			}
			rp, err := filepath.Rel(root, p)
			if err != nil || rp == "." {
				return nil
			}
			if strings.Contains(p, "/.git/") || strings.Contains(p, "/node_modules/") {
				if info.IsDir() {
					return filepath.SkipDir
				}
				return nil
			}
			if security.IsCredentialPath(p) && insideWorkspace(sc.WorkspaceDir, p) {
				return nil // workspace secrets hidden; external whitelist lists all dotfiles
			}
			if re.MatchString(filepath.ToSlash(rp)) {
				rel = append(rel, rp)
			}
			return nil
		})
	} else {
		matches, _ := filepath.Glob(filepath.Join(root, pattern))
		for _, m := range matches {
			if security.IsCredentialPath(m) && insideWorkspace(sc.WorkspaceDir, m) {
				continue
			}
			rp, _ := filepath.Rel(root, m)
			rel = append(rel, rp)
		}
		sort.Strings(rel)
	}
	truncated := len(rel) > limit
	if truncated {
		rel = rel[:limit]
	}
	return Result{Output: strings.Join(rel, "\n"), Truncated: truncated}, nil
}

// globToRegExp compiles a glob with ** (any depth), * (within a segment),
// ? and [...] classes to a regexp over slash-separated paths.
func globToRegExp(pattern string) (*regexp.Regexp, error) {
	var sb strings.Builder
	sb.WriteString("^")
	pat := filepath.ToSlash(pattern)
	for i := 0; i < len(pat); {
		switch pat[i] {
		case '*':
			if i+1 < len(pat) && pat[i+1] == '*' {
				sb.WriteString(".*")
				i += 2
				// A trailing slash after ** is optional ("src/**/x" matches "src/x").
				if i < len(pat) && pat[i] == '/' {
					sb.WriteString("/?")
					i++
				}
			} else {
				sb.WriteString("[^/]*")
				i++
			}
		case '?':
			sb.WriteString("[^/]")
			i++
		case '[':
			j := i + 1
			for j < len(pat) && pat[j] != ']' {
				j++
			}
			if j >= len(pat) {
				return nil, fmt.Errorf("unterminated [")
			}
			sb.WriteString(pat[i : j+1])
			i = j + 1
		default:
			sb.WriteString(regexp.QuoteMeta(string(pat[i])))
			i++
		}
	}
	sb.WriteString("$")
	return regexp.Compile(sb.String())
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

// timeoutArg reads an optional millisecond timeout.
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
	if !ok {
		r.mu.Unlock()
		return Result{}, fmt.Errorf("task_output: unknown task %q", id)
	}
	// Snapshot under the lock: finishTask writes these fields when the
	// background unit completes (data race when read unlocked).
	status, kind, label, output := t.Status, t.Kind, t.Label, t.Output
	r.mu.Unlock()
	if status == "running" {
		return Result{Output: "task " + id + " (" + kind + ") still running: " + label}, nil
	}
	out := output
	if len(out) > 30000 {
		out = out[:30000] + "\n…[truncated; full output retained in task " + id + "]"
		return Result{Output: out, Truncated: true}, nil
	}
	return Result{Output: out}, nil
}

func (r *Registry) doSubagent(ctx context.Context, sc SecCtx, args map[string]interface{}) (Result, error) {
	if sc.SubagentDepth >= r.maxDepth() {
		return Result{}, fmt.Errorf("subagent: nesting depth %d exceeded (subagents cannot launch subagents)", r.maxDepth())
	}
	if r.Subagent == nil {
		return Result{}, fmt.Errorf("subagent: runner not wired (no model backend)")
	}
	// Follow-ups target live child sessions, which Lumina does not keep:
	// fail loudly instead of silently starting an unrelated child.
	if s := strArg(args, "sessionID", "sessionId", "session_id"); s != "" {
		return Result{}, fmt.Errorf("subagent: follow-up via sessionID is not supported; start a new subagent with full context instead")
	}
	agentName := strArg(args, "agent", "name", "id")
	prompt := strArg(args, "prompt", "task")
	if agentName == "" || prompt == "" {
		return Result{}, fmt.Errorf("subagent: missing agent/prompt (need {agent, description, prompt})")
	}
	description := strArg(args, "description")
	model := strArg(args, "model")
	req := SubagentRequest{Agent: agentName, Description: description, Prompt: prompt, Model: model}
	if !boolArg(args, "background") {
		out, err := r.Subagent(ctx, sc, req)
		if err != nil {
			return Result{}, err
		}
		return Result{Output: out}, nil
	}
	// background:true detaches: return the task ID now,
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
		out, err := r.Subagent(context.Background(), childSc, req)
		if err != nil {
			r.finishTask(id, "", "error: "+err.Error())
		} else {
			r.finishTask(id, out, "")
		}
		r.notifyDone(sessionID, id, "subagent", label)
	}()
	return Result{Output: "started background subagent task " + id + " (poll with task_output)"}, nil
}

// doQuestion answers via the wired question UI, or guides the model on when
// none is attached (headless runs) so the turn proceeds with best guesses.
func (r *Registry) doQuestion(ctx context.Context, sc SecCtx, args map[string]interface{}) (Result, error) {
	qs, err := parseQuestions(args)
	if err != nil {
		return Result{}, err
	}
	if r.AskQuestion == nil {
		return Result{Output: unwiredQuestionGuidance(qs)}, nil
	}
	answers, err := r.AskQuestion(ctx, sc, qs)
	if err != nil {
		if err == ErrQuestionUnwired {
			return Result{Output: unwiredQuestionGuidance(qs)}, nil
		}
		// Dismissals and transport failures reach the model as guidance to
		// continue without the answers, never as a stuck turn.
		return Result{}, fmt.Errorf("question: %v; proceed with your best guess and note the assumption", err)
	}
	return Result{Output: formatQuestionAnswers(qs, answers)}, nil
}

// parseQuestions decodes the question-tool input (opencode Question.Prompt
// shape: {question, header, options: [{label, description}], multiple}).
func parseQuestions(args map[string]interface{}) ([]Question, error) {
	raw, _ := args["questions"].([]interface{})
	if len(raw) == 0 {
		return nil, fmt.Errorf("question: missing questions")
	}
	qs := make([]Question, 0, len(raw))
	for i, item := range raw {
		// Bare-string shorthand ("q1") means {question: "q1"}; models
		// often ask plain questions without options to pick from.
		if s, _ := item.(string); s != "" {
			qs = append(qs, Question{Question: strings.TrimSpace(s)})
			continue
		}
		m, _ := item.(map[string]interface{})
		if m == nil {
			return nil, fmt.Errorf("question: questions[%d] must be {question, header, options}", i)
		}
		q := Question{
			Question: strings.TrimSpace(strArg(m, "question")),
			Header:   strings.TrimSpace(strArg(m, "header")),
			Multiple: boolArg(m, "multiple"),
		}
		if q.Question == "" {
			return nil, fmt.Errorf("question: questions[%d].question is required", i)
		}
		if opts, _ := m["options"].([]interface{}); opts != nil {
			for j, o := range opts {
				om, _ := o.(map[string]interface{})
				if om == nil {
					return nil, fmt.Errorf("question: questions[%d].options[%d] must be {label, description}", i, j)
				}
				label := strings.TrimSpace(strArg(om, "label"))
				if label == "" {
					return nil, fmt.Errorf("question: questions[%d].options[%d].label is required", i, j)
				}
				q.Options = append(q.Options, QuestionOption{
					Label:       label,
					Description: strings.TrimSpace(strArg(om, "description")),
				})
			}
		}
		qs = append(qs, q)
	}
	return qs, nil
}

// formatQuestionAnswers renders replies model-side, mirroring opencode's
// QuestionTool.toModelContent.
func formatQuestionAnswers(qs []Question, answers [][]string) string {
	parts := make([]string, 0, len(qs))
	for i, q := range qs {
		var a []string
		if i < len(answers) {
			a = answers[i]
		}
		if len(a) == 0 {
			a = []string{"Unanswered"}
		}
		parts = append(parts, fmt.Sprintf("%q=%q", q.Question, strings.Join(a, ", ")))
	}
	return "User has answered your questions: " + strings.Join(parts, ", ") + ". You can now continue with the user's answers in mind."
}

func unwiredQuestionGuidance(qs []Question) string {
	names := make([]string, 0, len(qs))
	for _, q := range qs {
		names = append(names, q.Question)
	}
	return "question UI is not wired in this session; proceed with your best guess for (" +
		strings.Join(names, "; ") + ") and note the assumption"
}

// doTodoWrite stores the session's todo list and summarizes it, mirroring
// opencode's todowrite tool.
func (r *Registry) doTodoWrite(_ context.Context, sc SecCtx, args map[string]interface{}) (Result, error) {
	raw, _ := args["todos"].([]interface{})
	todos := make([]Todo, 0, len(raw))
	for i, item := range raw {
		m, _ := item.(map[string]interface{})
		if m == nil {
			return Result{}, fmt.Errorf("todowrite: todos[%d] must be {content, status}", i)
		}
		content := strings.TrimSpace(strArg(m, "content"))
		status := strings.ToLower(strings.TrimSpace(strArg(m, "status")))
		switch status {
		case "pending", "in_progress", "completed", "cancelled":
		default:
			return Result{}, fmt.Errorf("todowrite: todos[%d].status must be pending|in_progress|completed|cancelled", i)
		}
		if content == "" {
			return Result{}, fmt.Errorf("todowrite: todos[%d].content is required", i)
		}
		todos = append(todos, Todo{Content: content, Status: status})
	}
	r.mu.Lock()
	if r.todos == nil {
		r.todos = map[string][]Todo{}
	}
	r.todos[sc.SessionID] = todos
	r.mu.Unlock()
	return Result{Output: summarizeTodos(todos)}, nil
}

func summarizeTodos(todos []Todo) string {
	if len(todos) == 0 {
		return "Todos cleared"
	}
	count := func(status string) int {
		n := 0
		for _, t := range todos {
			if t.Status == status {
				n++
			}
		}
		return n
	}
	parts := []string{fmt.Sprintf("%d/%d completed", count("completed"), len(todos))}
	for _, s := range []string{"in_progress", "pending", "cancelled"} {
		if n := count(s); n > 0 {
			parts = append(parts, fmt.Sprintf("%d %s", n, strings.ReplaceAll(s, "_", " ")))
		}
	}
	out := "Todos updated: " + strings.Join(parts, ", ")
	var active []string
	for _, t := range todos {
		if t.Status == "in_progress" {
			active = append(active, t.Content)
		}
	}
	if len(active) > 0 {
		out += ". In progress: " + strings.Join(active, ", ")
	}
	return out
}

// --- webfetch (reimplemented) ---

func (r *Registry) doWebFetch(_ context.Context, _ SecCtx, args map[string]interface{}) (Result, error) {
	rawURL := strArg(args, "url")
	if rawURL == "" {
		return Result{}, fmt.Errorf("webfetch: missing url")
	}
	u, err := url.Parse(rawURL)
	if err != nil || (u.Scheme != "http" && u.Scheme != "https") || u.Host == "" {
		return Result{}, fmt.Errorf("webfetch: only http(s) URLs are allowed")
	}
	if security.IsCredentialPath(rawURL) || security.IsGoogleTarget(rawURL) {
		return Result{}, fmt.Errorf("webfetch: blocked target")
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
	// ponytail: manual redirect walk so every hop is scheme- and
	// hard-deny-checked like opencode's webfetch (no silent cross-host hops
	// past credential/Google blocks).
	client := &http.Client{Timeout: timeout, CheckRedirect: func(req *http.Request, _ []*http.Request) error {
		if len(strings.TrimSpace(req.URL.String())) == 0 {
			return fmt.Errorf("webfetch: empty redirect")
		}
		if req.URL.Scheme != "http" && req.URL.Scheme != "https" {
			return fmt.Errorf("webfetch: only http(s) URLs are allowed")
		}
		if security.IsCredentialPath(req.URL.String()) || security.IsGoogleTarget(req.URL.String()) {
			return fmt.Errorf("webfetch: blocked redirect target")
		}
		return nil
	}}
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
	out := string(b)
	if format == "markdown" || format == "text" {
		if strings.Contains(ct, "html") || looksLikeHTML(out) {
			out = htmlToText(out)
		}
	}
	if truncated {
		out += "\n…[truncated at 100KB]"
	}
	return Result{Output: out, Truncated: truncated}, nil
}

// looksLikeHTML sniffs markup when the server omits the content type.
func looksLikeHTML(s string) bool {
	t := strings.ToLower(strings.TrimSpace(s))
	return strings.HasPrefix(t, "<!doctype html") || strings.HasPrefix(t, "<html")
}

// htmlToText strips markup to readable text: scripts/styles dropped,
// block tags folded to newlines, entities unescaped, whitespace collapsed.
func htmlToText(s string) string {
	reScript := regexp.MustCompile(`(?is)<(script|style|noscript)[^>]*>.*?</(script|style|noscript)>`)
	s = reScript.ReplaceAllString(s, "\n")
	reBlock := regexp.MustCompile(`(?i)</?(p|div|br|li|ul|ol|tr|table|h[1-6]|section|article|header|footer|blockquote)[^>]*>`)
	s = reBlock.ReplaceAllString(s, "\n")
	reTag := regexp.MustCompile(`<[^>]+>`)
	s = reTag.ReplaceAllString(s, "")
	s = html.UnescapeString(s)
	lines := strings.Split(s, "\n")
	var kept []string
	for _, ln := range lines {
		if t := strings.TrimSpace(regexp.MustCompile(`[ \t]+`).ReplaceAllString(ln, " ")); t != "" {
			kept = append(kept, t)
		}
	}
	return strings.Join(kept, "\n")
}

// --- websearch (multi-provider, reimplemented) ---

// searchCooldown tracks per-provider 429 backoff (random-selection
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
		return Result{}, fmt.Errorf("websearch: no provider available (set TAVILY_API_KEY, EXA_API_KEY, FIRECRAWL_API_KEY, PARALLEL_API_KEY, or TINYFISH_API_KEY; pick with [websearch] provider) — fallback: use webfetch on a direct URL or public JSON API (no key needed)")
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
			// Fixed (non-random) selection surfaces the error.
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
