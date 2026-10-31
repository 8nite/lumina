package main

import (
	"context"
	"encoding/json"
	"fmt"
	"net/http"
	"net/http/httptest"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/8nite/lumina/internal/agent"
	"github.com/8nite/lumina/internal/config"
	"github.com/8nite/lumina/internal/harness"
	"github.com/8nite/lumina/internal/kb"
	"github.com/8nite/lumina/internal/llm"
	"github.com/8nite/lumina/internal/mcp"
	"github.com/8nite/lumina/internal/plugins"
	"github.com/8nite/lumina/internal/rules"
	"github.com/8nite/lumina/internal/security"
	"github.com/8nite/lumina/internal/tools"
	"github.com/8nite/lumina/internal/tui"
	"github.com/8nite/lumina/pkg/sdk"
	tea "github.com/charmbracelet/bubbletea"
)

func verdict(t *testing.T, id string, pass bool, detail string) {
	status := "PASS"
	if !pass {
		status = "FAIL"
	}
	t.Logf("[VERDICT] %s | %s | %s", id, status, detail)
}

func TestVerificationMatrix(t *testing.T) {
	repoRoot, _ := filepath.Abs("../..")

	// --- TP-01: Config, Secret Indirection & No-Google ---
	{
		cfgPath := filepath.Join(t.TempDir(), "missing.toml")
		c, err := config.Load(cfgPath)
		verdict(t, "TP-01.1", err == nil && c.Server.Host == "127.0.0.1" && c.Server.Port == 7432 && c.Defaults.PermissionMode == "ask" && c.KB.StandingOps && c.KB.IdleDebounceMinutes == 15,
			fmt.Sprintf("defaults host=%s:%d perm=%s standing_ops=%v debounce=%d", c.Server.Host, c.Server.Port, c.Defaults.PermissionMode, c.KB.StandingOps, c.KB.IdleDebounceMinutes))

		vCmd, errCmd := config.ResolveSecret(config.ProviderConfig{APIKeyCmd: "printf test-cmd-tok"})
		verdict(t, "TP-01.5", errCmd == nil && vCmd == "test-cmd-tok", fmt.Sprintf("api_key_cmd resolved len=%d err=%v", len(vCmd), errCmd))

		pPersist := filepath.Join(t.TempDir(), "config.toml")
		c2, _ := config.Load(pPersist)
		errP := c2.PersistModel("executor", "openrouter/qwen/qwen3-coder")
		c3, errL := config.Load(pPersist)
		verdict(t, "TP-01.6", errP == nil && errL == nil && c3.Router.Executor == "openrouter/qwen/qwen3-coder",
			fmt.Sprintf("PersistModel roundtrip executor=%q errP=%v errL=%v", c3.Router.Executor, errP, errL))
	}

	// --- TP-02: Security Fence & Adversarial Probes ---
	{
		ws := t.TempDir()
		reg := tools.NewRegistry()
		scPlanBypass := tools.SecCtx{Agent: security.AgentPlan, Permission: security.PermBypass, WorkspaceDir: ws}

		// TP-02.2: out-of-workspace plan.md escape probe
		escapePlan := filepath.Join(t.TempDir(), "plan.md")
		_, errEscPlan := reg.Call(context.Background(), scPlanBypass, "write", map[string]interface{}{"path": escapePlan, "content": "escaped"})
		verdict(t, "TP-02.2", errEscPlan != nil, fmt.Sprintf("out-of-workspace %s write err=%v", escapePlan, errEscPlan))

		// TP-02.3: plan bash shell chaining / redirection probes
		advCmds := []string{
			"echo pwned > a.go",
			"ls; touch pwned",
			"git status && touch pwned",
			"cat README.md | tee pwned",
			"echo $(touch pwned)",
		}
		var allowedAdv []string
		for _, cmd := range advCmds {
			if d := security.Authorize(security.AgentPlan, security.PermBypass, "bash", cmd, "/ws", nil, false); d == security.Allow {
				allowedAdv = append(allowedAdv, cmd)
			}
		}
		verdict(t, "TP-02.3", len(allowedAdv) == 0, fmt.Sprintf("adversarial plan bash allowed=%v", allowedAdv))

		// TP-02.4: credential hard-deny including .kube/config and grep walk
		kubeDeny := security.Authorize(security.AgentBuild, security.PermBypass, "read", "/home/u/.kube/config", "/ws", nil, false) == security.Deny
		_ = os.WriteFile(filepath.Join(ws, ".env"), []byte("TOP_SECRET_MARKER=12345\n"), 0o600)
		_ = os.WriteFile(filepath.Join(ws, "ok.txt"), []byte("TOP_SECRET_MARKER=safe\n"), 0o644)
		scBuildBypass := tools.SecCtx{Agent: security.AgentBuild, Permission: security.PermBypass, WorkspaceDir: ws}
		grepRes, _ := reg.Call(context.Background(), scBuildBypass, "grep", map[string]interface{}{"pattern": "TOP_SECRET_MARKER"})
		grepLeakedEnv := strings.Contains(grepRes.Output, ".env:")
		verdict(t, "TP-02.4", kubeDeny && !grepLeakedEnv,
			fmt.Sprintf(".kube/config_deny=%v grep_leaked_.env=%v (grep out=%q)", kubeDeny, grepLeakedEnv, strings.TrimSpace(grepRes.Output)))

		// TP-02.5: workspace escape via "file" alias vs "path"
		// NOTE: must use a non-whitelisted absolute path. t.TempDir()
		// (/tmp/...) is intentionally writable per IsWhitelistedWrite,
		// so it cannot prove the escape fence (see TestFileAliasEscapeBlocked
		// which uses /var/empty for the same reason).
		outsideFile := "/var/empty/lumina-escape-test.txt"
		_, errPath := reg.Call(context.Background(), scBuildBypass, "write", map[string]interface{}{"path": outsideFile, "content": "x"})
		_, errFileAlias := reg.Call(context.Background(), scBuildBypass, "write", map[string]interface{}{"file": outsideFile, "content": "x"})
		verdict(t, "TP-02.5", errPath != nil && errFileAlias != nil,
			fmt.Sprintf("escape blocked: path_err=%v file_alias_err=%v", errPath != nil, errFileAlias != nil))

		// TP-02.6: full build matrix including destructive bash, mcp, tooling docs
		dDestructAuto := security.Authorize(security.AgentBuild, security.PermAutoApprove, "bash", "rm -rf build/", "/ws", nil, false)
		dDestructBypass := security.Authorize(security.AgentBuild, security.PermBypass, "bash", "rm -rf build/", "/ws", nil, false)
		dMCPAuto := security.Authorize(security.AgentBuild, security.PermAutoApprove, "mcp", "srv", "/ws", nil, false)
		dMCPBypass := security.Authorize(security.AgentBuild, security.PermBypass, "mcp", "srv", "/ws", nil, false)
		dAgentsEdit := security.Authorize(security.AgentBuild, security.PermBypass, "edit", "AGENTS.md", "/ws", nil, false)
		verdict(t, "TP-02.6",
			dDestructAuto == security.Ask && dDestructBypass == security.Allow &&
				dMCPAuto == security.Ask && dMCPBypass == security.Allow &&
				dAgentsEdit == security.Deny,
			fmt.Sprintf("rm-rf(auto=%s,bypass=%s) mcp(auto=%s,bypass=%s) AGENTS.md_edit=%s", dDestructAuto, dDestructBypass, dMCPAuto, dMCPBypass, dAgentsEdit))
	}

	// --- TP-03: BYO-LLM Adapters (OpenAI tool_calls, Responses tool_calls, Anthropic, Ollama) ---
	{
		// OpenAI tool_calls + reasoning
		oaiSrv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
			w.Header().Set("Content-Type", "text/event-stream")
			fmt.Fprint(w, `data: {"choices":[{"delta":{"reasoning_content":"think..","tool_calls":[{"id":"c1","function":{"name":"read","arguments":"{\"path\":\"a.txt\"}"}}]}}]}`+"\n\n")
			fmt.Fprint(w, `data: {"choices":[{"delta":{"content":"done"},"finish_reason":"tool_calls"}]}`+"\n\n")
		}))
		defer oaiSrv.Close()
		txt, calls, err := llm.Complete(context.Background(), &llm.OpenAICompat{BaseURL: oaiSrv.URL}, llm.Request{Model: "m"})
		verdict(t, "TP-03.1", err == nil && strings.Contains(txt, "think..") && len(calls) == 1 && calls[0].Name == "read" && calls[0].Args["path"] == "a.txt",
			fmt.Sprintf("openai txt=%q calls=%+v err=%v", txt, calls, err))

		// Responses function_call_arguments.delta accumulation
		respSrv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
			w.Header().Set("Content-Type", "text/event-stream")
			fmt.Fprint(w, "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"call_id\":\"fc1\",\"name\":\"bash\"}}\n\n")
			fmt.Fprint(w, "data: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":0,\"delta\":\"{\\\"command\\\":\\\"ls\"}\n\n")
			fmt.Fprint(w, "data: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":0,\"delta\":\" -la\\\"}\"}\n\n")
			fmt.Fprint(w, "data: {\"type\":\"response.completed\"}\n\n")
		}))
		defer respSrv.Close()
		_, rCalls, rErr := llm.Complete(context.Background(), &llm.Responses{BaseURL: respSrv.URL}, llm.Request{Model: "m"})
		verdict(t, "TP-03.2", rErr == nil && len(rCalls) == 1 && rCalls[0].Args["command"] == "ls -la",
			fmt.Sprintf("responses calls=%+v err=%v", rCalls, rErr))

		// Anthropic SSE + check whether tools are sent in body
		var anthBody map[string]interface{}
		anthSrv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
			_ = json.NewDecoder(r.Body).Decode(&anthBody)
			w.Header().Set("Content-Type", "text/event-stream")
			fmt.Fprint(w, "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"hmm \"}}\n\n")
			fmt.Fprint(w, "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n")
			fmt.Fprint(w, "data: {\"type\":\"message_stop\"}\n\n")
		}))
		defer anthSrv.Close()
		aTxt, _, aErr := llm.Complete(context.Background(), &llm.Anthropic{BaseURL: anthSrv.URL, APIKey: "k"}, llm.Request{
			Model: "claude", Tools: []llm.ToolSpec{{Name: "read", Description: "read"}},
		})
		_, anthSentTools := anthBody["tools"]
		verdict(t, "TP-03.3", aErr == nil && aTxt == "hmm hi" && anthSentTools,
			fmt.Sprintf("anthropic stream=%q sent_tools_schema=%v", aTxt, anthSentTools))

		// Ollama NDJSON
		ollSrv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
			fmt.Fprintln(w, `{"message":{"content":"local "},"done":false}`)
			fmt.Fprintln(w, `{"message":{"content":"ok"},"done":true}`)
		}))
		defer ollSrv.Close()
		oTxt, _, oErr := llm.Complete(context.Background(), &llm.Ollama{BaseURL: ollSrv.URL}, llm.Request{Model: "qwen"})
		verdict(t, "TP-03.4", oErr == nil && oTxt == "local ok", fmt.Sprintf("ollama txt=%q err=%v", oTxt, oErr))
	}

	// --- TP-04: Agent Multi-Round Tool Loop, Tool Schemas, KB/Skill Wiring & MCP ---
	{
		ws := t.TempDir()
		st, _ := agent.Open(filepath.Join(t.TempDir(), "s.db"))
		defer st.Close()

		var reqTools []interface{}
		round := 0
		mockLLM := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
			if r.URL.Path == "/models" {
				// Runner's context-window lookup; not part of the round count.
				w.Header().Set("Content-Type", "application/json")
				fmt.Fprint(w, `{"data":[]}`)
				return
			}
			var body map[string]interface{}
			_ = json.NewDecoder(r.Body).Decode(&body)
			if ts, ok := body["tools"].([]interface{}); ok {
				reqTools = ts
			}
			w.Header().Set("Content-Type", "text/event-stream")
			round++
			if round == 1 {
				fmt.Fprint(w, `data: {"choices":[{"delta":{"tool_calls":[{"id":"1","function":{"name":"write","arguments":"{\"path\":\"note.txt\",\"content\":\"hello-loop\"}"}}]},"finish_reason":"tool_calls"}]}`+"\n\n")
				return
			}
			fmt.Fprint(w, `data: {"choices":[{"delta":{"content":"loop-complete"},"finish_reason":"stop"}]}`+"\n\n")
		}))
		defer mockLLM.Close()

		cfg := config.Defaults()
		cfg.Providers = map[string]config.ProviderConfig{"mock": {Protocol: "openai-compatible", BaseURL: mockLLM.URL}}
		cfg.Router.Executor = "mock/test-model"
		rtr, _ := llm.NewRouter(cfg)
		_, _, stackReg := loadStack()
		sess, _ := st.CreateSession(ws, "build", "auto-edit", "mock/test-model", "")
		runner := agent.NewRunner(st, rtr, stackReg)
		reply, err := runner.RunPrompt(context.Background(), sess.ID, agent.TurnInput{Text: "write note"})
		written, _ := os.ReadFile(filepath.Join(ws, "note.txt"))
		hasParams := false
		if len(reqTools) > 0 {
			if fn, ok := reqTools[0].(map[string]interface{})["function"].(map[string]interface{}); ok && fn["parameters"] != nil {
				hasParams = true
			}
		}
		kbWired := stackReg.KBQuery != nil
		skillWired := stackReg.Skill != nil
		verdict(t, "TP-04.4",
			err == nil && reply == "loop-complete" && string(written) == "hello-loop" && hasParams && kbWired && skillWired,
			fmt.Sprintf("loop_ok=%v file=%q tool_json_schema=%v kb_wired=%v skill_wired=%v",
				reply == "loop-complete", string(written), hasParams, kbWired, skillWired))

		// TP-04.6: MCP SSE + Google URL block check
		mcpSSE := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
			_ = json.NewEncoder(w).Encode(map[string]interface{}{
				"jsonrpc": "2.0", "id": 1,
				"result": map[string]interface{}{"tools": []map[string]string{{"name": "ping", "description": "pong"}}},
			})
		}))
		defer mcpSSE.Close()
		mc, _ := mcp.Dial(mcp.ServerConf{Transport: "sse", URL: mcpSSE.URL})
		mTools, mErr := mc.ListTools(context.Background())
		_, gErr := mcp.Dial(mcp.ServerConf{Transport: "sse", URL: "https://generativelanguage.googleapis.com/mcp"})
		verdict(t, "TP-04.6", mErr == nil && len(mTools) == 1 && gErr != nil,
			fmt.Sprintf("mcp_sse_ok=%v google_sse_blocked=%v", mErr == nil && len(mTools) == 1, gErr != nil))
	}

	// --- TP-05: Rules 4-Layer Precedence & Out-of-Repo Plugin Host ---
	{
		home := t.TempDir()
		t.Setenv("HOME", home)
		ws := t.TempDir()
		_ = os.MkdirAll(filepath.Join(ws, ".git"), 0o755)
		sub := filepath.Join(ws, "pkg", "sub")
		_ = os.MkdirAll(sub, 0o755)
		_ = os.MkdirAll(filepath.Join(home, ".config", "lumina"), 0o755)
		_ = os.WriteFile(filepath.Join(home, ".config", "lumina", "AGENTS.md"), []byte("L1-GLOBAL"), 0o644)
		_ = os.WriteFile(filepath.Join(ws, "AGENTS.md"), []byte("L2-ROOT"), 0o644)
		_ = os.WriteFile(filepath.Join(sub, "AGENTS.md"), []byte("L2-SUB"), 0o644)
		_ = os.MkdirAll(filepath.Join(sub, ".lumina", "harness"), 0o755)
		_ = os.WriteFile(filepath.Join(sub, ".lumina", "harness", "system_prompt.md"), []byte("L3-HARNESS"), 0o644)
		b := rules.Load(rules.Options{WorkspaceDir: sub, ImportGlobalRules: true})
		i0 := strings.Index(b.SystemPrompt, "Layer 0")
		i1 := strings.Index(b.SystemPrompt, "L1-GLOBAL")
		i2r := strings.Index(b.SystemPrompt, "L2-ROOT")
		i2s := strings.Index(b.SystemPrompt, "L2-SUB")
		i3 := strings.Index(b.SystemPrompt, "L3-HARNESS")
		verdict(t, "TP-05.1", i0 >= 0 && i0 < i1 && i1 < i2r && i2r < i2s && i2s < i3,
			fmt.Sprintf("layer indices L0=%d L1=%d L2root=%d L2sub=%d L3=%d", i0, i1, i2r, i2s, i3))

		// TP-05.5: Plugin Host from outside repo cwd (without LUMINA_PLUGIN_HOST env)
		origWD, _ := os.Getwd()
		outsideDir := t.TempDir()
		plugPath := filepath.Join(outsideDir, "p.mjs")
		_ = os.WriteFile(plugPath, []byte(`export default async () => ({ "tool.execute.before": async () => ({ patch: { injected: true } }) });`), 0o644)
		_ = os.Chdir(outsideDir)
		os.Unsetenv("LUMINA_PLUGIN_HOST")
		ph := plugins.NewHost([]string{plugPath})
		pOut, pErr := ph.Dispatch(context.Background(), plugins.HookToolBefore, map[string]interface{}{"tool": "read"})
		_ = ph.Close()
		_ = os.Chdir(origWD)
		verdict(t, "TP-05.5", pErr == nil && pOut["injected"] == true,
			fmt.Sprintf("out-of-repo plugin dispatch injected=%v err=%v", pOut["injected"], pErr))
	}

	// --- TP-06: Real memory/graphify-out/graph.json <2ms Traversal ---
	{
		realGraphPath := filepath.Join(repoRoot, "memory", "graphify-out", "graph.json")
		t0 := time.Now()
		g, err := kb.LoadGraph(realGraphPath)
		var bfs []string
		var gods []string
		if err == nil && len(g.Nodes) > 0 {
			bfs = g.BFS(g.Nodes[0].ID, 20)
			gods = g.GodNodes(3)
		}
		dur := time.Since(t0)
		verdict(t, "TP-06.2", err == nil && len(g.Nodes) >= 10 && dur < 2*time.Millisecond,
			fmt.Sprintf("real graph nodes=%d edges=%d bfs=%d gods=%v latency=%v", len(g.Nodes), len(g.Edges), len(bfs), gods, dur))
	}

	// --- TP-07: Harness e2e-*.sh Rejection Gate ---
	{
		ws := t.TempDir()
		for _, args := range [][]string{{"init"}, {"config", "user.email", "t@t"}, {"config", "user.name", "t"}} {
			c := exec.Command("git", args...)
			c.Dir = ws
			_ = c.Run()
		}
		_ = os.WriteFile(filepath.Join(ws, "README.md"), []byte("hi"), 0o644)
		cAdd := exec.Command("git", "add", "-A")
		cAdd.Dir = ws
		_ = cAdd.Run()
		cCom := exec.Command("git", "commit", "-m", "init")
		cCom.Dir = ws
		_ = cCom.Run()

		failE2E := harness.Candidate{
			ID: "broken-e2e",
			Files: map[string]string{
				"scripts/e2e-fail.sh": "#!/bin/sh\necho 'failing gate'\nexit 1\n",
			},
		}
		res, err := harness.Probe(ws, []harness.Candidate{failE2E}, func(sandbox string, c harness.Candidate) (bool, float64, string) {
			return true, 1.0, "should not reach"
		})
		verdict(t, "TP-07.2", err == nil && len(res) == 1 && !res[0].Pass && strings.Contains(res[0].Detail, "failing gate"),
			fmt.Sprintf("e2e-*.sh gate blocked broken candidate: pass=%v detail=%q", res[0].Pass, strings.TrimSpace(res[0].Detail)))
	}

	// --- TP-09: TUI Daemon Sync & Cross-Session Event Filtering ---
	{
		var patched int
		var permAnswered int
		srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
			if r.Method == http.MethodPatch && strings.HasPrefix(r.URL.Path, "/api/v1/sessions/") {
				patched++
				_ = json.NewEncoder(w).Encode(sdk.Session{ID: "s1", Agent: "plan", PermissionMode: "ask"})
				return
			}
			if r.Method == http.MethodPost && strings.HasPrefix(r.URL.Path, "/api/v1/permissions/") {
				permAnswered++
				_ = json.NewEncoder(w).Encode(map[string]string{"status": "ok"})
				return
			}
			w.WriteHeader(200)
		}))
		defer srv.Close()

		cl := sdk.New(srv.URL, "")
		m := tui.New(cl, &sdk.Session{ID: "s1", Agent: "build", PermissionMode: "ask"})
		// Tab triggers pushAgentCmd
		nextM, cmd := m.Update(tea.KeyMsg{Type: tea.KeyTab})
		if cmd != nil {
			_ = cmd()
		}
		m = nextM.(tui.Model)
		// /plan slash command — check if it also syncs to daemon!
		slashCmd := m.ApplyCommand("/build")
		if slashCmd != nil {
			_ = slashCmd()
		}
		verdict(t, "TP-09.1", patched >= 1, fmt.Sprintf("Tab PATCH count=%d", patched))
		verdict(t, "TP-09.2", slashCmd != nil, fmt.Sprintf("/build returned daemon sync cmd=%v", slashCmd != nil))

		// Permission modal enter -> check if AnswerPermission called on daemon
		m.Modal = &tui.PermModal{RequestID: "req-1", Tool: "bash", Target: "ls"}
		nextM2, modalCmd := m.Update(tea.KeyMsg{Type: tea.KeyEnter})
		if modalCmd != nil {
			_ = modalCmd()
		}
		m = nextM2.(tui.Model)

		// Cross-session turn.completed event -> should NOT append to s1's lines
		beforeLines := len(m.Lines)
		nextM3, _ := m.Update(tui.EventMsg{Event: sdk.Event{
			Type: "turn.completed", SessionID: "other-session",
			Data: json.RawMessage(`{"text":"foreign reply"}`),
		}})
		m = nextM3.(tui.Model)
		foreignIgnored := len(m.Lines) == beforeLines
		verdict(t, "TP-09.4", permAnswered == 1 && foreignIgnored,
			fmt.Sprintf("modal_daemon_answer_count=%d foreign_session_ignored=%v", permAnswered, foreignIgnored))
	}
}

func TestCopilotMatrix(t *testing.T) {
	// --- TP-11: Copilot exchange, device auth, /model wiring (all mocked) ---
	var exchanges int
	var chatURL string
	chat := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Header.Get("Copilot-Integration-Id") == "" {
			t.Error("missing Copilot-Integration-Id")
		}
		w.Header().Set("Content-Type", "text/event-stream")
		fmt.Fprint(w, `data: {"choices":[{"delta":{"content":"copilot-hi"},"finish_reason":"stop"}]}`+"\n\n")
	}))
	defer chat.Close()
	chatURL = chat.URL
	tok := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		exchanges++
		w.Header().Set("Content-Type", "application/json")
		fmt.Fprintf(w, `{"token":"sess-matrix","expires_at":%d,"endpoints":{"api":%q}}`, time.Now().Unix()+1800, chatURL)
	}))
	defer tok.Close()

	cp := &llm.Copilot{GitHubToken: "ghu-matrix", TokenURL: tok.URL}
	txt, _, err := llm.Complete(context.Background(), cp, llm.Request{Model: "m", Messages: []llm.Message{{Role: "user", Content: "hi"}}})
	_, _, _ = llm.Complete(context.Background(), cp, llm.Request{Model: "m", Messages: []llm.Message{{Role: "user", Content: "hi"}}})
	verdict(t, "TP-11.1", err == nil && txt == "copilot-hi" && exchanges == 1,
		fmt.Sprintf("chat=%q exchanges=%d (cached)", txt, exchanges))

	// Enterprise override wins over exchanged endpoint.
	cpEnt := &llm.Copilot{GitHubToken: "ghu-matrix", TokenURL: tok.URL, BaseURL: chatURL}
	txtEnt, _, errEnt := llm.Complete(context.Background(), cpEnt, llm.Request{Model: "m"})
	verdict(t, "TP-11.1b", errEnt == nil && txtEnt == "copilot-hi",
		fmt.Sprintf("enterprise base override chat=%q err=%v", txtEnt, errEnt))

	// Device flow against a mock host + 0600 store roundtrip.
	dev := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		if r.URL.Path == "/login/device/code" {
			fmt.Fprint(w, `{"device_code":"d1","user_code":"U1","verification_uri":"https://example.test/d","expires_in":900,"interval":1}`)
			return
		}
		fmt.Fprint(w, `{"access_token":"ghu-matrix-device"}`)
	}))
	defer dev.Close()
	home := t.TempDir()
	t.Setenv("HOME", home)
	dc, err := llm.StartDeviceFlow(context.Background(), dev.URL+"/x", "")
	stored := ""
	if err == nil {
		tok2, perr := llm.PollDeviceFlow(context.Background(), dev.URL+"/x", dc.DeviceCode)
		if perr == nil {
			stored = tok2
			_ = llm.WriteTokenStore("copilot", tok2)
		}
	}
	_, rerr := os.ReadFile(filepath.Join(home, ".config", "lumina", "tokens", "copilot.json"))
	verdict(t, "TP-11.2", err == nil && dc.UserCode == "U1" && stored == "ghu-matrix-device" && rerr == nil,
		fmt.Sprintf("device code=%q stored_ok=%v", dc.UserCode, rerr == nil))

	// /model wiring: sdk ListModels + PATCH + PUT against a mock daemon.
	var patched, routerPut int
	dsrv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		switch {
		case r.URL.Path == "/api/v1/models":
			_ = json.NewEncoder(w).Encode(map[string]interface{}{"models": []map[string]string{{"id": "copilot/gpt-4o", "name": "GPT-4o"}}})
		case r.Method == http.MethodPatch:
			patched++
			_ = json.NewEncoder(w).Encode(sdk.Session{ID: "s1"})
		case r.Method == http.MethodPut:
			routerPut++
			_ = json.NewEncoder(w).Encode(map[string]string{"status": "ok"})
		default:
			w.WriteHeader(200)
		}
	}))
	defer dsrv.Close()
	cl := sdk.New(dsrv.URL, "")
	ms, merr := cl.ListModels(context.Background(), "copilot")
	_, _ = cl.UpdateSession(context.Background(), "s1", map[string]string{"model": "copilot/gpt-4o"})
	_ = cl.SetRouterModel(context.Background(), "executor", "copilot/gpt-4o")
	verdict(t, "TP-11.3", merr == nil && len(ms) == 1 && ms[0].ID == "copilot/gpt-4o" && patched == 1 && routerPut == 1,
		fmt.Sprintf("models=%v patched=%d routerPut=%d", ms, patched, routerPut))
}

func TestConfigDefaultsSeedFlags(t *testing.T) {
	// [defaults] permission_mode/agent in config.toml must seed TUI and
	// headless runs; explicit CLI flags still win over the file.
	dir := t.TempDir()
	cfgPath := filepath.Join(dir, "config.toml")
	if err := os.WriteFile(cfgPath, []byte("[defaults]\nagent = \"plan\"\npermission_mode = \"bypass\"\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	t.Setenv("LUMINA_CONFIG", cfgPath)
	if f := configBaseFlags(); f.agent != "plan" || f.perm != "bypass" {
		t.Fatalf("config defaults not applied: %+v", f)
	}
	if g := parseTUIArgs([]string{"--auto-edit"}); g.perm != "auto-edit" || g.agent != "plan" {
		t.Fatalf("CLI flag must win over config: %+v", g)
	}
	t.Setenv("LUMINA_CONFIG", filepath.Join(dir, "missing.toml"))
	if h := configBaseFlags(); h.agent != "build" || h.perm != "bypass" {
		t.Fatalf("missing config must fall back to build/bypass: %+v", h)
	}
}
