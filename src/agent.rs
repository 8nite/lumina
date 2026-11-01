//! Agentic turn loop: prompt → stream → tool calls → repeat.
//! Sessions, subagents, compaction, checkpoints.
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

/// Tool lifecycle for `turn.tool` SSE rows (Go OnToolStart/OnToolDone parity).
#[derive(Debug, Clone)]
pub enum ToolEvent {
    Start { name: String, target: String },
    Done { name: String, target: String, preview: String, error: bool, duration_ms: u64 },
}

/// Turn result: final text plus token usage for turn.completed stats.
#[derive(Debug, Clone, Default)]
pub struct TurnOutcome {
    pub text: String,
    pub usage: crate::llm::Usage,
}

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
            ToolSpec { name: "bash".into(), description: "Run a shell command in the workspace (workdir optional; background:true runs detached; poll with task_output).".into(), schema: obj(&["command", "timeout_ms"]) },
            ToolSpec { name: "grep".into(), description: "Regex-search workspace files.".into(), schema: obj(&["pattern", "include"]) },
            ToolSpec { name: "lsp".into(), description: "Code intelligence: symbols/refs (grep-backed alias).".into(), schema: obj(&["pattern", "include"]) },
            ToolSpec { name: "glob".into(), description: "List workspace paths by glob.".into(), schema: obj(&["pattern"]) },
            ToolSpec { name: "kb_query".into(), description: "Query Markdown+graphify memory before grepping code. Prefer for durable knowledge.".into(), schema: serde_json::json!({"type": "object", "properties": {"query": {"type": "string"}, "budget": {"type": "integer"}}}) },
            ToolSpec { name: "memory_write".into(), description: "Save a durable fact to ./memory/<topic>.md (deduped, secrets rejected).".into(), schema: obj(&["topic", "content"]) },
            ToolSpec { name: "skill".into(), description: "Load a full SKILL.md body by name (only name+description are advertised).".into(), schema: obj(&["name"]) },
            ToolSpec { name: "question".into(), description: "Ask the user clarifying questions (interactive sessions only). Options are {label, description}; multiple:true allows several picks; headless runs get best-guess guidance.".into(), schema: serde_json::json!({"type": "object", "properties": {"questions": {"type": "array", "items": {"type": "object", "properties": {"question": {"type": "string"}, "header": {"type": "string"}, "multiple": {"type": "boolean"}, "options": {"type": "array", "items": {"type": "object", "properties": {"label": {"type": "string"}, "description": {"type": "string"}}, "required": ["label"]}}}, "required": ["question"]}}}, "required": ["questions"]}) },
            ToolSpec { name: "websearch".into(), description: "Search the web for current info (needs a provider key; pick with [websearch] provider).".into(), schema: obj(&["query", "provider"]) },
            ToolSpec { name: "webfetch".into(), description: "Fetch one HTTP(S) URL as markdown/text/HTML (cap 100KB).".into(), schema: obj(&["url", "format"]) },
            ToolSpec { name: "mcp".into(), description: "Call a local MCP tool (stdio/SSE; see .lumina/mcp.json).".into(), schema: obj(&["server", "tool", "args"]) },
            ToolSpec { name: "subagent".into(), description: "Launch a scoped subagent (general|explore). Subagents cannot delegate further. background:true returns a task ID; poll with task_output.".into(), schema: obj(&["task", "agent"]) },
            ToolSpec { name: "task_output".into(), description: "Poll a background bash/subagent task by ID.".into(), schema: obj(&["task_id"]) },
            ToolSpec { name: "todowrite".into(), description: "Track task progress as a todo list.".into(), schema: serde_json::json!({"type": "object", "properties": {"todos": {"type": "array", "items": {"type": "object", "properties": {"content": {"type": "string"}, "status": {"type": "string"}}, "required": ["content"]}}}}) },
            ToolSpec { name: "patch".into(), description: "Apply a *** Begin Patch *** multi-file diff (add/update/delete/move); unified diff also accepted.".into(), schema: obj(&["patchText", "patch_text", "patch", "diff"]) },
        ]
        .into_iter()
        .filter(|t| !(hide_websearch && t.name == "websearch"))
        .collect()
    }

    fn system_prompt(bundle: &Bundle, agent: Agent) -> String {
        let mut s = bundle.system_prompt.clone();
        s.push_str("\nYou are Lumina, a local-first coding agent. Answer with tool calls when action helps; otherwise reply directly.\n");
        s.push_str("Persistence: continue the ORIGINAL user request until fully complete (all edits applied, tests/build pass). Do NOT stop with a status summary, do NOT ask the user to say 'continue', and do NOT end with 'what I found' when fixes were requested — finish the work, then reply with the completed result.\n");
        s.push_str("Verify every code change in the same turn: a turn that ran edit/write/patch must also run a passing test/build/typecheck command (cargo test, pnpm build, tsc, …) before replying done — an edit without a green verification run is not done.\n");
        if agent == Agent::Plan {
            s.push_str("PLAN MODE: research and blueprint only. Never edit source files; scoped writes to plan.md or .lumina/plans/*.md only.\n");
        }
        s.push_str("Tool guidance: prefer kb_query over grep for durable knowledge; batch independent reads; quote file:line references. After tool results arrive, keep going until done; call more tools if anything remains.\n");
        s.push_str(crate::vault::VAULT_GUIDANCE);
        s.push('\n');
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
        mut on_stage: impl FnMut(&str) + Send,
        mut on_tool: impl FnMut(ToolEvent) + Send,
    ) -> Result<TurnOutcome, AgentError> {
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
            // Vault boundary: the provider sees refs, never values.
            if t.id == user_turn.id {
                continue;
            }
            if t.role == "user" || t.role == "assistant" {
                let sub = store.vault.substitute(session_id, &t.text).map_err(AgentError::Msg)?;
                messages.push(llm::Message { role: t.role, content: sub, thinking: String::new() });
            }
        }
        // Vault boundary: store raw, send vaulted.
        let vaulted_user = store.vault.substitute(session_id, &user_text).map_err(AgentError::Msg)?;
        messages.push(llm::Message { role: "user".into(), content: vaulted_user, thinking: String::new() });

        let reg = Registry {
            kb_query: Some(std::sync::Arc::new(|ws, q, b| {
                crate::kb::query(ws, q, b).map_err(|e| e.to_string())
            })),
            bundle: Some(bundle),
            websearch_provider: cfg.websearch.provider.clone(),
            websearch_disabled: cfg.websearch.disabled,
            plugins: self.plugins.clone(),
            disabled_tools: custom.as_ref().map(|d| {
                d.tools.iter().filter(|(_, &on)| !on).map(|(t, _)| t.clone()).collect()
            }).unwrap_or_default(),
            subagent: Some(Arc::new({
                let router = router.clone();
                let cfg = cfg.clone();
                let vault = store.vault.clone();
                move |sc: &SecCtx, req: &crate::tools::SubagentReq| {
                    // Sync tool context inside the async turn: park this worker
                    // thread and drive the subagent future to completion.
                    tokio::task::block_in_place(|| {
                        tokio::runtime::Handle::current()
                            .block_on(run_subtask(&router, &cfg, &vault, sc, req))
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
        let (final_text, usage) = self
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
                &mut on_stage,
                &mut on_tool,
                store,
                session_id,
                &user_turn.id,
                hide_websearch,
            )
            .await?;
        // (All assistant turns are persisted inside the loop as they stream,
        // so transcripts and /resume replay show every tool round.)
        Ok(TurnOutcome { text: final_text, usage })
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
        on_stage: &mut (dyn FnMut(&str) + Send),
        on_tool: &mut (dyn FnMut(ToolEvent) + Send),
        store: &Store,
        session_id: &str,
        user_turn_id: &str,
        hide_websearch: bool,
    ) -> Result<(String, crate::llm::Usage), AgentError> {
        let mut last_text = String::new();
        // Thinking never reaches the transcript through the pump below
        // (only deltas join last_text), so accumulate it here: each tool
        // round persists its segment as a `thinking` turn, and the
        // remainder joins the final answer (Go parity).
        let mut thinking_buf = String::new();
        let mut usage = crate::llm::Usage::default();
        let mut last_nonempty = String::new();
        let mut snapshotted = false;
        let mut did_work = false;
        // Per-call repeat ledger + identical-round streak (Go parity).
        // `task_output` polls are progress, not loops — exempt.
        let mut seen: HashMap<String, usize> = HashMap::new();
        // Verify-after-fix gate (stuck session 56d41a74: 23 code
        // mutations, 4 verify runs — later edits never verified and
        // drifted). `mutated` flips on edit/write/patch, clears only via
        // a passing verify command; the empty-finish path below refuses
        // to end the turn while mutations sit unverified (max 2 forced
        // rounds, then the tail carries an explicit resume pointer).
        let mut mutated = false;
        let mut verified = false;
        let mut verify_rounds = 0usize;
        // Same-file re-read ledger: paging one file with different
        // offsets evades the identical-call key above (the stuck session
        // re-read SessionView.tsx 10x). Nudge once per file per turn.
        let mut read_counts: HashMap<String, usize> = HashMap::new();
        let mut prev_round_key = String::new();
        let mut repeat_streak = 0usize;
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
                    thinking_buf.push_str(&c.thinking);
                }
                if !c.tool_calls.is_empty() {
                    calls = c.tool_calls;
                }
                if c.done {
                    break;
                }
            }
            let u = handle
                .await
                .map_err(|e| AgentError::Msg(format!("stream task: {e}")))??;
            usage.input += u.input;
            usage.output += u.output;
            if calls.is_empty() {
                // Verify-after-fix gate: the model wants to end the turn
                // with code mutations it never verified. Force up to two
                // verification rounds instead of accepting a text summary.
                if mutated && !verified && verify_rounds < 2 {
                    verify_rounds += 1;
                    let demand = "Unverified changes: this turn ran edit/write/patch but no passing test/build/typecheck command yet. Run the relevant verification now (e.g. `cargo test`, `pnpm --dir web build`) and fix failures until green; do NOT end with a text summary until verification passes. If verification is genuinely impossible in this workspace, state exactly what blocks it.";
                    messages.push(llm::Message { role: "user".into(), content: demand.to_string(), thinking: String::new() });
                    let _ = store.add_turn(session_id, "user", demand);
                    on_stage("verifying…");
                    continue;
                }
                // Empty-final guard: silence after real work gets one more
                // tools-stripped round asking for done-vs-remaining, so the
                // transcript never ends in silence. Pure-empty turns (no
                // tool calls at all) pass through untouched — no extra
                // backend round trip for a turn that did nothing.
                if last_text.trim().is_empty() && did_work {
                    on_stage("summarizing\u{2026}");
                    let capped_thinking: String = thinking_buf.chars().take(4000).collect();
                    if !capped_thinking.trim().is_empty() {
                        let resolved = store.vault.resolve(session_id, &capped_thinking).map_err(AgentError::Msg)?;
                        store.add_turn(session_id, "thinking", &resolved)?;
                        thinking_buf.clear();
                    }
                    return Ok((
                        self.empty_final_text(router, agent_name, model, effort, messages, store, session_id)
                            .await,
                        usage,
                    ));
                }
                let final_text = std::mem::take(&mut last_text);
                // Persist trailing reasoning as its own closing dialog.
                let capped_thinking: String = thinking_buf.chars().take(4000).collect();
                if !capped_thinking.trim().is_empty() {
                    let resolved = store.vault.resolve(session_id, &capped_thinking).map_err(AgentError::Msg)?;
                    store.add_turn(session_id, "thinking", &resolved)?;
                }
                // User transparency: stored transcript and return carry
                // values; refs only ever faced the provider.
                let mut final_text = store.vault.resolve(session_id, &final_text).map_err(AgentError::Msg)?;
                if mutated && !verified {
                    final_text.push_str("\n\n");
                    final_text.push_str(UNVERIFIED_POINTER);
                }
                store.add_turn(session_id, "assistant", &final_text)?;
                return Ok((final_text, usage));
            }
            did_work = true;
            // Identical-round streak: same tool round 3+ times in a row
            // with no new result gets a change-approach nudge.
            let round_key = {
                let mut keys: Vec<String> = calls.iter().map(|c| tool_call_key(&c.name, &c.args)).collect();
                keys.sort();
                keys.join("\x00")
            };
            if !round_key.is_empty() && round_key == prev_round_key {
                repeat_streak += 1;
            } else {
                repeat_streak = 1;
                prev_round_key = round_key;
            }
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
                on_stage(&format!("tool: {}", tool_label(&tc.name, &tc.args)));
                // Resolve direction: the model only ever saw refs, so its
                // args carry them. Resolve before execution (tools get
                // values). Fail closed on vault errors.
                let args_val = serde_json::Value::Object(
                    tc.args.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
                );
                let resolved = store.vault.resolve_value(session_id, &args_val);
                let args: HashMap<String, serde_json::Value> = resolved
                    .as_object()
                    .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
                    .unwrap_or_default();
                let target = crate::tools::tool_target(&tc.name, &args);
                on_tool(ToolEvent::Start { name: tc.name.clone(), target: target.clone() });
                // Hard cap: the 4th-read nudge below is advisory and the
                // stuck session (56d41a74) re-read SessionView.tsx 10x
                // anyway (224 reads, 0 nudge hits — the code postdates it).
                // Block the 6th+ read of one path per turn so the loop must
                // act instead of re-paging; edits reset the count below.
                let blocked = read_repeat_blocked(&tc.name, &target, read_counts.get(&target).copied().unwrap_or(0));
                let was_blocked = blocked.is_some();
                let t0 = std::time::Instant::now();
                let result = match blocked {
                    Some(msg) => Err(crate::tools::ToolError::Msg(msg)),
                    None => reg.call(sc, &tc.name, &args),
                };
                let elapsed_ms = t0.elapsed().as_millis() as u64;
                let (preview, is_err) = match &result {
                    Ok(o) => (tool_preview(&o.output), false),
                    Err(e) => (tool_preview(&e.to_string()), true),
                };
                on_tool(ToolEvent::Done {
                    name: tc.name.clone(),
                    target,
                    preview,
                    error: is_err,
                    duration_ms: elapsed_ms,
                });
                let result_ok = result.is_ok();
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
                // Third identical execution: name the call and tell the
                // model to stop. Fires once per call per turn so the nudge
                // doesn't itself become transcript spam.
                let mut rendered = rendered;
                let key = tool_call_key(&tc.name, &args);
                if !key.is_empty() {
                    let n = seen.entry(key).or_insert(0);
                    *n += 1;
                    if *n == 3 {
                        rendered.push_str(&format!(
                            "\n\nLoop guard: identical `{}` call already ran 3 times this turn with no new result. Do NOT call it again with the same arguments — change approach (different args or tool) or give the final text answer now.",
                            tool_label(&tc.name, &args)
                        ));
                    }
                }
                // Same-file re-read: different offsets still mean no
                // progress. Fires once per file (4th read) per turn.
                if tc.name.eq_ignore_ascii_case("read") {
                    let target = crate::tools::tool_target(&tc.name, &args);
                    if !target.is_empty() {
                        let n = read_counts.entry(target.clone()).or_insert(0);
                        *n += 1;
                        if !was_blocked {
                            if let Some(nudge) = read_repeat_nudge(&target, *n) {
                                rendered.push_str(&nudge);
                            }
                        }
                    }
                }
                // Verify-after-fix tracking: a code mutation invalidates
                // prior verification; only a passing verify command
                // (bash runs a test/build/lint command and its output has
                // no `[exit N]` failure marker) counts as verified.
                // Re-read after a change is legitimate too: an
                // edit/write/patch resets that path's re-read count.
                if result_ok
                    && (tc.name.eq_ignore_ascii_case("edit")
                        || tc.name.eq_ignore_ascii_case("write")
                        || tc.name.eq_ignore_ascii_case("patch"))
                {
                    mutated = true;
                    verified = false;
                    let target = crate::tools::tool_target(&tc.name, &args);
                    if !target.is_empty() {
                        read_counts.remove(&target);
                    }
                }
                if tc.name.eq_ignore_ascii_case("bash") && result_ok && !rendered.contains("[exit ") {
                    let cmd = crate::tools::tool_target(&tc.name, &args);
                    if is_verify_command(&cmd) {
                        verified = true;
                    }
                }
                on_event("", &rendered);
                let tool_msg = format!("[tool result {}]\n{rendered}", tc.name);
                // Vault boundary: tool outputs may carry values — store raw,
                // send vaulted.
                let tool_msg_sub = store.vault.substitute(session_id, &tool_msg).map_err(AgentError::Msg)?;
                messages.push(llm::Message { role: "user".into(), content: tool_msg_sub, thinking: String::new() });
                store.add_turn(session_id, "user", &tool_msg)?;
            }
            // Continuation nudge: the model must not stop with a partial
            // summary while work remains. Checkpoint every 8 rounds so long
            // tasks restate done-vs-remaining and keep going.
            let mut nudge = if (round + 1) % 8 == 0 {
                "Checkpoint: briefly note what is done vs remaining, then CONTINUE until the original request is fully done (all edits applied, tests/build pass). Do NOT stop with a status summary or ask the user to say 'continue'; call more tools if anything remains, else give the final completed answer.".to_string()
            } else {
                "Continue the task until the original request is fully complete; call more tools if anything remains, else give the final completed answer. Do NOT end with a partial summary when work is still pending.".to_string()
            };
            if repeat_streak >= 3 {
                nudge.push_str(&format!(
                    " Loop guard: this is the {repeat_streak}-th identical tool round in a row with no new result. Do NOT call the same tool with the same arguments again — change approach (different args or tool) or give the final text answer now."
                ));
            }
            // Steered input joins the active turn (Go takeSteered parity):
            // stored raw, sent vaulted, answered before anything else.
            let steered_raw = store.take_steered(session_id);
            if !steered_raw.trim().is_empty() {
                let _ = store.add_turn(session_id, "user", &steered_raw);
                let steered_sub = store.vault.substitute(session_id, &steered_raw).map_err(AgentError::Msg)?;
                nudge.push_str(&format!("\n\nSteered input (answer this too):\n{steered_sub}"));
                on_stage("steered \u{2713}");
            }
            messages.push(llm::Message { role: "user".into(), content: nudge, thinking: String::new() });
            // Skip empty assistant echoes so the model doesn't pattern-match
            // its own filler ("I'll query...") into a retry loop.
            // (Continuation nudges above are deterministic scaffolding:
            // re-added by the loop, never persisted.)
            // Flush this round's reasoning as its own dialog (cap mirrors
            // the 4KB tool-result cap into context).
            let capped_thinking: String = thinking_buf.chars().take(4000).collect();
            if !capped_thinking.trim().is_empty() {
                let resolved = store.vault.resolve(session_id, &capped_thinking).map_err(AgentError::Msg)?;
                store.add_turn(session_id, "thinking", &resolved)?;
            }
            if !last_text.trim().is_empty() {
                messages.push(llm::Message { role: "assistant".into(), content: store.vault.substitute(session_id, &last_text).map_err(AgentError::Msg)?, thinking: String::new() });
                let interim = store.vault.resolve(session_id, &last_text).map_err(AgentError::Msg)?;
                store.add_turn(session_id, "assistant", &interim)?;
                last_nonempty = last_text.clone();
            }
            last_text.clear();
            thinking_buf.clear();
        }
        // Exhausted ceiling with tools still pending: return the last real
        // model text plus an explicit resume pointer instead of an empty
        // turn (empty turns read as silent stops and force `continue`).
        let tail = if last_nonempty.trim().is_empty() {
            "Tool budget exhausted after 120 tool rounds with work still pending. Say `continue` to resume: the next turn should list exactly what remains and the precise next tool calls (names + arguments).".to_string()
        } else {
            last_nonempty
        };
        // User transparency: resolve any refs the model echoed before storing.
        let mut tail = store.vault.resolve(session_id, &tail).map_err(AgentError::Msg)?;
        if mutated && !verified {
            tail.push_str("\n\n");
            tail.push_str(UNVERIFIED_POINTER);
        }
        store.add_turn(session_id, "assistant", &tail)?;
        Ok((tail, usage))
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
        let text = store.vault.resolve(session_id, &text).unwrap_or(text);
        let _ = store.add_turn(session_id, "assistant", &text);
        text
    }
}

/// Tools whose execution mutates the workspace (checkpoint trigger).
fn is_mutating(name: &str) -> bool {
    matches!(name, "edit" | "write" | "bash" | "memory_write" | "patch")
}

/// Canonicalizes one invocation for loop detection: lowercase name plus
/// sorted args. task_output returns "" (exempt — polling a background task
/// to completion is progress, not a loop).
fn tool_call_key(name: &str, args: &HashMap<String, serde_json::Value>) -> String {
    if name.eq_ignore_ascii_case("task_output") {
        return String::new();
    }
    let mut kv: Vec<(&String, &serde_json::Value)> = args.iter().collect();
    kv.sort_by(|a, b| a.0.cmp(b.0));
    let obj: serde_json::Map<String, serde_json::Value> =
        kv.into_iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    format!("{}:{}", name.to_lowercase(), serde_json::Value::Object(obj))
}

/// Condenses tool output for live display (300 chars, single line).
fn tool_preview(out: &str) -> String {
    let one: String = out.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut s: String = one.chars().take(300).collect();
    if one.chars().count() > 300 {
        s.push('\u{2026}');
    }
    s
}

/// Prefixes a transcript turn with its target (command/path/URL) so repeats
/// stay distinguishable after compaction drops the call args.
fn tool_label(name: &str, args: &HashMap<String, serde_json::Value>) -> String {
    let target = crate::tools::tool_target(name, args);
    if target.is_empty() || target.eq_ignore_ascii_case(name) {
        name.to_string()
    } else {
        format!("{name}: {target}")
    }
}

/// Same-file re-read nudge: paging one file with different offsets evades
/// the identical-call key. Fires once, on the 4th read of a path per turn.
fn read_repeat_nudge(target: &str, n: usize) -> Option<String> {
    if target.is_empty() || n != 4 {
        return None;
    }
    Some(format!(
        "\n\nLoop guard: `{target}` already read {n} times this turn. Do NOT re-page it — use grep/glob for the next fact, or batch the remaining reads in one round, then act."
    ))
}

/// Hard cap behind the nudge: block the 6th+ read of one path per turn.
/// `n_before` is the completed-read count so far; advisory nudges (4th)
/// don't stop a stuck loop, so the 6th attempt never reaches the
/// filesystem — it returns a short error naming the file instead.
fn read_repeat_blocked(tool: &str, target: &str, n_before: usize) -> Option<String> {
    if !tool.eq_ignore_ascii_case("read") || target.is_empty() || n_before < 5 {
        return None;
    }
    Some(format!(
        "read blocked: `{target}` already read {n} times this turn — act on what you have (grep/glob for the next fact, then edit) instead of re-reading it.",
        n = n_before + 1
    ))
}

/// Verify-after-fix gate: does this bash command run a test, build,
/// typecheck, or lint suite whose green run proves a fix? Substring match
/// on the lowered command; entries need enough context to avoid traps
/// like `cat tsconfig.json` (no bare `tsc`).
fn is_verify_command(cmd: &str) -> bool {
    const VERIFY: &[&str] = &[
        "cargo test",
        "cargo clippy",
        "cargo build",
        "cargo check",
        "tsc --noemit",
        "tsc -b",
        "tsc -p",
        "vite build",
        "next build",
        "pnpm build",
        "pnpm test",
        "npm test",
        "npm run build",
        "npm run test",
        "pytest",
        "go test",
        "make test",
        "make build",
        "make lint",
        "jest",
        "vitest",
        "eslint",
    ];
    let lower = cmd.to_lowercase();
    if VERIFY.iter().any(|v| lower.contains(v)) {
        return true;
    }
    // `pnpm --dir <dir> <sub>` never contains "pnpm <sub>" contiguously
    // (the session's own `pnpm --dir web build`); match the runner plus
    // a build/test subcommand anywhere in the line.
    lower.contains("pnpm") && (lower.contains("build") || lower.contains("test"))
}

/// Resume pointer carried by turns whose code edits never saw a passing
/// verification run — the next `continue` must verify until green.
const UNVERIFIED_POINTER: &str = "Note: this turn's edits were never verified (no passing test/build run) — say `continue` to verify: run the relevant tests/build and fix failures until green.";




/// Scoped subagent run: fresh context with a builtin (general|explore) system
/// prompt, restricted tool specs, max 6 steps, no further delegation.
async fn run_subtask(
    router: &Router,
    cfg: &crate::config::Config,
    vault: &crate::vault::Vault,
    parent: &SecCtx,
    req: &crate::tools::SubagentReq,
) -> Result<String, AgentError> {
    let bundle = Bundle::load(&parent.workspace, cfg.compat.import_global_rules, cfg.compat.import_global_skills);
    // Custom agents (mode subagent|all) plus builtins; primary-only rejects.
    let def = bundle
        .agents
        .iter()
        .find(|a| a.name.eq_ignore_ascii_case(&req.agent))
        .cloned()
        .or_else(|| {
            crate::rules::builtin_agents().into_iter().find(|a| a.name.eq_ignore_ascii_case(&req.agent))
        })
        .ok_or_else(|| {
            let mut names: Vec<String> =
                crate::rules::builtin_agents().into_iter().map(|a| a.name).collect();
            names.extend(bundle.agents.iter().map(|a| a.name.clone()));
            AgentError::Msg(format!("unknown subagent {:?} (available: {})", req.agent, names.join(", ")))
        })?;
    if def.mode != "subagent" && def.mode != "all" && !def.mode.is_empty() {
        return Err(AgentError::Msg(format!(
            "subagent: agent {:?} is primary-only (mode={:?})",
            req.agent, def.mode
        )));
    }
    // Explicit model override wins; validate the provider exists.
    let mut model = if !req.model.is_empty() {
        if router.provider_for(&req.model).is_err() {
            return Err(AgentError::Msg(format!(
                "subagent: no provider for model {:?} (use provider/model)",
                req.model
            )));
        }
        req.model.clone()
    } else if !def.model.is_empty() {
        def.model.clone()
    } else {
        String::new()
    };
    if model.is_empty() {
        model = router.model_for("executor", "build");
    }
    // Guard the child context window: a model-drafted delegation prompt can
    // balloon. Truncate with a note instead of blowing the token budget.
    let mut prompt = req.task.clone();
    if prompt.len() > 6000 {
        prompt.truncate(6000);
        prompt.push_str("\n…[delegation prompt truncated at 6000 chars]");
    }
    let mut description = req.description.clone();
    if description.len() > 2000 {
        description.truncate(2000);
        description.push_str("…[truncated]");
    }
    let mut prompt_text = format!("You are the {} subagent. {}\n\nScope: use only the provided tools; you cannot launch subagents. When done, summarize findings with file references.", def.name, def.body);
    if !description.is_empty() {
        prompt_text.push_str("\n\nTask context: ");
        prompt_text.push_str(&description);
    }
    prompt_text.push_str(crate::vault::VAULT_GUIDANCE);
    prompt_text.push('\n');
    // Explore is read-only plus task polling; general gets everything but subagent.
    let specs: Vec<ToolSpec> = Runner::tool_specs_filtered(cfg.websearch.disabled)
        .into_iter()
        .filter(|t| {
            if req.agent.eq_ignore_ascii_case("explore") {
                matches!(
                    t.name.as_str(),
                    "read" | "glob" | "grep" | "lsp" | "skill"
                        | "kb_query" | "webfetch" | "websearch" | "bash" | "task_output"
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
        websearch_disabled: cfg.websearch.disabled,
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
        llm::Message { role: "system".into(), content: prompt_text, thinking: String::new() },
        llm::Message { role: "user".into(), content: vault.substitute(&parent.session, &prompt).map_err(AgentError::Msg)?, thinking: String::new() },
    ];
    let mut last = String::new();
    for _ in 0..32 {
        let (txs, mut rxs) = mpsc::unbounded_channel::<Chunk>();
        let req_msg = Request {
            model: model.clone(),
            messages: messages.clone(),
            tools: specs.clone(),
            max_tokens: 0,
            reasoning_effort: String::new(),
            images: vec![],
        };
        let router2 = router.clone();
        let handle = tokio::spawn(async move {
            match tokio::time::timeout(crate::llm::STREAM_TIMEOUT, router2.chat_stream("executor", "build", req_msg, txs)).await {
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
            return vault.resolve(&parent.session, &last).map_err(AgentError::Msg);
        }
        for tc in &calls {
            let args_val = serde_json::Value::Object(
                tc.args.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            );
            let resolved = vault.resolve_value(&parent.session, &args_val);
            let args: HashMap<String, serde_json::Value> = resolved
                .as_object()
                .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
                .unwrap_or_default();
            let result = reg.call(&sc, &tc.name, &args);
            let rendered = match result {
                Ok(o) => format!("tool {} →\n{}", tc.name, o.output),
                Err(e) => format!("tool {} failed: {e}", tc.name),
            };
            let sub = vault.substitute(&parent.session, &format!("[tool result {}]\n{rendered}", tc.name)).map_err(AgentError::Msg)?;
            messages.push(llm::Message { role: "user".into(), content: sub, thinking: String::new() });
        }
        let sub = vault.substitute(&parent.session, &last).map_err(AgentError::Msg)?;
        messages.push(llm::Message { role: "assistant".into(), content: sub, thinking: String::new() });
        last.clear();
    }
    vault.resolve(&parent.session, &last).map_err(AgentError::Msg)
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
        // Vault boundary: the digest may carry values — send substituted,
        // store the summary resolved.
        let summary = {
            let digest_sub = store.vault.substitute(session_id, &digest).map_err(AgentError::Msg)?;
            let (tx, mut rx) = mpsc::unbounded_channel::<Chunk>();
            let req = Request {
                model: String::new(),
                messages: vec![
                    llm::Message { role: "system".into(), content: "Summarize this coding session's middle turns in 10 bullet lines: decisions, file changes, open threads. No preamble.".into(), thinking: String::new() },
                    llm::Message { role: "user".into(), content: digest_sub, thinking: String::new() },
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
                Ok(Ok(_)) if !text.trim().is_empty() => store.vault.resolve(session_id, &text).map_err(AgentError::Msg)?,
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
mod loop_key_tests {
    use super::*;

    fn a(pairs: &[(&str, &str)]) -> HashMap<String, serde_json::Value> {
        pairs.iter().map(|(k, v)| ((*k).into(), serde_json::Value::String((*v).into()))).collect()
    }

    #[test]
    fn identical_calls_share_a_key() {
        let k1 = tool_call_key("bash", &a(&[("command", "ls"), ("timeout_ms", "5")]));
        let k2 = tool_call_key("BASH", &a(&[("timeout_ms", "5"), ("command", "ls")]));
        assert_eq!(k1, k2, "name case + arg order must not matter");
        assert_ne!(k1, tool_call_key("bash", &a(&[("command", "ls")])), "different args differ");
    }

    #[test]
    fn task_output_is_exempt_and_labeled() {
        assert_eq!(tool_call_key("task_output", &a(&[("task_id", "task-1")])), "", "polls are progress, not loops");
        assert_eq!(tool_label("bash", &a(&[("command", "ls")])), "bash: ls");
        assert_eq!(tool_label("read", &a(&[])), "read");
    }

    #[test]
    fn reread_nudge_fires_once_on_fourth_read() {
        assert!(read_repeat_nudge("web/src/App.tsx", 3).is_none());
        let nudge = read_repeat_nudge("web/src/App.tsx", 4).expect("4th read nudges");
        assert!(nudge.contains("web/src/App.tsx"), "{nudge}");
        assert!(read_repeat_nudge("web/src/App.tsx", 5).is_none(), "fires once, not spam");
        assert!(read_repeat_nudge("", 4).is_none());
    }

    #[test]
    fn reread_block_fires_on_sixth_read() {
        assert!(read_repeat_blocked("read", "web/src/App.tsx", 4).is_none(), "5th read still advisory-only");
        let msg = read_repeat_blocked("read", "web/src/App.tsx", 5).expect("6th read blocks");
        assert!(msg.contains("web/src/App.tsx"), "{msg}");
        assert!(read_repeat_blocked("read", "web/src/App.tsx", 9).is_some(), "stays blocked, no re-page");
        assert!(read_repeat_blocked("bash", "ls", 9).is_none(), "only reads are capped");
        assert!(read_repeat_blocked("read", "", 9).is_none());
    }

    #[test]
    fn verify_command_detection() {
        assert!(is_verify_command("cargo test --quiet loop_key"));
        assert!(is_verify_command("cargo clippy --tests"));
        assert!(is_verify_command("pnpm --dir web build"));
        assert!(is_verify_command("pnpm --dir web test && pnpm --dir web build"));
        assert!(is_verify_command("tsc --noEmit"));
        assert!(is_verify_command("pytest -x tests/"));
        assert!(is_verify_command("go test ./..."));
        assert!(!is_verify_command("ls -la"), "listing is not verification");
        assert!(!is_verify_command("cat tsconfig.json"), "tsconfig trap: no bare tsc");
        assert!(!is_verify_command("pnpm install"), "install is not verification");
        assert!(!is_verify_command("git diff --stat HEAD"), "diff is not verification");
        assert!(!is_verify_command("echo done"), "echo is not verification");
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
            .run_prompt(&store, &router, &cfg, &sess.id, "read a.txt", "", &[], |_, _| {}, |_| {}, |_| {})
            .await
            .unwrap();
        assert_eq!(out.text, "stuck: audit done, nothing left", "silent turn must end with a stuck summary, got {out:?}");
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
            .run_prompt(&store, &router, &cfg, &sess.id, "say nothing", "", &[], |_, _| {}, |_| {}, |_| {})
            .await
            .unwrap();
        assert!(out.text.trim().is_empty(), "no-work silence must pass through, got {out:?}");
        assert_eq!(hits.load(Ordering::SeqCst), 1, "no guard round trip for idle silence");
    }

    fn body_verify(n: usize) -> &'static str {
        match n {
            // Round 1: edit a.txt (alpha -> beta).
            1 => "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"1\",\"function\":{\"name\":\"edit\",\"arguments\":\"{\\\"path\\\":\\\"a.txt\\\",\\\"old_string\\\":\\\"alpha\\\",\\\"new_string\\\":\\\"beta\\\"}\"}}]}}]}\n\n",
            // Round 2: model ends with text, no tools — gate must refuse.
            2 => "data: {\"choices\":[{\"delta\":{\"content\":\"done\"}}]}\n\n",
            // Round 3: model runs a verify command (fails here: no crate
            // in the temp workspace — still counts as an attempt, and the
            // `[exit N]` marker keeps `verified` false).
            3 => "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"1\",\"function\":{\"name\":\"bash\",\"arguments\":\"{\\\"command\\\":\\\"cargo test --quiet\\\"}\"}}]}}]}\n\n",
            // Round 4: model gives up with text — gate refuses once more.
            4 => "data: {\"choices\":[{\"delta\":{\"content\":\"giving up\"}}]}\n\n",
            // Round 5: cap reached (2 forced rounds) — turn ends carrying
            // the unverified pointer.
            _ => "data: {\"choices\":[{\"delta\":{\"content\":\"still giving up\"}}]}\n\n",
        }
    }

    /// Verify-after-fix gate: an edit followed by a text finish must not
    /// end the turn. The gate forces verification rounds (cap 2), then
    /// releases with an explicit unverified resume pointer.
    #[tokio::test]
    async fn unverified_edit_forces_verify_rounds_then_pointer() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = std::sync::Arc::new(AtomicUsize::new(0));
        let hits2 = hits.clone();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            for _ in 0..5 {
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
                let n = hits2.fetch_add(1, Ordering::SeqCst) + 1;
                let payload = body_verify(n);
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
            .run_prompt(&store, &router, &cfg, &sess.id, "patch a.txt", "", &[], |_, _| {}, |_| {}, |_| {})
            .await
            .unwrap();
        assert_eq!(hits.load(Ordering::SeqCst), 5, "edit + 2 refused finishes + failing verify + release");
        assert_eq!(std::fs::read_to_string(dir.path().join("a.txt")).unwrap(), "beta", "edit must apply");
        assert!(out.text.contains("never verified"), "released turn must carry the pointer, got {out:?}");
        let turns = store.turns(&sess.id).unwrap();
        assert!(turns.iter().any(|t| t.text.contains("Unverified changes")), "gate demand must hit the transcript");
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
