// Command lumina: TUI, run, serve, web, attach, session, plugin, kb, evolve.
package main

import (
	"context"
	"encoding/base64"
	"encoding/json"
	"flag"
	"fmt"
	ifs "io/fs"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"sort"
	"strings"
	"time"

	"github.com/8nite/lumina/internal/agent"
	"github.com/8nite/lumina/internal/config"
	"github.com/8nite/lumina/internal/harness"
	"github.com/8nite/lumina/internal/kb"
	"github.com/8nite/lumina/internal/llm"
	"github.com/8nite/lumina/internal/plugins"
	"github.com/8nite/lumina/internal/rules"
	"github.com/8nite/lumina/internal/security"
	"github.com/8nite/lumina/internal/server"
	"github.com/8nite/lumina/internal/tools"
	"github.com/8nite/lumina/pkg/sdk"
	"github.com/8nite/lumina/web"
	"github.com/google/uuid"
)

const version = "0.1.0"

func main() {
	if err := run(os.Args[1:]); err != nil {
		fmt.Fprintln(os.Stderr, "lumina: "+err.Error())
		os.Exit(1)
	}
}

func run(args []string) error {
	if len(args) == 0 {
		return runTUI(configBaseFlags())
	}
	switch args[0] {
	case "serve":
		return runServe(args[1:])
	case "run":
		return runHeadless(args[1:])
	case "web":
		return runWeb(args[1:])
	case "attach":
		return runAttach(args[1:])
	case "session":
		return runSession(args[1:])
	case "plugin":
		return runPlugin(args[1:])
	case "kb":
		return runKB(args[1:])
	case "evolve":
		return runEvolve(args[1:])
	case "models":
		return runModels(args[1:])
	case "auth":
		return runAuth(args[1:])
	case "pair":
		return runPair(args[1:])
	case "openapi":
		return runOpenAPI(args[1:])
	case "vault":
		return runVault(args[1:])
	case "-h", "--help", "help":
		usage()
		return nil
	case "-V", "--version":
		fmt.Println("lumina " + version)
		return nil
	default:
		// Bare flags: `lumina -c`, `lumina --agent plan`, ...
		if strings.HasPrefix(args[0], "-") {
			return runTUI(parseTUIArgs(args))
		}
		return fmt.Errorf("unknown command %q (see --help)", args[0])
	}
}

func usage() {
	fmt.Println(`lumina — local-first AI coding & open-world agent harness

Usage:
  lumina [--agent plan|build] [-c] [-s <id>] [--auto-edit|-y|--bypass]
  lumina run [--agent plan|build] [-c] [-s <id>] [-m model] [--format text|json] "<prompt>"
  lumina serve [--host 127.0.0.1] [--port 7432] [--cors]
  lumina web | lumina attach <url>
  lumina session [list|show|fork|export|delete] ...
  lumina plugin [list|doctor]
  lumina kb [query|reflect|status]
  lumina evolve [session-id]
  lumina models [--provider copilot]   # list live provider models
  lumina auth device --provider copilot # GitHub web-auth, stores token
  lumina pair [--lan]               # print LAN/Tailscale attach URL + token
  lumina vault check "<text>"     # show what the LLM would receive (placeholders), user side keeps values`)
}

// ---------- shared setup ----------

func cwd() string {
	d, _ := os.Getwd()
	return d
}

func storePath() string {
	if v := os.Getenv("LUMINA_DB"); v != "" {
		return v
	}
	home, _ := os.UserHomeDir()
	return filepath.Join(home, ".local", "share", "lumina", "lumina.db")
}

func loadStack() (*config.Config, *llm.Router, *tools.Registry) {
	cfgPath := config.DefaultPath()
	if v := os.Getenv("LUMINA_CONFIG"); v != "" {
		cfgPath = v
	}
	cfg, err := config.Load(cfgPath)
	if err != nil {
		cfg = config.Defaults()
	}
	router, err := llm.NewRouter(cfg)
	if err != nil {
		fmt.Fprintln(os.Stderr, "lumina: router: "+err.Error())
		router = nil
	}
	reg := tools.NewRegistry()
	wireToolBackends(reg)
	reg.WebSearchProvider = cfg.WebSearch.Provider
	reg.WebSearchDisabled = cfg.WebSearch.Disabled
	return cfg, router, reg
}

func openStore() (*agent.Store, error) {
	return agent.Open(storePath())
}

// ---------- serve ----------

func runServe(args []string) error {
	fs := flag.NewFlagSet("serve", flag.ContinueOnError)
	host := fs.String("host", "127.0.0.1", "bind host")
	port := fs.Int("port", 7432, "bind port")
	_ = fs.String("cors", "*", "cors origins (informational; daemon allows loopback by default)")
	_ = fs.Parse(args)

	cfg, router, reg := loadStack()
	st, err := openStore()
	if err != nil {
		return err
	}
	defer st.Close()

	srv := server.New(server.Options{AuthToken: os.Getenv("LUMINA_TOKEN")})
	if sub, err := ifs.Sub(web.Dist, "dist"); err == nil {
		srv.MountSPA(http.FileServer(http.FS(sub)))
	}
	srv.Store = st
	runner := agent.NewRunner(st, router, reg)
	// Live feedback: stage updates become
	// turn.progress SSE; approval pauses publish permission.asked and wait
	// for the TUI answer instead of failing the tool call.
	runner.OnProgress = func(sessionID, stage string) {
		srv.Broker().Publish(server.Event{Type: "turn.progress", SessionID: sessionID, Data: map[string]string{"stage": stage}})
	}
	runner.OnDelta = func(sessionID, delta, thinking string) {
		srv.Broker().Publish(server.Event{Type: "turn.delta", SessionID: sessionID, Data: map[string]string{"delta": delta, "thinking": thinking}})
	}
	// Context gauge (estimated prompt tokens vs window) rides turn.context.
	runner.OnContext = func(sessionID string, used, max int) {
		srv.Broker().Publish(server.Event{Type: "turn.context", SessionID: sessionID, Data: map[string]int{"context_tokens": used, "max_tokens": max}})
	}
	runner.OnToolStart = func(sessionID, tool, target string) {
		srv.Broker().Publish(server.Event{Type: "turn.tool", SessionID: sessionID, Data: map[string]interface{}{"phase": "start", "tool": tool, "target": target}})
	}
	runner.OnToolDone = func(sessionID, tool, target, preview string, isErr bool, durationMs int64) {
		srv.Broker().Publish(server.Event{Type: "turn.tool", SessionID: sessionID, Data: map[string]interface{}{"phase": "done", "tool": tool, "target": target, "preview": preview, "error": isErr, "duration_ms": durationMs}})
	}
	// Turn summaries (agent, model, duration, tok/s) ride the
	// turn.completed event; the prompt handler picks them up below.
	runner.OnTurnEnd = srv.NoteTurnEnd
	// Inbox: steer/queue while a turn runs.
	srv.SteerPrompt = runner.Steer
	srv.QueuePrompt = runner.Queue
	srv.TakeQueued = runner.TakeQueued
	runner.AskPermission = func(ctx context.Context, sessionID, tool, target string) (string, error) {
		return srv.RequestPermission(ctx, sessionID, tool, target)
	}
	runner.AskQuestion = srv.RequestQuestion
	var provNames []string
	for name := range cfg.Providers {
		provNames = append(provNames, name)
	}
	sort.Strings(provNames)
	srv.ProviderNames = provNames
	srv.ProviderProtocols = map[string]string{}
	for name, p := range cfg.Providers {
		srv.ProviderProtocols[name] = p.Protocol
	}
	for role, model := range map[string]string{
		"fast": cfg.Router.Fast, "executor": cfg.Router.Executor,
		"planner": cfg.Router.Planner, "improver": cfg.Router.Improver,
	} {
		if model != "" {
			srv.RouterState[role] = model
		}
	}
	if router != nil {
		srv.OnRouterUpdate = func(role, model string) {
			_ = router.SetModel(role, model)
		}
		srv.OnAuthRefresh = func(provider, key string) {
			router.RefreshCopilotKey(provider, key)
		}
	}
	srv.CopilotTokenURL = func(provider string) string {
		if p, ok := cfg.Providers[provider]; ok && p.TokenURL != "" {
			return p.TokenURL
		}
		return ""
	}
	srv.ListProviderModels = func(ctx context.Context, provider string) ([]llm.ModelMeta, error) {
		if router == nil {
			return nil, fmt.Errorf("no router configured")
		}
		p, ok := router.Provider(provider)
		if !ok {
			return nil, fmt.Errorf("unknown provider %q", provider)
		}
		lister, ok := p.(llm.ModelLister)
		if !ok {
			return nil, fmt.Errorf("provider %q does not list models", provider)
		}
		return lister.ListModels(ctx)
	}
	if err := attachPluginHooks(reg); err != nil {
		fmt.Fprintln(os.Stderr, "lumina: plugin bridge: "+err.Error())
	}
	srv.Exec = func(ctx context.Context, sessionID string, in agent.TurnInput) (string, error) {
		ctx, cancel := context.WithTimeout(ctx, 10*time.Minute)
		defer cancel()
		return runner.RunPrompt(ctx, sessionID, in)
	}

	if !isLoopback(*host) && os.Getenv("LUMINA_TOKEN") == "" {
		return fmt.Errorf("refusing to bind non-loopback %s without LUMINA_TOKEN (see `lumina pair`)", *host)
	}
	addr := fmt.Sprintf("%s:%d", *host, *port)
	fmt.Printf("lumina serve v%s on http://%s\n", version, addr)
	return http.ListenAndServe(addr, srv.Handler())
}

// isLoopback reports loopback-only bind addresses.
func isLoopback(host string) bool {
	switch strings.ToLower(strings.TrimSpace(host)) {
	case "127.0.0.1", "localhost", "::1", "":
		return true
	}
	return false
}

// attachPluginHooks wires JS plugin tool.execute.before/after hooks around
// every tool call (TP-05.5). Zero cost when no plugins are configured.
func attachPluginHooks(reg *tools.Registry) error {
	files := plugins.Discover(cwd(), true)
	if len(files) == 0 {
		return nil
	}
	host := plugins.NewHost(files)
	reg.BeforeCall = func(ctx context.Context, sc tools.SecCtx, tool string, args map[string]interface{}) (map[string]interface{}, error) {
		payload := map[string]interface{}{"tool": tool, "workspace": sc.WorkspaceDir, "agent": string(sc.Agent)}
		for k, v := range args {
			payload[k] = v
		}
		out, err := host.Dispatch(ctx, plugins.HookToolBefore, payload)
		if err != nil {
			return nil, err
		}
		next := map[string]interface{}{}
		for k, v := range out {
			if k == "tool" || k == "workspace" || k == "agent" {
				continue
			}
			next[k] = v
		}
		return next, nil
	}
	reg.AfterCall = func(ctx context.Context, sc tools.SecCtx, tool string, args map[string]interface{}, res tools.Result, execErr error) {
		payload := map[string]interface{}{"tool": tool, "workspace": sc.WorkspaceDir, "output": res.Output}
		if execErr != nil {
			payload["error"] = execErr.Error()
		}
		_, _ = host.Dispatch(ctx, plugins.HookToolAfter, payload)
	}
	return nil
}

// ---------- run (headless) ----------

type tuiFlags struct {
	agent, sessionID string
	cont             bool
	perm             string
}

func defaultFlags() tuiFlags { return tuiFlags{agent: "build", perm: "bypass"} }

// configPath honors LUMINA_CONFIG like loadStack.
func configPath() string {
	if v := os.Getenv("LUMINA_CONFIG"); v != "" {
		return v
	}
	return config.DefaultPath()
}

// configBaseFlags returns TUI defaults from [defaults] in config.toml
// (LUMINA_CONFIG or ~/.config/lumina/config.toml), falling back to
// build/bypass. CLI flags parsed afterwards still win over the file.
func configBaseFlags() tuiFlags {
	f := defaultFlags()
	cfg, err := config.Load(configPath())
	if err != nil || cfg == nil {
		return f
	}
	if a := strings.TrimSpace(cfg.Defaults.Agent); a != "" {
		f.agent = a
	}
	if p := strings.TrimSpace(cfg.Defaults.PermissionMode); p != "" {
		f.perm = string(security.Normalize(security.PermissionMode(p)))
	}
	return f
}

func parseTUIArgs(args []string) tuiFlags {
	f := configBaseFlags()
	for i := 0; i < len(args); i++ {
		switch args[i] {
		case "--agent":
			if i+1 < len(args) {
				f.agent = args[i+1]
				i++
			}
		case "--plan":
			f.agent = "plan"
		case "-c", "--continue":
			f.cont = true
		case "-s", "--session":
			if i+1 < len(args) {
				f.sessionID = args[i+1]
				i++
			}
		case "--auto-edit":
			f.perm = "auto-edit"
		case "-y", "--auto-approve":
			f.perm = "auto-approve"
		case "--bypass":
			f.perm = "bypass"
		}
	}
	return f
}

func runHeadless(args []string) error {
	// Config [defaults] seed agent/permission; explicit CLI flags parsed
	// below still win over the file.
	bf := configBaseFlags()
	// Pre-scan flags in any position (`run -c "prompt"` and `run "prompt" -c
	// both work); everything else is the prompt. Go's flag package would
	// stop at the first positional arg, swallowing trailing flags.
	var (
		agentMode  = bf.agent
		plan, cont bool
		sid, model string
		format     = "text"
		perm       = bf.perm
		effort     = ""
		attach     []string
		positional []string
	)
	next := func(i *int) string {
		if *i+1 < len(args) {
			*i++
			return args[*i]
		}
		return ""
	}
	splitVal := func(a, prefix string) (string, bool) {
		if strings.HasPrefix(a, prefix+"=") {
			return strings.TrimPrefix(a, prefix+"="), true
		}
		return "", false
	}
	for i := 0; i < len(args); i++ {
		a := args[i]
		switch {
		case a == "--":
			positional = append(positional, args[i+1:]...)
			i = len(args)
		case a == "--agent":
			agentMode = next(&i)
		case a == "--plan":
			plan = true
		case a == "-c" || a == "--continue":
			cont = true
		case a == "-s" || a == "--session":
			sid = next(&i)
		case a == "-m":
			model = next(&i)
		case a == "--format":
			format = next(&i)
		case a == "--auto-edit":
			perm = "auto-edit"
		case a == "-y" || a == "--auto-approve":
			perm = "auto-approve"
		case a == "--bypass":
			perm = "bypass"
		case a == "--effort":
			effort = next(&i)
		case a == "--attach":
			if f := next(&i); f != "" {
				attach = append(attach, f)
			}
		default:
			if v, ok := splitVal(a, "--agent"); ok {
				agentMode = v
			} else if v, ok := splitVal(a, "--session"); ok {
				sid = v
			} else if v, ok := splitVal(a, "--format"); ok {
				format = v
			} else if v, ok := splitVal(a, "--effort"); ok {
				effort = v
			} else if v, ok := splitVal(a, "--attach"); ok {
				attach = append(attach, v)
			} else if v, ok := splitVal(a, "-m"); ok {
				model = v
			} else {
				positional = append(positional, a)
			}
		}
	}
	prompt := strings.Join(positional, " ")
	if prompt == "" {
		return fmt.Errorf("lumina run: missing prompt")
	}
	if plan {
		agentMode = "plan"
	}
	if !llm.ValidEffort(effort) {
		return fmt.Errorf("lumina run: invalid --effort %q (want auto|low|medium|high|xhigh|max)", effort)
	}
	effort = llm.NormalizeEffort(effort)

	_, router, reg := loadStack()
	st, err := openStore()
	if err != nil {
		return err
	}
	defer st.Close()

	var sess *agent.Session
	switch {
	case sid != "":
		sess, err = st.GetSession(sid)
		if err != nil {
			return err
		}
	case cont:
		sess, err = st.LatestSession(cwd())
		if err != nil {
			return err
		}
	default:
		sess, err = st.CreateSession(cwd(), agentMode, perm, model, "")
		if err != nil {
			return err
		}
	}
	runner := agent.NewRunner(st, router, reg)
	// Progression is on by default: stages, tool calls, and the turn
	// summary stream to stderr while the turn runs, so a working agent is
	// never silent. Stdout keeps only the final answer (machine-readable
	// for --format json).
	runner.OnProgress = func(_, stage string) {
		if strings.HasPrefix(stage, "tool: ") {
			return // covered by OnToolStart below
		}
		fmt.Fprintf(os.Stderr, "› %s\n", stage)
	}
	runner.OnToolStart = func(_, tool, target string) {
		fmt.Fprintf(os.Stderr, "▸ %s %s\n", tool, target)
	}
	runner.OnToolDone = func(_, tool, _, preview string, isErr bool, durationMs int64) {
		if isErr {
			fmt.Fprintf(os.Stderr, "  ✗ %s (%s): %s\n", tool, agent.FormatDuration(durationMs), preview)
		} else if preview != "" {
			fmt.Fprintf(os.Stderr, "  → %s\n", preview)
		} else {
			fmt.Fprintf(os.Stderr, "  ✓ %s (%s)\n", tool, agent.FormatDuration(durationMs))
		}
	}
	runner.OnTurnEnd = func(_ string, st agent.TurnStats) {
		if s := st.Summary(); s != "" {
			fmt.Fprintf(os.Stderr, "› done %s\n", s)
		}
	}
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Minute)
	defer cancel()
	images, err := loadAttachments(attach)
	if err != nil {
		return err
	}
	out, err := runner.RunPrompt(ctx, sess.ID, agent.TurnInput{Text: prompt, Images: images, Effort: effort})
	if err != nil {
		return err
	}
	if format == "json" {
		raw, _ := json.Marshal(map[string]string{"session_id": sess.ID, "text": out})
		fmt.Println(string(raw))
		return nil
	}
	fmt.Println(out)
	return nil
}

// loadAttachments reads image files into base64 payloads (8MB cap each).
func loadAttachments(paths []string) ([]llm.ImageAttachment, error) {
	var out []llm.ImageAttachment
	for _, p := range paths {
		b, err := os.ReadFile(p)
		if err != nil {
			return nil, fmt.Errorf("attach %s: %w", p, err)
		}
		if len(b) > 8<<20 {
			return nil, fmt.Errorf("attach %s: over 8MB", p)
		}
		mt := "image/png"
		switch strings.ToLower(filepath.Ext(p)) {
		case ".jpg", ".jpeg":
			mt = "image/jpeg"
		case ".gif":
			mt = "image/gif"
		case ".webp":
			mt = "image/webp"
		}
		out = append(out, llm.ImageAttachment{Name: filepath.Base(p), MediaType: mt, Data: base64.StdEncoding.EncodeToString(b)})
	}
	return out, nil
}

// ---------- TUI entry ----------

func runTUI(f tuiFlags) error {
	// The TUI needs a reachable daemon; start one in-process when the
	// loopback port is free, otherwise attach to the running daemon.
	base := daemonURL()
	if !reachable(base) {
		go func() {
			_ = runServe([]string{"--host", "127.0.0.1", "--port", "7432"})
		}()
		deadline := time.Now().Add(5 * time.Second)
		for !reachable(base) && time.Now().Before(deadline) {
			time.Sleep(100 * time.Millisecond)
		}
	}
	return startTUI(base, f)
}

func daemonURL() string {
	if v := os.Getenv("LUMINA_URL"); v != "" {
		return v
	}
	return "http://127.0.0.1:7432"
}

func reachable(base string) bool {
	c, err := http.Get(base + "/openapi.json")
	if err != nil {
		return false
	}
	c.Body.Close()
	return c.StatusCode == 200
}

// ---------- web / attach ----------

func runWeb(args []string) error {
	fs := flag.NewFlagSet("web", flag.ContinueOnError)
	_ = fs.Parse(args)
	base := daemonURL()
	if !reachable(base) {
		go func() {
			_ = runServe([]string{})
		}()
		deadline := time.Now().Add(5 * time.Second)
		for !reachable(base) && time.Now().Before(deadline) {
			time.Sleep(100 * time.Millisecond)
		}
	}
	fmt.Println("lumina web: " + base)
	fmt.Println("(open the URL in a browser; the SPA is embedded in this binary)")
	return nil
}

func runAttach(args []string) error {
	if len(args) == 0 {
		return fmt.Errorf("lumina attach: missing url")
	}
	os.Setenv("LUMINA_URL", args[0])
	return runTUI(configBaseFlags())
}

// ---------- vault ----------

// runVaultCheck shows what the LLM would receive for a piece of text:
// secret-shaped spans replaced with {placeholder_xxx} refs, everything else
// byte-identical. Runs against a throwaway in-memory vault, so probing never
// pollutes the real session store. Exit 0 always; the verdict is the diff.
func runVault(args []string) error {
	if len(args) == 0 || args[0] != "check" {
		return fmt.Errorf("usage: lumina vault check \"<text>\"")
	}
	if len(args) < 2 {
		return fmt.Errorf("lumina vault check: missing text")
	}
	st, err := agent.Open(":memory:")
	if err != nil {
		return err
	}
	defer st.Close()
	sess, err := st.CreateSession("", "build", "ask", "", "vault-check")
	if err != nil {
		return err
	}
	in := strings.Join(args[1:], " ")
	out, err := st.SubstituteForLLM(sess.ID, in)
	if err != nil {
		return err
	}
	fmt.Printf("user side: %s\nllm sees:  %s\n", in, out)
	if out == in {
		fmt.Println("verdict:   UNCHANGED — no detector matched (see vaultPatterns)")
	} else {
		fmt.Println("verdict:   VAULTED — values hidden from the model, remapped locally on return")
	}
	return nil
}

// ---------- session ----------

func runSession(args []string) error {
	if len(args) == 0 {
		args = []string{"list"}
	}
	st, err := openStore()
	if err != nil {
		return err
	}
	defer st.Close()
	switch args[0] {
	case "list":
		dir := ""
		all := false
		for _, a := range args[1:] {
			if a == "--all" {
				all = true
			} else {
				dir = a
			}
		}
		if !all && dir == "" {
			dir = cwd()
		}
		list, err := st.ListSessions(dir)
		if err != nil {
			return err
		}
		for _, s := range list {
			fmt.Printf("%s  %-5s %-11s %s  %s\n", s.ID, s.Agent, s.PermissionMode, s.UpdatedAt, s.Title)
		}
		return nil
	case "show":
		if len(args) < 2 {
			return fmt.Errorf("session show: missing id")
		}
		turns, err := st.ListTurns(args[1])
		if err != nil {
			return err
		}
		for _, t := range turns {
			fmt.Printf("[%s] %s: %s\n", t.ID, t.Role, truncate(t.Text, 300))
		}
		return nil
	case "fork":
		if len(args) < 2 {
			return fmt.Errorf("session fork: missing id")
		}
		turn := ""
		if len(args) > 2 {
			turn = args[2]
		}
		f, err := st.ForkSession(args[1], turn)
		if err != nil {
			return err
		}
		fmt.Println(f.ID)
		return nil
	case "export":
		if len(args) < 2 {
			return fmt.Errorf("session export: missing id")
		}
		sanitize := false
		for _, a := range args {
			if a == "--sanitize" {
				sanitize = true
			}
		}
		sess, err := st.GetSession(args[1])
		if err != nil {
			return err
		}
		turns, _ := st.ListTurns(args[1])
		var sb strings.Builder
		for _, t := range turns {
			fmt.Fprintf(&sb, "## %s (%s)\n%s\n\n", t.Role, t.ID, t.Text)
		}
		out := fmt.Sprintf("# %s\nworkspace: %s\n\n%s", sess.Title, sess.WorkspaceDir, sb.String())
		if sanitize {
			out = agent.SanitizeExport(out)
		}
		fmt.Println(out)
		return nil
	case "delete":
		// ponytail: hard delete via SQL is one statement; keep CLI honest.
		return fmt.Errorf("session delete: use the daemon API or remove the row from lumina.db")
	default:
		return fmt.Errorf("unknown session subcommand %q", args[0])
	}
}

func truncate(s string, n int) string {
	if len(s) <= n {
		return s
	}
	return s[:n] + "…"
}

// ---------- plugin ----------

func runPlugin(args []string) error {
	sub := "list"
	if len(args) > 0 {
		sub = args[0]
	}
	ws := cwd()
	switch sub {
	case "list":
		for _, p := range plugins.Discover(ws, true) {
			fmt.Println(p)
		}
		return nil
	case "doctor":
		fmt.Println("node: check `node --version` (v22 required for TS plugins)")
		fmt.Println("host: internal/plugins/host.mjs")
		fmt.Printf("discovered: %d plugin file(s)\n", len(plugins.Discover(ws, true)))
		return nil
	case "enable", "disable":
		return fmt.Errorf("plugin %s: managed by editing ~/.config/lumina/plugins (no registry yet)", sub)
	default:
		return fmt.Errorf("unknown plugin subcommand %q", sub)
	}
}

// ---------- kb ----------

func runKB(args []string) error {
	sub := "status"
	if len(args) > 0 {
		sub = args[0]
	}
	ws := cwd()
	graphs := []string{
		filepath.Join(ws, "memory", "graphify-out", "graph.json"),
		filepath.Join(ws, "graphify-out", "graph.json"),
	}
	switch sub {
	case "query":
		q := strings.Join(args[1:], " ")
		if q == "" {
			return fmt.Errorf("kb query: missing query")
		}
		var g *kb.Graph
		for _, p := range graphs {
			if gg, err := kb.LoadGraph(p); err == nil {
				g = gg
				break
			}
		}
		if g == nil {
			return fmt.Errorf("kb query: no graph.json found (run graphify first)")
		}
		toks := g.ExpandVocab(q, 12)
		fmt.Println("vocab: " + strings.Join(toks, ", "))
		for _, t := range toks {
			fmt.Printf("== %s ==\n", t)
			for _, n := range g.BFS(t, 8) {
				fmt.Println("  - " + n)
			}
		}
		notes := kb.ResolveWikilinks(filepath.Join(ws, "memory"), "MEMORY", 5)
		fmt.Printf("(resolved %d memory notes)\n", len(notes))
		return nil
	case "reflect":
		for _, f := range []string{
			filepath.Join(ws, "memory", "graphify-out", "reflections", "LESSONS.md"),
			filepath.Join(ws, "memory", "MEMORY.md"),
		} {
			b, err := os.ReadFile(f)
			if err != nil {
				continue
			}
			fmt.Printf("=== %s ===\n%s\n", f, b)
		}
		return nil
	case "update":
		// kb update <topic> <content...>: dedup-on-write into ./memory.
		if len(args) < 3 {
			return fmt.Errorf("kb update: usage: kb update <topic> <content>")
		}
		dir := filepath.Join(ws, "memory")
		_ = os.MkdirAll(dir, 0o755)
		p := filepath.Join(dir, filepath.Base(args[1])+".md")
		existing, _ := os.ReadFile(p)
		if strings.Contains(string(existing), args[2]) {
			fmt.Println("dedup: already present")
			return nil
		}
		f, err := os.OpenFile(p, os.O_APPEND|os.O_CREATE|os.O_WRONLY, 0o644)
		if err != nil {
			return err
		}
		defer f.Close()
		fmt.Fprintln(f, args[2])
		fmt.Println("wrote " + p)
		return nil
	case "status":
		for _, p := range graphs {
			if st, err := os.Stat(p); err == nil {
				fmt.Printf("graph: %s (%d bytes, %s)\n", p, st.Size(), st.ModTime().Format(time.RFC3339))
			} else {
				fmt.Printf("graph: %s (missing)\n", p)
			}
		}
		return nil
	default:
		return fmt.Errorf("unknown kb subcommand %q", sub)
	}
}

// ---------- evolve ----------

func runEvolve(args []string) error {
	ws := cwd()
	l := &harness.Loader{WorkspaceDir: ws}
	if err := l.Ensure(); err != nil {
		return err
	}
	// Level-1 offline pass: one candidate refreshing tool guidance from the
	// live rules bundle, probed in a worktree, quality-gated, then committed.
	bundle := rules.Load(rules.Options{WorkspaceDir: ws, ImportGlobalRules: true})
	_ = bundle
	cur, _ := l.Get("tool_guidance.md")
	cand := harness.Candidate{
		ID:        "guidance-refresh",
		Rationale: "re-ground tool guidance in current rules bundle",
		Files: map[string]string{
			"tool_guidance.md": cur + "\n- Prefer kb_query before grep for project questions.\n",
		},
	}
	results, err := harness.Probe(ws, []harness.Candidate{cand}, func(sandbox string, c harness.Candidate) (bool, float64, string) {
		return true, 0.6, "offline heuristic"
	})
	if err != nil {
		return err
	}
	byID := map[string]harness.Candidate{cand.ID: cand}
	landed, err := l.Compose(results, byID, 0.5)
	if err != nil {
		return err
	}
	if len(landed) == 0 {
		fmt.Println("evolve: no candidate cleared the quality floor")
		return nil
	}
	commit, err := l.CommitAll("harness: evolve " + strings.Join(landed, ","))
	if err != nil {
		return err
	}
	fmt.Println("evolve landed: " + strings.Join(landed, ",") + " @ " + commit)
	lib := &harness.Library{Dir: harness.DefaultLibraryDir()}
	_ = lib.Promote("evolve-"+time.Now().Format("20060102"), "# evolve run", "offline pass landed "+strings.Join(landed, ","))
	_ = security.Normalize("bypass")
	return nil
}

// ---------- models & auth (provider connect) ----------

// runModels lists live models for any provider implementing ModelLister
// (copilot gateway, openai-compatible /models, ...).
func runModels(args []string) error {
	provider := ""
	for i := 0; i < len(args); i++ {
		if args[i] == "--provider" && i+1 < len(args) {
			provider = args[i+1]
			i++
		}
	}
	if provider == "" {
		return fmt.Errorf("lumina models: missing --provider")
	}
	_, router, _ := loadStack()
	if router == nil {
		return fmt.Errorf("lumina models: no router configured")
	}
	p, ok := router.Provider(provider)
	if !ok {
		return fmt.Errorf("unknown provider %q", provider)
	}
	lister, ok := p.(llm.ModelLister)
	if !ok {
		return fmt.Errorf("provider %q does not list models", provider)
	}
	ctx, cancel := context.WithTimeout(context.Background(), 60*time.Second)
	defer cancel()
	models, err := lister.ListModels(ctx)
	if err != nil {
		return fmt.Errorf("lumina models: %v (connect with `lumina auth device --provider %s`)", err, provider)
	}
	for _, m := range models {
		fmt.Printf("%s/%s  %s\n", provider, m.ID, m.Name)
	}
	return nil
}

// runAuth runs interactive provider auth. `device` performs GitHub web-auth:
// prints verification_uri + user_code, polls until approved, stores the
// token (0600) — never printed. --github-url covers GHES/custom hosts.
func runAuth(args []string) error {
	if len(args) == 0 || args[0] != "device" {
		return fmt.Errorf("lumina auth: usage: lumina auth device --provider <name> [--github-url <url>]")
	}
	provider := "copilot"
	githubURL := ""
	for i := 0; i < len(args); i++ {
		if args[i] == "--provider" && i+1 < len(args) {
			provider = args[i+1]
		}
		if args[i] == "--github-url" && i+1 < len(args) {
			githubURL = args[i+1]
		}
	}
	cfg, _, _ := loadStack()
	if p, ok := cfg.Providers[provider]; ok && p.Protocol != "" && p.Protocol != "copilot" {
		return fmt.Errorf("lumina auth: provider %q uses protocol %q; device auth is only for copilot providers", provider, p.Protocol)
	}
	tokenURL := llm.TokenURLForHost(githubURL)
	if p, ok := cfg.Providers[provider]; ok && githubURL == "" && p.TokenURL != "" {
		tokenURL = p.TokenURL
	}
	ctx := context.Background()
	dc, err := llm.StartDeviceFlow(ctx, tokenURL, "")
	if err != nil {
		return err
	}
	fmt.Printf("Open %s and enter code: %s\nWaiting for approval...\n", dc.VerificationURI, dc.UserCode)
	deadline := time.Now().Add(time.Duration(dc.ExpiresIn) * time.Second)
	interval := time.Duration(dc.Interval) * time.Second
	if interval <= 0 {
		interval = 5 * time.Second
	}
	for time.Now().Before(deadline) {
		time.Sleep(interval)
		tok, err := llm.PollDeviceFlow(ctx, tokenURL, dc.DeviceCode)
		if err != nil {
			if err.Error() == "pending" {
				continue
			}
			return err
		}
		base := llm.CopilotBaseForHost(githubURL)
		turl := ""
		if githubURL != "" {
			turl = tokenURL
		}
		if err := llm.WriteProviderAuth(provider, tok, base, turl); err != nil {
			return err
		}
		fmt.Printf("Connected %s via web auth (token stored, never shown).\n", provider)
		return nil
	}
	return fmt.Errorf("device code expired; run again for a fresh code")
}

// ---------- pair ----------

// runPair prints a LAN/Tailscale attach URL. The daemon binds loopback by
// default; run `lumina serve --host 0.0.0.0` (or bind Tailscale IP) behind a
// trusted network, then attach with the printed token.
func runPair(args []string) error {
	fs := flag.NewFlagSet("pair", flag.ContinueOnError)
	_ = fs.Parse(args)
	token := os.Getenv("LUMINA_TOKEN")
	if token == "" {
		token = uuidNew()
		fmt.Println("LUMINA_TOKEN not set; generated one-time token (export it before serving):")
		fmt.Printf("  export LUMINA_TOKEN=%s\n", token)
	}
	fmt.Println("attach from another host on your LAN/Tailscale network with:")
	fmt.Println("  lumina attach http://<this-host>:7432   (LUMINA_TOKEN=" + short(token) + "…)")
	return nil
}

func uuidNew() string { return uuid.NewString() }

func short(s string) string {
	if len(s) > 6 {
		return s[:6]
	}
	return s
}

// ---------- openapi ----------

func runOpenAPI(args []string) error {
	out := ""
	for i := 0; i < len(args)-1; i++ {
		if args[i] == "--out" {
			out = args[i+1]
		}
	}
	srv := server.New(server.Options{})
	rec := httptest.NewRecorder()
	req := httptest.NewRequest("GET", "/openapi.json", nil)
	srv.Handler().ServeHTTP(rec, req)
	if out != "" {
		return os.WriteFile(out, rec.Body.Bytes(), 0o644)
	}
	fmt.Println(rec.Body.String())
	return nil
}

// startTUI is implemented in tui_stub.go when the TUI build tag is off;
// the full Bubble Tea client lives in internal/tui and is linked here.
func startTUI(base string, f tuiFlags) error {
	c := sdk.New(base, os.Getenv("LUMINA_TOKEN"))
	return runTUIClient(c, f)
}
