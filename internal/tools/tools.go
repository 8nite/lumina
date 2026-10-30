// Package tools implements the agent tool envelope: read, edit, write,
// bash, grep, glob, skill, lsp, mcp, kb_query, memory_write — every call
// gated by internal/security Authorize().
package tools

import (
	"bufio"
	"context"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"sort"
	"strings"
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
	// Plugin hooks (JS tool.execute.before/after). BeforeCall may
	// rewrite args or return an error to block with guidance for the model.
	BeforeCall func(ctx context.Context, sc SecCtx, tool string, args map[string]interface{}) (map[string]interface{}, error)
	AfterCall  func(ctx context.Context, sc SecCtx, tool string, args map[string]interface{}, res Result, execErr error)
}

// NewRegistry builds the default tool set.
func NewRegistry() *Registry {
	r := &Registry{tools: map[string]*Tool{}}
	r.tools["read"] = &Tool{Name: "read", Description: "Read a workspace file (offset/limit).", Execute: r.doRead}
	r.tools["edit"] = &Tool{Name: "edit", Description: "Replace exact text in a file.", Execute: r.doEdit}
	r.tools["write"] = &Tool{Name: "write", Description: "Create/overwrite a file.", Execute: r.doWrite}
	r.tools["bash"] = &Tool{Name: "bash", Description: "Run a shell command in the workspace.", Execute: r.doBash}
	r.tools["grep"] = &Tool{Name: "grep", Description: "Search file contents.", Execute: r.doGrep}
	r.tools["glob"] = &Tool{Name: "glob", Description: "Find files by glob.", Execute: r.doGlob}
	r.tools["skill"] = &Tool{Name: "skill", Description: "Load a SKILL.md body lazily.", Execute: r.doSkill}
	r.tools["lsp"] = &Tool{Name: "lsp", Description: "Code intelligence: symbols/refs (grep-backed).", Execute: r.doGrep}
	r.tools["mcp"] = &Tool{Name: "mcp", Description: "Call a local MCP tool (stdio/SSE).", Execute: r.doMCP}
	r.tools["kb_query"] = &Tool{Name: "kb_query", Description: "Query Markdown+graphify memory.", Execute: r.doKBQuery}
	r.tools["memory_write"] = &Tool{Name: "memory_write", Description: "Write atomic memory note.", Execute: r.doMemoryWrite}
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
	case "bash":
		if s, _ := args["command"].(string); s != "" {
			return s
		}
	case "grep", "lsp":
		// Authorize on the searched path only; the pattern itself is data.
		if p, _ := args["path"].(string); p != "" {
			return p
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
	ctx, cancel := context.WithTimeout(ctx, 120*time.Second)
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
