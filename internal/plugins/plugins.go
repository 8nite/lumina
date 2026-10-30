// Package plugins bridges JS/TS plugins into the Go
// daemon via an on-demand Node v22 worker (host.mjs) over a local Unix
// domain socket with per-hook timeouts. Zero cost when unconfigured.
package plugins

import (
	"bufio"
	"context"
	_ "embed" // directive-only use for //go:embed host.mjs
	"encoding/json"
	"fmt"
	"net"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"sync"
	"time"
)

//go:embed host.mjs
var embeddedHost string

// Hook names supported by the bridge.
const (
	HookConfig        = "config"
	HookChatMessage   = "chat.message"
	HookChatTransform = "chat.messages.transform"
	HookToolBefore    = "tool.execute.before"
	HookToolAfter     = "tool.execute.after"
	HookShellEnv      = "shell.env"
	HookPermissionAsk = "permission.ask"
	HookEvent         = "event"
	defaultTimeout    = 5 * time.Second
)

// Call is one hook invocation.
type Call struct {
	Hook      string      `json:"hook"`
	Output    interface{} `json:"output,omitempty"`
	SessionID string      `json:"session_id,omitempty"`
}

// Result is the worker reply.
type Result struct {
	Patch    map[string]interface{} `json:"patch,omitempty"`
	Block    string                 `json:"block,omitempty"`
	Allow    *bool                  `json:"allow,omitempty"`
	Decision string                 `json:"decision,omitempty"`
	Error    string                 `json:"error,omitempty"`
}

// Plugin describes a loaded file.
type Plugin struct {
	Path string `json:"path"`
	ID   string `json:"id"`
}

// Host manages the node worker lifecycle.
type Host struct {
	mu      sync.Mutex
	nodeBin string
	hostJS  string
	sock    string
	cmd     *exec.Cmd
	conn    net.Conn
	plugins []Plugin
	timeout time.Duration
}

// Discover finds plugin files in global + workspace + harness roots.
func Discover(workspaceDir string, optInExternal bool) []string {
	home, _ := os.UserHomeDir()
	roots := []string{
		filepath.Join(home, ".config", "lumina", "plugins"),
	}
	if workspaceDir != "" {
		roots = append(roots,
			filepath.Join(workspaceDir, ".lumina", "plugins"),
			filepath.Join(workspaceDir, ".lumina", "harness", "plugins"))
	}
	if optInExternal {
		roots = append(roots, filepath.Join(home, ".config", "lumina", "compat-plugins"))
	}
	var out []string
	for _, root := range roots {
		entries, err := os.ReadDir(root)
		if err != nil {
			continue
		}
		for _, e := range entries {
			p := filepath.Join(root, e.Name())
			if e.IsDir() {
				for _, cand := range []string{"plugin.js", "plugin.mjs", "plugin.ts", "index.js"} {
					if st, err := os.Stat(filepath.Join(p, cand)); err == nil && !st.IsDir() {
						out = append(out, filepath.Join(p, cand))
					}
				}
				continue
			}
			if strings.HasSuffix(p, ".js") || strings.HasSuffix(p, ".mjs") || strings.HasSuffix(p, ".ts") {
				out = append(out, p)
			}
		}
	}
	return out
}

// NewHost prepares (but does not start) the worker for these plugin files.
func NewHost(pluginFiles []string) *Host {
	var ps []Plugin
	for _, f := range pluginFiles {
		ps = append(ps, Plugin{Path: f, ID: filepath.Base(filepath.Dir(f)) + "/" + filepath.Base(f)})
	}
	return &Host{nodeBin: "node", plugins: ps, timeout: defaultTimeout}
}

// hostScriptPath returns the worker script: explicit override, on-disk copy,
// or the embedded fallback (so plugins work with cwd outside the repo).
func hostScriptPath() string {
	if p := os.Getenv("LUMINA_PLUGIN_HOST"); p != "" {
		return p
	}
	for _, c := range []string{"internal/plugins/host.mjs", "host.mjs"} {
		if _, err := os.Stat(c); err == nil {
			return c
		}
	}
	return materializeEmbedded()
}

var (
	embeddedMu   sync.Mutex
	embeddedPath string
)

// materializeEmbedded writes the go:embed host.mjs to a temp file (once per
// path; re-materializes after CleanupEmbedded).
func materializeEmbedded() string {
	embeddedMu.Lock()
	defer embeddedMu.Unlock()
	if embeddedPath != "" {
		if _, err := os.Stat(embeddedPath); err == nil {
			return embeddedPath
		}
	}
	dir, err := os.MkdirTemp("", "lumina-host-*")
	if err != nil {
		return ""
	}
	p := filepath.Join(dir, "host.mjs")
	if err := os.WriteFile(p, []byte(embeddedHost), 0o644); err != nil {
		return ""
	}
	embeddedPath = p
	return embeddedPath
}

// CleanupEmbedded removes the materialized host dir (test hygiene; the file
// is re-materialized on next use). Safe to call when no Host is running.
func CleanupEmbedded() {
	embeddedMu.Lock()
	defer embeddedMu.Unlock()
	if embeddedPath == "" {
		return
	}
	_ = os.RemoveAll(filepath.Dir(embeddedPath))
	embeddedPath = ""
}

// ensureStarted lazily spawns node host.mjs.
func (h *Host) ensureStarted() error {
	h.mu.Lock()
	defer h.mu.Unlock()
	if h.conn != nil || len(h.plugins) == 0 {
		return nil
	}
	if _, err := exec.LookPath(h.nodeBin); err != nil {
		return fmt.Errorf("plugins: node not found")
	}
	dir, err := os.MkdirTemp("", "lumina-plugins-*")
	if err != nil {
		return err
	}
	h.sock = filepath.Join(dir, "host.sock")
	h.hostJS = hostScriptPath()
	files, _ := json.Marshal(h.plugins)
	h.cmd = exec.Command(h.nodeBin, h.hostJS, "--socket", h.sock, "--plugins", string(files))
	h.cmd.Stdout = os.Stderr
	h.cmd.Stderr = os.Stderr
	if err := h.cmd.Start(); err != nil {
		return err
	}
	// Wait for socket with deadline.
	deadline := time.Now().Add(10 * time.Second)
	for {
		c, err := net.DialTimeout("unix", h.sock, time.Second)
		if err == nil {
			h.conn = c
			return nil
		}
		if time.Now().After(deadline) {
			_ = h.cmd.Process.Kill()
			return fmt.Errorf("plugins: worker failed to listen: %w", err)
		}
		time.Sleep(100 * time.Millisecond)
	}
}

// Dispatch invokes a hook across all plugins in order.
func (h *Host) Dispatch(ctx context.Context, hook string, payload map[string]interface{}) (map[string]interface{}, error) {
	if len(h.plugins) == 0 {
		return payload, nil
	}
	if err := h.ensureStarted(); err != nil {
		return payload, err
	}
	h.mu.Lock()
	conn := h.conn
	h.mu.Unlock()
	if conn == nil {
		return payload, fmt.Errorf("plugins: worker not running")
	}
	req := map[string]interface{}{"hook": hook, "payload": payload}
	raw, _ := json.Marshal(req)
	ctx, cancel := context.WithTimeout(ctx, h.timeout)
	defer cancel()
	type res struct {
		line string
		err  error
	}
	ch := make(chan res, 1)
	go func() {
		_, err := fmt.Fprintf(conn, "%s\n", raw)
		if err != nil {
			ch <- res{err: err}
			return
		}
		line, err := bufio.NewReader(conn).ReadString('\n')
		ch <- res{line: line, err: err}
	}()
	select {
	case <-ctx.Done():
		return payload, ctx.Err()
	case r := <-ch:
		if r.err != nil {
			return payload, r.err
		}
		var out map[string]interface{}
		if err := json.Unmarshal([]byte(r.line), &out); err != nil {
			return payload, err
		}
		if blk, _ := out["block"].(string); blk != "" {
			return payload, fmt.Errorf("plugin blocked: %s", blk)
		}
		if emsg, _ := out["error"].(string); emsg != "" {
			return payload, fmt.Errorf("plugin blocked: %s", emsg)
		}
		if patch, ok := out["patch"].(map[string]interface{}); ok {
			for k, v := range patch {
				payload[k] = v
			}
		}
		return payload, nil
	}
}

// Loaded lists discovered plugin files.
func (h *Host) Loaded() []Plugin { return h.plugins }

// Close stops the worker.
func (h *Host) Close() error {
	h.mu.Lock()
	defer h.mu.Unlock()
	if h.conn != nil {
		h.conn.Close()
		h.conn = nil
	}
	if h.cmd != nil && h.cmd.Process != nil {
		_ = h.cmd.Process.Kill()
		_ = h.cmd.Wait()
	}
	if h.sock != "" {
		_ = os.RemoveAll(filepath.Dir(h.sock))
	}
	return nil
}
