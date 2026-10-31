//! Agentic turn loop: prompt → stream → tool calls → repeat.
//! Mirrors `internal/agent`: sessions, subagents, compaction, checkpoints.
use crate::{
    approve::ApprovalGate,
    llm::{self, Chunk, ImageAttachment, Request, Router, ToolSpec},
    plugins::Host,
    rules::Bundle,
    security::{Agent, PermissionMode},
    store::Store,
    tools::{Registry, SecCtx},
};
use std::sync::Arc;
use std::{collections::HashMap, path::PathBuf};
use thiserror::Error;
use tokio::sync::mpsc;

#[derive(Debug, Error)]
pub enum AgentError {
    #[error("agent: {0}")]
    Msg(String),
    #[error(transparent)]
    Llm(#[from] llm::LlmError),
    #[error(transparent)]
    Store(#[from] crate::store::StoreError),
    #[error(transparent)]
    Tool(#[from] crate::tools::ToolError),
}

pub struct Runner {
    pub max_steps: usize,
    /// Live approval broker (server/TUI mode). None = headless `run`.
    pub approval: Option<ApprovalGate>,
    /// Lumina-compatible JS/TS plugin worker (None when unconfigured).
    pub plugins: Option<Arc<Host>>,
}

impl Runner {
    pub fn new() -> Self {
        // Auto-continue ceiling: must match Go hardCap (120). The old 16
        // ended long tasks with a silent stop that forced `continue`.
        Self { max_steps: 120, approval: None, plugins: None }
    }

    fn tool_specs() -> Vec<ToolSpec> {
        Self::tool_specs_filtered(false)
    }

    fn tool_specs_filtered(hide_websearch: bool) -> Vec<ToolSpec> {
        let obj = |props: &[&str]| {
            serde_json::json!({
                "type": "object",
                "properties": props.iter().map(|p| (p.to_string(), serde_json::json!({"type": "string"}))).collect::<serde_json::Map<_,_>>(),
            })
        };
        vec![
            ToolSpec { name: "read".into(), description: "Read a workspace file (paged for large files).".into(), schema: obj(&["path", "offset", "limit"]) },
            ToolSpec { name: "edit".into(), description: "Replace exactly one needle occurrence in a file.".into(), schema: obj(&["path", "old_string", "new_string"]) },
            ToolSpec { name: "write".into(), description: "Create or overwrite a workspace file.".into(), schema: obj(&["path", "content"]) },
            ToolSpec { name: "bash".into(), description: "Run a shell command in the workspace.".into(), schema: obj(&["command", "timeout_ms"]) },
            ToolSpec { name: "grep".into(), description: "Regex-search workspace files.".into(), schema: obj(&["pattern", "include"]) },
            ToolSpec { name: "lsp".into(), description: "Code intelligence: symbols/refs (grep-backed alias).".into(), schema: obj(&["pattern", "include"]) },
            ToolSpec { name: "glob".into(), description: "List workspace paths by glob.".into(), schema: obj(&["pattern"]) },
            ToolSpec { name: "kb_query".into(), description: "Query Markdown+graphify memory before grepping code. Prefer for durable knowledge.".into(), schema: serde_json::json!({"type": "object", "properties": {"query": {"type": "string"}, "budget": {"type": "integer"}}}) },
            ToolSpec { name: "memory_write".into(), description: "Save a durable fact to ./memory/<topic>.md (deduped, secrets rejected).".into(), schema: obj(&["topic", "content"]) },
            ToolSpec { name: "skill".into(), description: "Load a full SKILL.md body by name (only name+description are advertised).".into(), schema: obj(&["name"]) },
            ToolSpec { name: "question".into(), description: "Ask the user a clarifying question with options (interactive sessions only).".into(), schema: serde_json::json!({"type": "object", "properties": {"questions": {"type": "array", "items": {"type": "object", "properties": {"text": {"type": "string"}, "options": {"type": "array", "items": {"type": "string"}}}, "required": ["text"]}}}, "required": ["questions"]}) },
            ToolSpec { name: "websearch".into(), description: "Search the web for current info (needs a provider key; pick with [websearch] provider).".into(), schema: obj(&["query", "provider"]) },
            ToolSpec { name: "webfetch".into(), description: "Fetch one HTTP(S) URL as markdown/text/HTML (cap 100KB).".into(), schema: obj(&["url", "format"]) },
            ToolSpec { name: "mcp".into(), description: "Call a local MCP tool (stdio/SSE; see .lumina/mcp.json).".into(), schema: obj(&["server", "tool", "args"]) },
            ToolSpec { name: "subagent".into(), description: "Launch a scoped subagent (general|explore). Subagents cannot delegate further.".into(), schema: obj(&["task", "agent"]) },
            ToolSpec { name: "todowrite".into(), description: "Track task progress as a todo list.".into(), schema: serde_json::json!({"type": "object", "properties": {"todos": {"type": "array", "items": {"type": "object", "properties": {"content": {"type": "string"}, "status": {"type": "string"}}, "required": ["content"]}}}}) },
            ToolSpec { name: "patch".into(), description: "Apply a unified diff (---/+++ with @@ hunks) with exact context matching.".into(), schema: obj(&["patch"]) },
        ]
        .into_iter()
        .filter(|t| !(hide_websearch && t.name == "websearch"))
        .collect()
    }

    fn system_prompt(bundle: &Bundle, agent: Agent) -> String {
        let mut s = bundle.system_prompt.clone();
        s.push_str("\nYou are Lumina, a local-first coding agent. Answer with tool calls when action helps; otherwise reply directly.\n");
        s.push_str("Persistence: continue the ORIGINAL user request until fully complete (all edits applied, tests/build pass). Do NOT stop with a status summary, do NOT ask the user to say 'continue', and do NOT end with 'what I found' when fixes were requested — finish the work, then reply with the completed result.\n");
        if agent == Agent::Plan {
            s.push_str("PLAN MODE: research and blueprint only. Never edit source files; scoped writes to plan.md or .lumina/plans/*.md only.\n");
        }
        s.push_str("Tool guidance: prefer kb_query over grep for durable knowledge; batch independent reads; quote file:line references. After tool results arrive, keep going until done; call more tools if anything remains.\n");
        s.push_str(&bundle.skills_block());
        s
    }

    /// Run one user turn to completion. `on_event` receives streaming deltas
    /// (SSE broker in server mode; headless `run` ignores it).
    pub async fn run_prompt(
        &self,
        store: &Store,
        router: &Router,
        cfg: &crate::config::Config,
        session_id: &str,
        text: &str,
        effort: &str,
        images: &[ImageAttachment],
        mut on_event: impl FnMut(&str, &str) + Send,
    ) -> Result<String, AgentError> {
        let sess = store
            .get_session(session_id)?
            .ok_or_else(|| AgentError::Msg(format!("session {session_id} not found")))?;
        let workspace = PathBuf::from(&sess.workspace_dir);
        let agent = if sess.agent == "plan" { Agent::Plan } else { Agent::Build };
        let mode = match sess.permission_mode.as_str() {
            "ask" => PermissionMode::Ask,
            "auto-edit" => PermissionMode::AutoEdit,
            "auto-approve" => PermissionMode::AutoApprove,
            _ => PermissionMode::Bypass,
        };
        let bundle = Bundle::load(&workspace, cfg.compat.import_global_rules, cfg.compat.import_global_skills);
        // Custom primary agent (agents/*.md, mode: primary): extra system
        // framing, per-agent model default, and tool matrix.
        let custom = if sess.agent != "plan" && sess.agent != "build" {
            bundle.agents.iter().find(|a| a.name == sess.agent && a.mode == "primary").cloned()
        } else {
            None
        };
        // Auto-compact near the context limit (best-effort, never fails the turn).
        if Self::maybe_auto_compact(store, router, cfg, session_id).await.is_err() {
            // proceed with full history; the loop degrades gracefully
        }
        // Lumina hook: chat.message may rewrite the incoming turn text.
        let mut user_text = text.to_string();
        if let Some(h) = &self.plugins {
            let payload = serde_json::json!({"text": text, "sessionID": session_id});
            if let Some(out) = h.dispatch("chat.message", &payload, std::time::Duration::from_secs(5)) {
                if let Some(t) = out.get("patch").and_then(|p| p.get("text")).and_then(|t| t.as_str()) {
                    user_text = t.to_string();
                }
            }
        }
        let user_turn = store.add_turn(session_id, "user", &user_text)?;

        let mut messages = vec![llm::Message {
            role: "system".into(),
            content: Self::system_prompt(&bundle, agent),
            thinking: String::new(),
        }];
        if let Some(def) = &custom {
            if !def.body.trim().is_empty() {
                messages.push(llm::Message {
                    role: "system".into(),
                    content: format!("Active custom agent '{}':\n{}", def.name, def.body),
                    thinking: String::new(),
                });
            }
        }
        for t in store.turns(session_id)? {
            // Replay persisted history, skipping the user turn just stored
            // above (re-added below) to avoid sending it twice.
            if t.id == user_turn.id {
                continue;
            }
            if t.role == "user" || t.role == "assistant" {
                messages.push(llm::Message { role: t.role, content: t.text, thinking: String::new() });
            }
        }
        messages.push(llm::Message { role: "user".into(), content: user_text.clone(), thinking: String::new() });

        let reg = Registry {
            kb_query: Some(std::sync::Arc::new(|ws, q, b| {
                crate::kb::query(ws, q, b).map_err(|e| e.to_string())
            })),
            bundle: Some(bundle),
            websearch_provider: cfg.websearch.provider.clone(),
            plugins: self.plugins.clone(),
            disabled_tools: custom.as_ref().map(|d| {
                d.tools.iter().filter(|(_, &on)| !on).map(|(t, _)| t.clone()).collect()
            }).unwrap_or_default(),
            subagent: Some(Arc::new({
                let router = router.clone();
                let cfg = cfg.clone();
                move |sc: &SecCtx, task: &str, name: &str| {
                    // Sync tool context inside the async turn: park this worker
                    // thread and drive the subagent future to completion.
                    tokio::task::block_in_place(|| {
                        tokio::runtime::Handle::current()
                            .block_on(run_subtask(&router, &cfg, sc, task, name))
                    })
                    .map_err(|e| e.to_string())
                }
            })),
        };
        let (runner_headless, runner_gate) = match &self.approval {
            Some(g) => (false, g.clone()),
            None => (true, ApprovalGate::default()),
        };
        let sc = SecCtx {
            workspace: workspace.clone(),
            session: sess.id.clone(),
            agent,
            mode,
            headless: runner_headless,
            gate: runner_gate,
            bash_rules: cfg.permissions_bash.clone(),
            allow_tooling_write: false,
            depth: 0,
        };
        let agent_name = sess.agent.clone();
        // Per-agent model default (falls back to the router executor tier).
        let model = if sess.model.is_empty() {
            custom.as_ref().and_then(|d| (!d.model.is_empty()).then(|| d.model.clone())).unwrap_or_default()
        } else {
            sess.model.clone()
        };
        let hide_websearch = cfg.websearch.disabled;
        let final_text = self
            .loop_until_done(
                router,
                &reg,
                &sc,
                &agent_name,
                &model,
                effort,
                images,
                &mut messages,
                &mut on_event,
                store,
                session_id,
                &user_turn.id,
                hide_websearch,
            )
            .await?;
        // (All assistant turns are persisted inside the loop as they stream,
        // so transcripts and /resume replay show every tool round.)
        Ok(final_text)
    }

    async fn loop_until_done(
        &self,
        router: &Router,
        reg: &Registry,
        sc: &SecCtx,
        agent_name: &str,
        model: &str,
        effort: &str,
        images: &[ImageAttachment],
        messages: &mut Vec<llm::Message>,
        on_event: &mut (dyn FnMut(&str, &str) + Send),
        store: &Store,
        session_id: &str,
        user_turn_id: &str,
        hide_websearch: bool,
    ) -> Result<String, AgentError> {
        let mut last_text = String::new();
        let mut last_nonempty = String::new();
        let mut snapshotted = false;
        let mut did_work = false;
        for round in 0..self.max_steps {
            let (tx, mut rx) = mpsc::unbounded_channel::<Chunk>();
            let req = Request {
                model: model.into(),
                messages: messages.clone(),
                tools: Self::tool_specs_filtered(hide_websearch),
                max_tokens: 0,
                reasoning_effort: effort.into(),
                images: images.to_vec(),
            };
            let role = if agent_name == "plan" { "planner" } else { "executor" };
            // Spawn the stream pump (Router is Clone + 'static); drain below.
            let router2 = router.clone();
            let role2 = role.to_string();
            let agent2 = agent_name.to_string();
            // A silent gateway stream fails the turn instead of hanging the drain.
        let handle = tokio::spawn(async move {
            match tokio::time::timeout(crate::llm::STREAM_TIMEOUT, router2.chat_stream(&role2, &agent2, req, tx)).await {
                Ok(inner) => inner,
                Err(_) => Err(crate::llm::LlmError::Msg("llm: stream timed out (no response for 600s)".into())),
            }
        });
            let mut calls = Vec::new();
            while let Some(c) = rx.recv().await {
                if !c.delta.is_empty() || !c.thinking.is_empty() {
                    on_event(&c.delta, &c.thinking);
                    last_text.push_str(&c.delta);
                }
                if !c.tool_calls.is_empty() {
                    calls = c.tool_calls;
                }
                if c.done {
                    break;
                }
            }
            let _usage = handle
                .await
                .map_err(|e| AgentError::Msg(format!("stream task: {e}")))??;
            if calls.is_empty() {
                // Empty-final guard: silence after real work gets one more
                // tools-stripped round asking for done-vs-remaining, so the
                // transcript never ends in silence. Pure-empty turns (no
                // tool calls at all) pass through untouched — no extra
                // backend round trip for a turn that did nothing.
                if last_text.trim().is_empty() && did_work {
                    return Ok(self
                        .empty_final_text(router, agent_name, model, effort, messages, store, session_id)
                        .await);
                }
                let final_text = std::mem::take(&mut last_text);
                messages.push(llm::Message { role: "assistant".into(), content: final_text.clone(), thinking: String::new() });
                store.add_turn(session_id, "assistant", &final_text)?;
                return Ok(final_text);
            }
            did_work = true;
            // Execute requested tools, feed results back as user messages.
            if std::env::var("LUMINA_DEBUG_SSE").is_ok() {
                let names: Vec<&str> = calls.iter().map(|c| c.name.as_str()).collect();
                eprintln!("[agent] iter calls: {names:?}");
            }
            // Shadow-Git pre-image: snapshot before the first mutating tool
            // runs, so /undo restores the pre-turn tree (best-effort).
            if !snapshotted && calls.iter().any(|tc| is_mutating(&tc.name)) {
                match crate::checkpoints::snapshot(&sc.workspace, session_id, user_turn_id) {
                    Ok(commit) => {
                        let _ = store.add_trace(
                            session_id,
                            "checkpoint",
                            &serde_json::json!({"turn": user_turn_id, "commit": commit}).to_string(),
                        );
                    }
                    Err(e) => tracing::warn!("checkpoint skipped: {e}"),
                }
                snapshotted = true;
            }
            for tc in &calls {
                let args: HashMap<String, serde_json::Value> = tc.args.clone();
                let result = reg.call(sc, &tc.name, &args);
                let rendered = match result {
                    Ok(o) => {
                        // Cap what flows back into context (full blobs stay out of
                        // the loop; the model can re-read files if it needs more).
                        let mut out = o.output;
                        let mut note = String::new();
                        if o.truncated || out.len() > 4000 {
                            let total = out.len();
                            out.truncate(4000);
                            note = format!("\n[truncated {total} chars to 4000]");
                        }
                        format!("tool {} →\n{out}{note}", tc.name)
                    }
                    Err(e) => format!("tool {} failed: {e}", tc.name),
                };
                on_event("", &rendered);
                let tool_msg = format!("[tool result {}]\n{rendered}", tc.name);
                messages.push(llm::Message { role: "user".into(), content: tool_msg.clone(), thinking: String::new() });
                store.add_turn(session_id, "user", &tool_msg)?;
            }
            // Continuation nudge: the model must not stop with a partial
            // summary while work remains. Checkpoint every 8 rounds so long
            // tasks restate done-vs-remaining and keep going.
            if (round + 1) % 8 == 0 {
                messages.push(llm::Message { role: "user".into(), content: "Checkpoint: briefly note what is done vs remaining, then CONTINUE until the original request is fully done (all edits applied, tests/build pass). Do NOT stop with a status summary or ask the user to say 'continue'; call more tools if anything remains, else give the final completed answer.".into(), thinking: String::new() });
            } else {
                messages.push(llm::Message { role: "user".into(), content: "Continue the task until the original request is fully complete; call more tools if anything remains, else give the final completed answer. Do NOT end with a partial summary when work is still pending.".into(), thinking: String::new() });
            }
            // Skip empty assistant echoes so the model doesn't pattern-match
            // its own filler ("I'll query...") into a retry loop.
            // (Continuation nudges above are deterministic scaffolding:
            // re-added by the loop, never persisted.)
            if !last_text.trim().is_empty() {
                messages.push(llm::Message { role: "assistant".into(), content: last_text.clone(), thinking: String::new() });
                store.add_turn(session_id, "assistant", &last_text)?;
                last_nonempty = last_text.clone();
            }
            last_text.clear();
        }
        // Exhausted ceiling with tools still pending: return the last real
        // model text plus an explicit resume pointer instead of an empty
        // turn (empty turns read as silent stops and force `continue`).
        let tail = if last_nonempty.trim().is_empty() {
            "Tool budget exhausted after 120 tool rounds with work still pending. Say `continue` to resume: the next turn should list exactly what remains and the precise next tool calls (names + arguments).".to_string()
        } else {
            last_nonempty
        };
        store.add_turn(session_id, "assistant", &tail)?;
        Ok(tail)
    }

    /// One tools-stripped round asking the model to state done-vs-remaining
    /// after a silent turn end. Never fails the turn: a still-silent model
    /// yields the canned resume pointer.
    async fn empty_final_text(
        &self,
        router: &Router,
        agent_name: &str,
        model: &str,
        effort: &str,
        messages: &[llm::Message],
        store: &Store,
        session_id: &str,
    ) -> String {
        let mut msgs = messages.to_vec();
        msgs.push(llm::Message {
            role: "assistant".into(),
            content: String::new(),
            thinking: String::new(),
        });
        msgs.push(llm::Message {
            role: "user".into(),
            content: "The turn produced no final text. State briefly: what was accomplished this turn, what remains undone, and the exact next step. Text only, do not call tools.".into(),
            thinking: String::new(),
        });
        let (tx, mut rx) = mpsc::unbounded_channel::<Chunk>();
        let req = Request {
            model: model.into(),
            messages: msgs,
            tools: Vec::new(),
            max_tokens: 0,
            reasoning_effort: effort.into(),
            images: Vec::new(),
        };
        let role = if agent_name == "plan" { "planner" } else { "executor" };
        let router2 = router.clone();
        let role2 = role.to_string();
        let agent2 = agent_name.to_string();
        let handle = tokio::spawn(async move {
            match tokio::time::timeout(crate::llm::STREAM_TIMEOUT, router2.chat_stream(&role2, &agent2, req, tx)).await {
                Ok(inner) => inner,
                Err(_) => Err(crate::llm::LlmError::Msg("llm: stream timed out (no response for 600s)".into())),
            }
        });
        let mut text = String::new();
        while let Some(c) = rx.recv().await {
            text.push_str(&c.delta);
            text.push_str(&c.thinking);
            if c.done {
                break;
            }
        }
        let _ = handle.await;
        let _ = store.add_turn(session_id, "user", "The turn produced no final text. State briefly: what was accomplished this turn, what remains undone, and the exact next step. Text only, do not call tools.");
        if text.trim().is_empty() {
            let canned = "(turn completed with no text reply; transcript holds the tool results \u{2014} say `continue` to resume)".to_string();
            let _ = store.add_turn(session_id, "assistant", &canned);
            return canned;
        }
        let _ = store.add_turn(session_id, "assistant", &text);
        text
    }
}

/// Tools whose execution mutates the workspace (checkpoint trigger).
fn is_mutating(name: &str) -> bool {
    matches!(name, "edit" | "write" | "bash" | "memory_write" | "patch")
}




/// Scoped subagent run: fresh context with a builtin (general|explore) system
/// prompt, restricted tool specs, max 6 steps, no further delegation.
async fn run_subtask(
    router: &Router,
    cfg: &crate::config::Config,
    parent: &SecCtx,
    task: &str,
    name: &str,
) -> Result<String, AgentError> {
    let def = crate::rules::builtin_agents()
        .into_iter()
        .find(|a| a.name == name)
        .ok_or_else(|| AgentError::Msg(format!("unknown subagent {name:?} (want general|explore)")))?;
    let bundle = Bundle::load(&parent.workspace, cfg.compat.import_global_rules, cfg.compat.import_global_skills);
    let mut prompt = bundle.system_prompt.clone();
    prompt.push_str(&format!("\n{}\n", def.body));
    let specs: Vec<ToolSpec> = Runner::tool_specs()
        .into_iter()
        .filter(|t| {
            if name == "explore" {
                matches!(
                    t.name.as_str(),
                    "read" | "grep" | "glob" | "kb_query" | "skill" | "webfetch" | "websearch"
                )
            } else {
                t.name != "subagent"
            }
        })
        .collect();
    let reg = Registry {
        kb_query: Some(std::sync::Arc::new(|ws, q, b| {
            crate::kb::query(ws, q, b).map_err(|e| e.to_string())
        })),
        bundle: Some(bundle),
        websearch_provider: cfg.websearch.provider.clone(),
        plugins: None,
        subagent: None, // depth guard is also enforced in do_subagent
        disabled_tools: std::collections::HashSet::new(),
    };
    let sc = SecCtx {
        workspace: parent.workspace.clone(),
        session: parent.session.clone(),
        agent: parent.agent,
        mode: parent.mode,
        headless: parent.headless,
        gate: parent.gate.clone(),
        bash_rules: parent.bash_rules.clone(),
        allow_tooling_write: false,
        depth: parent.depth + 1,
    };
    let mut messages = vec![
        llm::Message { role: "system".into(), content: prompt, thinking: String::new() },
        llm::Message { role: "user".into(), content: task.into(), thinking: String::new() },
    ];
    let model = if def.model.is_empty() { String::new() } else { def.model.clone() };
    let mut last = String::new();
    for _ in 0..32 {
        let (txs, mut rxs) = mpsc::unbounded_channel::<Chunk>();
        let req = Request {
            model: model.clone(),
            messages: messages.clone(),
            tools: specs.clone(),
            max_tokens: 0,
            reasoning_effort: String::new(),
            images: vec![],
        };
        let router2 = router.clone();
        let handle = tokio::spawn(async move {
            match tokio::time::timeout(crate::llm::STREAM_TIMEOUT, router2.chat_stream("executor", "build", req, txs)).await {
                Ok(inner) => inner,
                Err(_) => Err(crate::llm::LlmError::Msg("llm: stream timed out (no response for 600s)".into())),
            }
        });
        let mut calls = Vec::new();
        while let Some(c) = rxs.recv().await {
            if !c.delta.is_empty() {
                last.push_str(&c.delta);
            }
            if !c.tool_calls.is_empty() {
                calls = c.tool_calls;
            }
            if c.done {
                break;
            }
        }
        let _usage = handle.await.map_err(|e| AgentError::Msg(format!("subagent task: {e}")))??;
        if calls.is_empty() {
            return Ok(last);
        }
        for tc in &calls {
            let result = reg.call(&sc, &tc.name, &tc.args);
            let rendered = match result {
                Ok(o) => format!("tool {} →\n{}", tc.name, o.output),
                Err(e) => format!("tool {} failed: {e}", tc.name),
            };
            messages.push(llm::Message { role: "user".into(), content: format!("[tool result {}]\n{rendered}", tc.name), thinking: String::new() });
        }
        messages.push(llm::Message { role: "assistant".into(), content: last.clone(), thinking: String::new() });
        last.clear();
    }
    Ok(last)
}

impl Runner {
    /// Rough token estimate (chars/4 over messages + tool specs).
    pub fn estimate_tokens(messages: &[llm::Message], specs: &[ToolSpec]) -> usize {
        let chars: usize = messages.iter().map(|m| m.content.len() + m.role.len()).sum::<usize>()
            + specs.iter().map(|s| s.name.len() + s.description.len() + s.schema.to_string().len()).sum::<usize>();
        chars / 4
    }

    /// Compact when the estimate crosses 80% of the advertised window.
    pub async fn maybe_auto_compact(
        store: &Store,
        router: &Router,
        cfg: &crate::config::Config,
        session_id: &str,
    ) -> Result<(), AgentError> {
        let sess = store
            .get_session(session_id)?
            .ok_or_else(|| AgentError::Msg(format!("session {session_id} not found")))?;
        let turns = store.turns(session_id)?;
        if turns.len() < 20 {
            return Ok(());
        }
        let model = if sess.model.is_empty() { router.model_for("executor", "build") } else { sess.model.clone() };
        let window = router.context_window(&model).await as usize;
        if window == 0 {
            return Ok(()); // unknown window: never auto-compact blind
        }
        let msgs: Vec<llm::Message> = turns
            .iter()
            .map(|t| llm::Message { role: t.role.clone(), content: t.text.clone(), thinking: String::new() })
            .collect();
        let used = Self::estimate_tokens(&msgs, &Self::tool_specs());
        if used * 100 < window * 80 {
            return Ok(());
        }
        Self::compact_session(store, router, cfg, session_id).await?;
        Ok(())
    }

    /// Keep the first 2 + last 6 turns, summarize the middle via the fast
    /// tier (naive truncation note when no LLM is reachable), and persist a
    /// summary turn. Mirrors Go auto-compaction (rules/diffs/memory pointers
    /// are rebuilt from disk, never summarized away).
    pub async fn compact_session(
        store: &Store,
        router: &Router,
        cfg: &crate::config::Config,
        session_id: &str,
    ) -> Result<String, AgentError> {
        let turns = store.turns(session_id)?;
        if turns.len() < 20 {
            return Ok("already compact".into());
        }
        let keep_head = 2.min(turns.len());
        let keep_tail = 6.min(turns.len() - keep_head);
        let middle = &turns[keep_head..turns.len() - keep_tail];
        if middle.is_empty() {
            return Ok("already compact".into());
        }
        let digest: String = middle
            .iter()
            .map(|t| format!("{}: {}", t.role, t.text.chars().take(400).collect::<String>()))
            .collect::<Vec<_>>()
            .join("\n")
            .chars()
            .take(12_000)
            .collect();
        // Summarize with the fast tier; degrade to a pointer note offline.
        let summary = {
            let (tx, mut rx) = mpsc::unbounded_channel::<Chunk>();
            let req = Request {
                model: String::new(),
                messages: vec![
                    llm::Message { role: "system".into(), content: "Summarize this coding session's middle turns in 10 bullet lines: decisions, file changes, open threads. No preamble.".into(), thinking: String::new() },
                    llm::Message { role: "user".into(), content: digest.clone(), thinking: String::new() },
                ],
                tools: vec![],
                max_tokens: 0,
                reasoning_effort: String::new(),
                images: vec![],
            };
            let router2 = router.clone();
            let handle = tokio::spawn(async move {
            match tokio::time::timeout(crate::llm::STREAM_TIMEOUT, router2.chat_stream("fast", "build", req, tx)).await {
                Ok(inner) => inner,
                Err(_) => Err(crate::llm::LlmError::Msg("llm: stream timed out (no response for 600s)".into())),
            }
        });
            let mut text = String::new();
            while let Some(c) = rx.recv().await {
                text.push_str(&c.delta);
                if c.done {
                    break;
                }
                if text.len() > 4000 {
                    break;
                }
            }
            match handle.await {
                Ok(Ok(_)) if !text.trim().is_empty() => text,
                _ => format!("(omitted {} middle turns; LLM unavailable)\n{}", middle.len(), digest.chars().take(2000).collect::<String>()),
            }
        };
        let ids: Vec<String> = middle.iter().map(|t| t.id.clone()).collect();
        let n = store.delete_turns(session_id, &ids)?;
        store.add_turn(session_id, "assistant", &format!("Prior context summary ({n} turns compacted):\n{summary}"))?;
        let _ = cfg; // compat flags are read at prompt time, not compaction time
        Ok(format!("compacted {n} turns"))
    }
}

#[cfg(test)]
mod specs_tests {
    use super::*;

    #[test]
    fn tool_specs_include_lsp_alias() {
        let specs = Runner::tool_specs();
        let names: Vec<&str> = specs.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"lsp"), "{names:?}");
        assert!(names.contains(&"grep"), "{names:?}");
    }

    #[test]
    fn tool_specs_filtered_hides_websearch() {
        let specs = Runner::tool_specs_filtered(true);
        let names: Vec<&str> = specs.iter().map(|s| s.name.as_str()).collect();
        assert!(!names.contains(&"websearch"), "{names:?}");
        assert!(names.contains(&"webfetch"), "{names:?}");
        let all_specs = Runner::tool_specs_filtered(false);
        let all: Vec<&str> = all_specs.iter().map(|s| s.name.as_str()).collect();
        assert!(all.contains(&"websearch"), "{all:?}");
    }
}

#[cfg(test)]
mod guard_tests {
    use super::*;
    use std::io::{Read, Write};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn body_for(n: usize) -> &'static str {
        match n {
            // Round 1: one read tool call.
            1 => "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"1\",\"function\":{\"name\":\"read\",\"arguments\":\"{\\\"path\\\":\\\"a.txt\\\"}\"}}]}}]}\n\n",
            // Round 2: silent end (empty delta, no calls).
            2 => "data: {\"choices\":[{\"delta\":{}}]}\n\n",
            // Round 3 (empty-final guard, tools stripped): stuck summary.
            _ => "data: {\"choices\":[{\"delta\":{\"content\":\"stuck: audit done, nothing left\"}}]}\n\n",
        }
    }

    /// Raw-TCP scripted OpenAI-compatible gateway: 3 sequential POSTs get
    /// tool-call, silence, then the guard's stuck summary.
    #[tokio::test]
    async fn empty_final_turn_gets_stuck_summary() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = std::sync::Arc::new(AtomicUsize::new(0));
        let hits2 = hits.clone();
        std::thread::spawn(move || {
            for _ in 0..3 {
                let Ok((mut s, _)) = listener.accept() else { return };
                // Consume headers + body (client POSTs JSON).
                let mut head = Vec::new();
                while !head.ends_with(b"\r\n\r\n") && head.len() < 65536 {
                    let mut b = [0u8; 1];
                    if s.read_exact(&mut b).is_err() {
                        return;
                    }
                    head.push(b[0]);
                }
                let head_str = String::from_utf8_lossy(&head);
                let len = head_str
                    .lines()
                    .find_map(|l| {
                        l.strip_prefix("Content-Length:")
                            .or_else(|| l.strip_prefix("content-length:"))
                    })
                    .and_then(|v| v.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                let mut body = vec![0u8; len];
                if len > 0 && s.read_exact(&mut body).is_err() {
                    return;
                }
                let n = hits2.fetch_add(1, Ordering::SeqCst) + 1;
                let payload = body_for(n.min(3));
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    payload.len(),
                    payload
                );
                let _ = s.write_all(resp.as_bytes());
            }
        });

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "alpha").unwrap();
        let ws = dir.path().display().to_string();
        let db = dir.path().join("t.db").display().to_string();
        let store = Store::open(Some(Box::leak(db.into_boxed_str()))).unwrap();
        let mut providers = HashMap::new();
        providers.insert(
            "testprov".into(),
            crate::config::ProviderConfig {
                protocol: "openai-compatible".into(),
                base_url: format!("http://{addr}"),
                api_key_env: String::new(),
                api_key_cmd: String::new(),
                token_url: String::new(),
            },
        );
        let cfg = crate::config::Config {
            router: crate::config::RouterConfig {
                fast: String::new(),
                executor: "testprov/echo".into(),
                planner: String::new(),
                improver: String::new(),
            },
            providers,
            ..Default::default()
        };
        let router = Router::new(&cfg).unwrap();
        let sess = store.create_session(&ws, "build", "bypass", "").unwrap();
        let runner = Runner::new();
        let out = runner
            .run_prompt(&store, &router, &cfg, &sess.id, "read a.txt", "", &[], |_, _| {})
            .await
            .unwrap();
        assert_eq!(out, "stuck: audit done, nothing left", "silent turn must end with a stuck summary, got {out:?}");
        assert_eq!(hits.load(Ordering::SeqCst), 3, "expected tool + silent + guard rounds");
        let turns = store.turns(&sess.id).unwrap();
        let last = turns.last().unwrap();
        assert_eq!(last.role, "assistant");
        assert!(!last.text.trim().is_empty(), "transcript must not end in silence");
    }
    /// Pure-empty turns (no tool calls, silent model) pass through with no
    /// extra backend round trip — the guard only fires after real work.
    #[tokio::test]
    async fn empty_final_without_work_passes_through() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = std::sync::Arc::new(AtomicUsize::new(0));
        let hits2 = hits.clone();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            for _ in 0..1 {
                let Ok((mut s, _)) = listener.accept() else { return };
                let mut head = Vec::new();
                while !head.ends_with(b"\r\n\r\n") && head.len() < 65536 {
                    let mut b = [0u8; 1];
                    if s.read_exact(&mut b).is_err() {
                        return;
                    }
                    head.push(b[0]);
                }
                let head_str = String::from_utf8_lossy(&head);
                let len = head_str
                    .lines()
                    .find_map(|l| {
                        l.strip_prefix("Content-Length:")
                            .or_else(|| l.strip_prefix("content-length:"))
                    })
                    .and_then(|v| v.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                let mut body = vec![0u8; len];
                if len > 0 && s.read_exact(&mut body).is_err() {
                    return;
                }
                hits2.fetch_add(1, Ordering::SeqCst);
                let payload = "data: {\"choices\":[{\"delta\":{}}]}\n\n";
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    payload.len(),
                    payload
                );
                let _ = s.write_all(resp.as_bytes());
            }
        });

        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().display().to_string();
        let db = dir.path().join("t.db").display().to_string();
        let store = Store::open(Some(Box::leak(db.into_boxed_str()))).unwrap();
        let mut providers = HashMap::new();
        providers.insert(
            "testprov".into(),
            crate::config::ProviderConfig {
                protocol: "openai-compatible".into(),
                base_url: format!("http://{addr}"),
                api_key_env: String::new(),
                api_key_cmd: String::new(),
                token_url: String::new(),
            },
        );
        let cfg = crate::config::Config {
            router: crate::config::RouterConfig {
                fast: String::new(),
                executor: "testprov/echo".into(),
                planner: String::new(),
                improver: String::new(),
            },
            providers,
            ..Default::default()
        };
        let router = Router::new(&cfg).unwrap();
        let sess = store.create_session(&ws, "build", "bypass", "").unwrap();
        let runner = Runner::new();
        let out = runner
            .run_prompt(&store, &router, &cfg, &sess.id, "say nothing", "", &[], |_, _| {})
            .await
            .unwrap();
        assert!(out.trim().is_empty(), "no-work silence must pass through, got {out:?}");
        assert_eq!(hits.load(Ordering::SeqCst), 1, "no guard round trip for idle silence");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_system_prompt_fences() {
        let bundle = Bundle::default();
        let s = Runner::system_prompt(&bundle, Agent::Plan);
        assert!(s.contains("PLAN MODE"), "{s}");
        let s = Runner::system_prompt(&bundle, Agent::Build);
        assert!(!s.contains("PLAN MODE"));
    }
}

#[cfg(test)]
mod compact_tests {
    use super::*;

    fn setup() -> (Store, Router, crate::config::Config, String) {
        let d = tempfile::tempdir().unwrap();
        let db = d.path().join("t.db").display().to_string();
        // Keep the tempdir alive via the path alone is unsafe; leak it for test scope.
        let dir = d.keep();
        let _ = dir;
        let store = Store::open(Some(Box::leak(db.into_boxed_str()))).unwrap();
        let cfg = crate::config::Config::default();
        let router = Router::new(&cfg).unwrap();
        let sess = store.create_session("/w", "build", "bypass", "").unwrap();
        (store, router, cfg, sess.id)
    }

    #[tokio::test]
    async fn compact_small_session_is_noop() {
        let (store, router, cfg, id) = setup();
        for i in 0..5 {
            store.add_turn(&id, "user", &format!("q{i}")).unwrap();
        }
        let msg = Runner::compact_session(&store, &router, &cfg, &id).await.unwrap();
        assert_eq!(msg, "already compact");
        assert_eq!(store.turns(&id).unwrap().len(), 5);
    }

    #[tokio::test]
    async fn compact_large_session_summarizes_without_llm() {
        let (store, router, cfg, id) = setup();
        for i in 0..25 {
            let role = if i % 2 == 0 { "user" } else { "assistant" };
            store.add_turn(&id, role, &format!("turn text number {i}")).unwrap();
        }
        // No LLM reachable (local_ollama down or foreign): naive note path.
        let msg = Runner::compact_session(&store, &router, &cfg, &id).await.unwrap();
        assert!(msg.starts_with("compacted"), "{msg}");
        let turns = store.turns(&id).unwrap();
        assert_eq!(turns.len(), 2 + 6 + 1, "kept head+tail+summary, got {}", turns.len());
        assert!(turns.last().unwrap().text.contains("Prior context summary"));
    }

    #[tokio::test]
    async fn auto_compact_skips_unknown_window() {
        let (store, router, cfg, id) = setup();
        for i in 0..25 {
            store.add_turn(&id, "user", &format!("q{i}")).unwrap();
        }
        // Executor model unset + no Ollama → window 0 → never auto-compact blind.
        Runner::maybe_auto_compact(&store, &router, &cfg, &id).await.unwrap();
        assert_eq!(store.turns(&id).unwrap().len(), 25);
    }
}
