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

/// Hash a rendered tool result for the identical-output ledger.
/// Byte-exact: same information in any tool counts together; any
/// guard-nudge suffix already applied counts as part of the text.
fn output_key(s: &str) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

/// Hash assistant text case- and whitespace-insensitively: filler
/// ("Comparing X — pulling Y." x5) matches verbatim; real answers
/// with new content do not.
fn text_key(s: &str) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let norm: String = s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase();
    let mut h = DefaultHasher::new();
    norm.hash(&mut h);
    h.finish()
}

/// Tool lifecycle for `turn.tool` SSE rows (Go OnToolStart/OnToolDone parity).
#[derive(Debug, Clone)]
pub enum ToolEvent {
    Start { name: String, target: String },
    Done { name: String, target: String, preview: String, error: bool, duration_ms: u64 },
}

/// Turn result: final text plus token usage for turn.completed stats.
/// `needs_continue` drives server-side auto-continue: an unfinished turn
/// (unverified edits, zero-edit work request, budget ceiling) loops back
/// in instead of asking the user to type `continue`.
#[derive(Debug, Clone, Default)]
pub struct TurnOutcome {
    pub text: String,
    pub usage: crate::llm::Usage,
    pub needs_continue: bool,
    pub continue_reason: ContinueReason,
}

/// Why a turn is unfinished (for the synthetic auto-continue prompt).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum ContinueReason {
    #[default]
    None,
    NeedsVerify,
    NeedsAction,
    BudgetExhausted,
}

impl ContinueReason {
    /// Synthetic user turn that re-arms the gates: names the miss and
    /// forbids ending with another status summary. Ends with a `fix`
    /// hint so the work gate stays armed even on a bare follow-up.
    pub fn auto_prompt(&self, n: usize, cap: usize) -> String {
        match self {
            ContinueReason::NeedsVerify => format!("auto-continue {n}/{cap}: this turn's edits were never verified (no passing test/build run) — fix: run the relevant tests/build now and repair failures until green. Do not end with a status summary."),
            ContinueReason::NeedsAction => format!("auto-continue {n}/{cap}: no edit/write/patch ran yet but the request needs a fix — make the change now, then verify it (run tests/build and fix failures until green). Do not end with another status summary."),
            ContinueReason::BudgetExhausted => format!("auto-continue {n}/{cap}: tool budget hit with work pending — fix: list exactly what remains and continue with the precise next tool calls. Do not end with a status summary."),
            ContinueReason::None => format!("auto-continue {n}/{cap}: fix: continue the work until done."),
        }
    }
}

/// Pure classifier shared by both turn exits (normal finish + budget
/// ceiling). Unverified edits and zero-edit work requests always need
/// another turn; a clean finish only continues when the budget gave out.
fn continue_status(
    mutated: bool,
    verified: bool,
    work_requested: bool,
    budget_exhausted: bool,
) -> (bool, ContinueReason) {
    if mutated && !verified {
        (true, ContinueReason::NeedsVerify)
    } else if !mutated && work_requested {
        (true, ContinueReason::NeedsAction)
    } else if budget_exhausted {
        (true, ContinueReason::BudgetExhausted)
    } else {
        (false, ContinueReason::None)
    }
}

#[derive(Debug, Error)]
pub enum AgentError {
    #[error("agent: {0}")]
    Msg(String),
    /// Cooperative stop (POST /stop): not a failure — the turn ends
    /// cleanly with a "Stopped by user." note, no retry.
    #[error("stopped by user")]
    Stopped,
    #[error(transparent)]
    Llm(#[from] llm::LlmError),
    #[error(transparent)]
    Store(#[from] crate::store::StoreError),
    #[error(transparent)]
    Tool(#[from] crate::tools::ToolError),
}

/// Assistant text persisted (and SSE-completed) when a stop lands.
pub const STOPPED_TEXT: &str = "Stopped by user.";

/// Persist a cooperative stop: flush pending reasoning, then a plain
/// assistant note so the transcript (and `continue`) reads sanely.
fn persist_stopped(store: &Store, session_id: &str, thinking_buf: &str) {
    // Reasoning lives in traces, not turns: turns are the replay prefix
    // (every byte billed every round), traces are debug-only. A thinking
    // turn would never be replayed (replay filters user/assistant only)
    // yet would inflate compaction and token estimates.
    if !thinking_buf.trim().is_empty() {
        let capped: String = thinking_buf.chars().take(4000).collect();
        if let Ok(resolved) = store.vault.resolve(session_id, &capped) {
            let _ = store.add_trace(session_id, "thinking", &resolved);
        }
    }
    let _ = store.add_turn(session_id, "assistant", STOPPED_TEXT);
}

pub struct Runner {
    pub max_steps: usize,
    /// Live approval broker (server/TUI mode). None = headless `run`.
    pub approval: Option<ApprovalGate>,
    /// Lumina-compatible JS/TS plugin worker (None when unconfigured).
    pub plugins: Option<Arc<Host>>,
    /// Shared language-server manager (servers persist across turns;
    /// cloned, never rebuilt, so `run` and the daemon reuse processes).
    pub lsp: Arc<crate::lsp::Manager>,
}

impl Runner {
    pub fn new() -> Self {
        // Auto-continue ceiling: must match Go hardCap (120). The old 16
        // ended long tasks with a silent stop that forced `continue`.
        Self { max_steps: 120, approval: None, plugins: None, lsp: Arc::new(crate::lsp::Manager::new(Default::default())) }
    }

    /// Attach the configured language servers (call once per process;
    /// the manager spawns servers lazily on first tool use).
    pub fn set_lsp(&mut self, cfg: crate::config::LspConfig) {
        self.lsp = Arc::new(crate::lsp::Manager::new(cfg));
    }

    fn tool_specs() -> Vec<ToolSpec> {
        Self::tool_specs_filtered(false, false)
    }

    fn tool_specs_filtered(hide_websearch: bool, hide_lsp: bool) -> Vec<ToolSpec> {
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
            ToolSpec { name: "bash".into(), description: "Run a shell command in the workspace (git/cargo/pnpm/tests; workdir optional; background:true runs detached; poll with task_output). Prefer native read/grep/glob for file search/read/listing, not bash equivalents.".into(), schema: obj(&["command", "timeout_ms"]) },
            ToolSpec { name: "grep".into(), description: "Regex-search workspace files.".into(), schema: obj(&["pattern", "include"]) },
            ToolSpec { name: "lsp".into(), description: "Semantic code intelligence via the workspace language server (rust-analyzer/gopls/typescript-language-server/pylsp): op=symbols|definition|references|diagnostics on a workspace-relative path (1-based line/character, 0/0 = file start; query filters symbol names). Cross-file questions first — cheaper and more precise than grep chains.".into(), schema: serde_json::json!({"type": "object", "properties": {"op": {"type": "string"}, "path": {"type": "string"}, "line": {"type": "integer"}, "character": {"type": "integer"}, "query": {"type": "string"}}}) },
            ToolSpec { name: "glob".into(), description: "List workspace paths by glob.".into(), schema: obj(&["pattern"]) },
            ToolSpec { name: "kb_query".into(), description: "Query the workspace knowledge base (Markdown+graphify) AFTER intent is known. Only when AGENTS.md/rules declare a KB dir; the tool says when no KB is configured (then skip). One call with request keywords, budget 4000; follow [[wikilinks]] only if needed.".into(), schema: serde_json::json!({"type": "object", "properties": {"query": {"type": "string"}, "budget": {"type": "integer"}}}) },
            ToolSpec { name: "memory_write".into(), description: "Save a durable fact to the rule-declared KB dir (<kb>/<topic>.md, deduped, secrets rejected). Post-completion only after a verified fix; never mid-turn speculation.".into(), schema: obj(&["topic", "content"]) },
            ToolSpec { name: "skill".into(), description: "Load a full SKILL.md body by name (only name+description are advertised).".into(), schema: obj(&["name"]) },
            ToolSpec { name: "question".into(), description: "Ask the user clarifying questions (interactive sessions only). Options are {label, description}; multiple:true allows several picks; headless runs get best-guess guidance.".into(), schema: serde_json::json!({"type": "object", "properties": {"questions": {"type": "array", "items": {"type": "object", "properties": {"question": {"type": "string"}, "header": {"type": "string"}, "multiple": {"type": "boolean"}, "options": {"type": "array", "items": {"type": "object", "properties": {"label": {"type": "string"}, "description": {"type": "string"}}, "required": ["label"]}}}, "required": ["question"]}}}, "required": ["questions"]}) },
            ToolSpec { name: "websearch".into(), description: "Search the web for current info (needs a provider key; pick with [websearch] provider).".into(), schema: obj(&["query", "provider"]) },
            ToolSpec { name: "webfetch".into(), description: "Fetch one HTTP(S) URL as markdown/text/HTML (cap 100KB).".into(), schema: obj(&["url", "format"]) },
            ToolSpec { name: "mcp".into(), description: "Call a local MCP tool (stdio/SSE; see .lumina/mcp.json).".into(), schema: obj(&["server", "tool", "args"]) },
            ToolSpec { name: "subagent".into(), description: format!("Launch a scoped subagent (general|explore). Subagents cannot delegate further. background:true returns a task ID; poll with task_output. Budget: max {SUBAGENT_BUDGET} per turn — read/grep/edit directly once blocked."), schema: obj(&["task", "agent"]) },
            ToolSpec { name: "task_output".into(), description: "Poll a background bash/subagent task by ID.".into(), schema: obj(&["task_id"]) },
            ToolSpec { name: "todowrite".into(), description: "Track task progress as a todo list.".into(), schema: serde_json::json!({"type": "object", "properties": {"todos": {"type": "array", "items": {"type": "object", "properties": {"content": {"type": "string"}, "status": {"type": "string"}}, "required": ["content"]}}}}) },
            ToolSpec { name: "patch".into(), description: "Apply a *** Begin Patch *** multi-file diff (add/update/delete/move); unified diff also accepted.".into(), schema: obj(&["patchText", "patch_text", "patch", "diff"]) },
        ]
        .into_iter()
        .filter(|t| !(hide_websearch && t.name == "websearch"))
        .filter(|t| !(hide_lsp && t.name == "lsp"))
        .collect()
    }

    fn system_prompt(bundle: &Bundle, agent: Agent) -> String {
        let mut s = bundle.system_prompt.clone();
        s.push_str("\nYou are Lumina, a local-first coding agent. Answer with tool calls when action helps; otherwise reply directly.\n");
        s.push_str("Persistence: continue the ORIGINAL user request until fully complete (all edits applied, tests/build pass). Do NOT stop with a status summary, do NOT ask the user to say 'continue', and do NOT end with 'what I found' when fixes were requested — finish the work, then reply with the completed result.\n");
        s.push_str("Fix/build/change requests are not done with zero edits: make the change (then verify it) or state exactly what is missing or blocked — ending with a status summary and no edits is not done.\n");
        s.push_str("Verify every code change in the same turn: a turn that ran edit/write/patch must also run a passing test/build/typecheck command (cargo test, pnpm build, tsc, …) before replying done — an edit without a green verification run is not done.\n");
        if agent == Agent::Plan {
            s.push_str("PLAN MODE: research and blueprint only. Never edit source files; scoped writes to plan.md or .lumina/plans/*.md only.\n");
        }
        s.push_str("Tool guidance: use native tools, not bash equivalents — read (not bash cat/head/tail), grep (not bash grep/rg), glob (not bash find/ls); KB protocol is lazy: read the user request FIRST, use `question` when intent is ambiguous, then at most one `kb_query` with request keywords (budget 4000) and only when a KB is configured (the tool reports when none exists — then skip KB and proceed with code/tools); never preload memory bodies into the prompt, carry IDs/stubs; `memory_write` runs post-completion only after a verified fix, never mid-turn speculation; batch independent reads; quote file:line references. After tool results arrive, keep going until done; call more tools if anything remains.\n");
        s.push_str(crate::vault::VAULT_GUIDANCE);
        s.push('\n');
        s.push_str(&bundle.skills_block());
        s
    }

    /// Persist one user turn per queued prompt (call once per turn; the
    /// retry path reuses the same turn instead of storing a duplicate and
    /// replaying already-executed tools).
    pub fn begin_turn(
        &self,
        store: &Store,
        session_id: &str,
        text: &str,
    ) -> Result<crate::store::Turn, AgentError> {
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
        Ok(store.add_turn(session_id, "user", &user_text)?)
    }

    /// Run one user turn to completion. `user_turn` must come from
    /// [`Runner::begin_turn`] (one per queued prompt, retries included).
    /// `on_event` receives streaming deltas
    /// (SSE broker in server mode; headless `run` ignores it).
    // ponytail: allow 11-arg hot path (single call site; struct would churn diff).
    #[allow(clippy::too_many_arguments)]
    pub async fn run_prompt(
        &self,
        store: &Store,
        router: &Router,
        cfg: &crate::config::Config,
        session_id: &str,
        user_turn: &crate::store::Turn,
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
        let user_text = user_turn.text.clone();

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
        let history = store.turns(session_id)?;
        // Follow-up prompts ("continue", "done?") carry no intent by
        // themselves: inherit the session's original task for gate
        // decisions, or the gates stay blind exactly when the user says
        // the least (7614829e "continue" turn: 124 tools, zero edits, no
        // gate fired). First non-tool user turn is the task by
        // construction (tool results only ever follow a prompt).
        let first_task = history
            .iter()
            .find(|t| t.role == "user" && !t.text.trim_start().starts_with("[tool result "))
            .map(|t| t.text.clone())
            .unwrap_or_default();
        for t in &history {
            // Replay persisted history, skipping the user turn just stored
            // above (re-added below) to avoid sending it twice.
            // Vault boundary: the provider sees refs, never values.
            if t.id == user_turn.id {
                continue;
            }
            if t.role == "user" || t.role == "assistant" {
                let sub = store.vault.substitute(session_id, &t.text).map_err(AgentError::Msg)?;
                messages.push(llm::Message { role: t.role.clone(), content: sub, thinking: String::new() });
            }
        }
        // Vault boundary: store raw, send vaulted.
        let vaulted_user = store.vault.substitute(session_id, &user_text).map_err(AgentError::Msg)?;
        messages.push(llm::Message { role: "user".into(), content: vaulted_user, thinking: String::new() });

        // Subagent cost attribution: run_subtask returns its own usage, which
        // the executor closure accumulates here and run_prompt merges into the
        // turn total below (background deliveries included).
        let sub_usage = std::sync::Arc::new(std::sync::Mutex::new(crate::llm::Usage::default()));
        let sub_usage_exec = sub_usage.clone();
        let kb_override = cfg.kb.memory_dir.clone();
        let lsp_manager = self.lsp.clone();
        let reg = Registry {
            kb_query: Some(std::sync::Arc::new(move |ws, q, b| {
                let ov = if kb_override.trim().is_empty() { None } else { Some(kb_override.as_str()) };
                crate::kb::query_in(ws, q, b, ov).map_err(|e| e.to_string())
            })),
            lsp_query: Some(std::sync::Arc::new(move |ws, op, path, line, ch, filter| {
                lsp_manager.query(ws, op, path, line, ch, filter).map_err(|e| e.to_string())
            })),
            kb_memory_dir: cfg.kb.memory_dir.clone(),
            bundle: Some(bundle),            websearch_provider: cfg.websearch.provider.clone(),
            websearch_disabled: cfg.websearch.disabled,
            plugins: self.plugins.clone(),
            disabled_tools: custom.as_ref().map(|d| {
                d.tools.iter().filter(|(_, &on)| !on).map(|(t, _)| t.clone()).collect()
            }).unwrap_or_default(),
            subagent: Some(Arc::new({
                let router = router.clone();
                let cfg = cfg.clone();
                let vault = store.vault.clone();
                let acc = sub_usage_exec.clone();
                let lsp = self.lsp.clone();
                move |sc: &SecCtx, req: &crate::tools::SubagentReq| {
                    // Sync tool context inside the async turn: park this worker
                    // thread and drive the subagent future to completion.
                    tokio::task::block_in_place(|| {
                        tokio::runtime::Handle::current().block_on(run_subtask(
                            &router, &cfg, &vault, sc, req, &lsp,
                        ))
                    })
                    .map(|(text, du)| {
                        let mut a = acc.lock().unwrap();
                        a.add(&du);
                        text
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
            sandbox: crate::sandbox::Profile::from_config(&cfg.sandbox),
        };
        let agent_name = sess.agent.clone();
        // Per-agent model default (falls back to the router executor tier).
        let model = if sess.model.is_empty() {
            custom.as_ref().and_then(|d| (!d.model.is_empty()).then(|| d.model.clone())).unwrap_or_default()
        } else {
            sess.model.clone()
        };
        let hide_websearch = cfg.websearch.disabled;
        let hide_lsp = cfg.lsp.disabled;
        // No-progress gate input (stuck session 81bdb764: a fix request
        // ended after 11 explore calls with zero edits and duplicated
        // filler). Plan mode is research-only by design, so it is exempt.
        // The original task counts, not just this prompt, so bare
        // follow-ups ("continue") keep the gate armed.
        let work_requested =
            agent != Agent::Plan && (looks_like_work(&user_text) || looks_like_work(&first_task));
        let mut outcome = self
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
                hide_lsp,
                work_requested,
            )
            .await?;
        // Merge subagent cost (side-channel: the executor closure above).
        // (All assistant turns are persisted inside the loop as they stream,
        // so transcripts and /resume replay show every tool round.)
        let extra = sub_usage.lock().unwrap();
        outcome.usage.add(&extra);
        // Persist per-turn usage for cost dashboards (opencode parity) and
        // emit a trace row so `traces` can answer "what did this turn cost".
        let _ = store.add_usage(session_id, &outcome.usage);
        let _ = store.add_trace(session_id, "usage", &serde_json::json!({
            "turn": user_turn.id, "input": outcome.usage.input, "output": outcome.usage.output,
            "cache_read": outcome.usage.cache_read, "cache_write": outcome.usage.cache_write,
        }).to_string());
        Ok(outcome)
    }

    // ponytail: allow 18-arg turn loop (single call site; struct would churn diff).
    #[allow(clippy::too_many_arguments)]
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
        hide_lsp: bool,
        work_requested: bool,
    ) -> Result<TurnOutcome, AgentError> {
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
        // No-progress gate (stuck session 81bdb764: a fix request ended
        // after 11 explore calls with zero edits and duplicated filler —
        // the verify gate above can't fire without mutations). Ending a
        // work request with no edit/write/patch forces up to 2 action
        // rounds, then releases as-is (no breakage risk without changes,
        // so no pointer — unlike the unverified tail below).
        let mut progress_rounds = 0usize;
        // Idle-round nudge (stuck session 7614829e "continue" turn: 124
        // tools over ~40 rounds — 43 todowrite re-plans, 66 explore bashes
        // — zero edits, no finish attempt for the end-gates to catch).
        // Counts consecutive mutation-free rounds; task_output-only
        // polling rounds are waiting, not idling, and don't count.
        let mut idle_rounds = 0usize;
        // Same-file re-read ledger: paging one file with different
        // offsets evades the identical-call key above (the stuck session
        // re-read SessionView.tsx 10x). Nudge once per file per turn.
        let mut read_counts: HashMap<String, usize> = HashMap::new();
        // Per-turn subagent budget: one subagent fans out to its own
        // trajectory, so unbounded delegation turns one stuck turn into
        // dozens of billed trajectories (stuck session cc686408: 30
        // subagents at 2-10 min each, some proxy-reading a read-capped
        // file to dodge the cap below).
        let mut subagent_calls = 0usize;
        let mut prev_round_key = String::new();
        let mut repeat_streak = 0usize;
        // Native-tool redirect: bash cat/grep/find/ls burns rounds the
        // dedicated tools do in one call (session 843e971d: 80 bash for
        // 4 edits). First occurrence is advisory; repeats are blocked in
        // the execution guard above, so this stays a one-shot nudge.
        let mut bash_nudged = false;
        // Session de530754 (105 turns, 1.2M tokens on a read-only audit):
        // 16x identical todowrite, 8x identical glob-no-matches, 5x+4x
        // verbatim filler text — all with VARIED calls/args, so the
        // identical-call key never fired. Three ledgers close the gap:
        // identical OUTPUTS (any tool), consecutive-identical todowrite
        // results, and identical assistant TEXT. Output/text repeats force
        // a tools-stripped answer round (max 2/turn); todowrite blocks.
        let mut out_counts: HashMap<u64, usize> = HashMap::new();
        let mut todowrite_streak = 0usize;
        let mut todowrite_last = 0u64;
        let mut text_hash = 0u64;
        let mut text_streak = 0usize;
        let mut forced_answers = 0u8;
        let mut force_answer = false;
        for round in 0..self.max_steps {
            // Cooperative stop (web Stop button / POST /stop): end the
            // turn at the next round boundary instead of running to the
            // step ceiling.
            if store.stop_requested(session_id) {
                persist_stopped(store, session_id, &thinking_buf);
                return Err(AgentError::Stopped);
            }
            let (tx, mut rx) = mpsc::unbounded_channel::<Chunk>();
            // Forced answer round: identical outputs/texts kept coming back
            // while the model kept tooling. Strip tools for exactly one
            // round so the model states its answer instead of re-running
            // what produces nothing new (max 2 per turn, then release).
            let mut req_tools = Self::tool_specs_filtered(hide_websearch, hide_lsp);
            let mut forced_this_round = false;
            if force_answer {
                force_answer = false;
                if forced_answers < 2 {
                    forced_answers += 1;
                    forced_this_round = true;
                    req_tools = Vec::new();
                    messages.push(llm::Message { role: "user".into(),
                        content: "Answer round: state your current answer, findings, or what remains — do NOT call tools this round (tool use is disabled for exactly this round). You can keep using tools afterwards if work remains.".into(),
                        thinking: String::new() });
                    on_stage("answering…");
                }
            }
            let req = Request {
                model: model.into(),
                messages: messages.clone(),
                tools: req_tools,
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
            // Poll the stream with a short timeout so a pending stop lands
            // mid-response instead of waiting out the 600s stream timeout.
            let mut stopped_midstream = false;
            loop {
                let chunk = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv()).await;
                let c = match chunk {
                    Ok(Some(c)) => c,
                    Ok(None) => break, // pump finished
                    Err(_) => {
                        if store.stop_requested(session_id) {
                            stopped_midstream = true;
                            break;
                        }
                        continue;
                    }
                };
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
            if stopped_midstream {
                handle.abort();
                persist_stopped(store, session_id, &thinking_buf);
                return Err(AgentError::Stopped);
            }
            let u = handle
                .await
                .map_err(|e| AgentError::Msg(format!("stream task: {e}")))??;
            usage.add(&u);
            // A non-compliant gateway can still emit tool calls on a
            // tools-stripped round (Anthropic would 400 them as undeclared).
            // Drop them: the round's contract is text-only.
            if forced_this_round && !calls.is_empty() {
                calls.clear();
                on_stage("tools held this round…");
            }
            if calls.is_empty() {
                // Verify-after-fix gate: the model wants to end the turn
                // with code mutations it never verified. Force up to two
                // verification rounds instead of accepting a text summary.
                if mutated && !verified && verify_rounds < 2 {
                    verify_rounds += 1;
                    let demand = "Unverified changes: this turn ran edit/write/patch but no passing test/build/typecheck command yet. Run the relevant verification now (e.g. `cargo test`, `pnpm --dir web build`) and fix failures until green; do NOT end with a text summary until verification passes. If verification is genuinely impossible in this workspace, state exactly what blocks it.";
                    // In-memory only: control-plane nudges must not persist
                    // as fake user turns (they corrupt replay/compaction and
                    // teach the model to discount real user input).
                    messages.push(llm::Message { role: "user".into(), content: demand.to_string(), thinking: String::new() });
                    on_stage("verifying…");
                    continue;
                }
                // No-progress gate: the model wants to end a work request
                // with zero edits (explore-only rounds + a status summary).
                // Force up to two action rounds instead; a false positive
                // costs at most two extra rounds, then the turn releases.
                if !mutated && work_requested && progress_rounds < 2 {
                    progress_rounds += 1;
                    let demand = "No changes made yet: this request asks to fix/build/change something, but the turn ran no edit/write/patch. Act now (make the change, then verify it) or state precisely what is missing or blocked — do NOT end with another status summary and zero edits.";
                    // In-memory only (see verify gate above).
                    messages.push(llm::Message { role: "user".into(), content: demand.to_string(), thinking: String::new() });
                    on_stage("acting…");
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
                        let _ = store.add_trace(session_id, "thinking", &resolved);
                        thinking_buf.clear();
                    }
                    let text = self
                        .empty_final_text(router, agent_name, model, effort, messages, store, session_id)
                        .await;
                    let (needs_continue, continue_reason) =
                        continue_status(mutated, verified, work_requested, false);
                    return Ok(TurnOutcome { text, usage, needs_continue, continue_reason });
                }
                let final_text = std::mem::take(&mut last_text);
                // Trailing reasoning archives to traces (turns stay
                // replay-clean: only user/assistant re-enter context).
                let capped_thinking: String = thinking_buf.chars().take(4000).collect();
                if !capped_thinking.trim().is_empty() {
                    let resolved = store.vault.resolve(session_id, &capped_thinking).map_err(AgentError::Msg)?;
                    let _ = store.add_trace(session_id, "thinking", &resolved);
                }
                // User transparency: stored transcript and return carry
                // values; refs only ever faced the provider.
                let mut final_text = store.vault.resolve(session_id, &final_text).map_err(AgentError::Msg)?;
                if mutated && !verified {
                    final_text.push_str("\n\n");
                    final_text.push_str(UNVERIFIED_POINTER);
                }
                // A work request ending with zero edits must not read as
                // done — the gate already forced action rounds and the
                // model still didn't act, so name the miss for `continue`.
                if !mutated && work_requested {
                    final_text.push_str("\n\n");
                    final_text.push_str(NO_PROGRESS_POINTER);
                }
                store.add_turn(session_id, "assistant", &final_text)?;
                let (needs_continue, continue_reason) =
                    continue_status(mutated, verified, work_requested, false);
                return Ok(TurnOutcome { text: final_text, usage, needs_continue, continue_reason });
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
            let mut round_acted = false;
            let mut round_only_polls = true;
            let mut round_executed = false;
            // Parallel fast path: a pure batch of side-effect-free reads
            // (the system prompt already tells the model to batch them)
            // runs concurrently; mixed batches stay sequential so
            // ordering/approval semantics never change. Args + block
            // verdicts are snapshotted up front (cheap, order-independent);
            // only the I/O runs on scope threads, ledger/render stays
            // sequential in call order below.
            let parallel_batch = calls.len() > 1 && calls.iter().all(|tc| is_parallel_tool(&tc.name));
            let mut batch_args: Vec<HashMap<String, serde_json::Value>> = Vec::new();
            let mut batch_blocked: Vec<Option<String>> = Vec::new();
            if parallel_batch {
                for tc in &calls {
                    let args_val = serde_json::Value::Object(
                        tc.args.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
                    );
                    let resolved = store.vault.resolve_value(session_id, &args_val);
                    let args: HashMap<String, serde_json::Value> = resolved
                        .as_object()
                        .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
                        .unwrap_or_default();
                    let b = loop_blocked(&tc.name, &args, &seen, &read_counts)
                        .or_else(|| subagent_guard(&tc.name, &args, &read_counts, subagent_calls));
                    batch_args.push(args);
                    batch_blocked.push(b);
                }
            }
            // Concrete I/O results for the parallel batch, in call order.
            // (Blocked slots stay None; sequential path computes inline.)
            let mut batch_results: Vec<Option<(Result<crate::tools::Outcome, crate::tools::ToolError>, u64)>> = Vec::new();
            if parallel_batch {
                std::thread::scope(|scope| {
                    let mut handles = Vec::with_capacity(calls.len());
                    for (i, tc) in calls.iter().enumerate() {
                        if batch_blocked[i].is_some() {
                            continue;
                        }
                        let args = batch_args[i].clone();
                        let name = tc.name.clone();
                        handles.push(scope.spawn(move || {
                            let t0 = std::time::Instant::now();
                            let r = reg.call(sc, &name, &args);
                            (i, r, t0.elapsed().as_millis() as u64)
                        }));
                    }
                    let mut out: Vec<Option<(Result<crate::tools::Outcome, crate::tools::ToolError>, u64)>> =
                        (0..calls.len()).map(|_| None).collect();
                    for h in handles {
                        if let Ok((i, r, ms)) = h.join() {
                            out[i] = Some((r, ms));
                        }
                    }
                    batch_results = out;
                });
            }
            for (idx, tc) in calls.iter().enumerate() {
                on_stage(&format!("tool: {}", tool_label(&tc.name, &tc.args)));
                if tc.name.eq_ignore_ascii_case("edit")
                    || tc.name.eq_ignore_ascii_case("write")
                    || tc.name.eq_ignore_ascii_case("patch")
                {
                    round_acted = true;
                }
                if !tc.name.eq_ignore_ascii_case("task_output") {
                    round_only_polls = false;
                }
                // Resolve direction: the model only ever saw refs, so its
                // args carry them. Resolve before execution (tools get
                // values). Fail closed on vault errors. Parallel batches
                // reuse the pre-resolved snapshot above so the I/O that
                // already ran on scope threads lines up with this ledger.
                let args: HashMap<String, serde_json::Value> = if parallel_batch {
                    batch_args[idx].clone()
                } else {
                    let args_val = serde_json::Value::Object(
                        tc.args.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
                    );
                    let resolved = store.vault.resolve_value(session_id, &args_val);
                    resolved
                        .as_object()
                        .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
                        .unwrap_or_default()
                };
                let target = crate::tools::tool_target(&tc.name, &args);
                on_tool(ToolEvent::Start { name: tc.name.clone(), target: target.clone() });
                // Identical calls hard-stop on the 4th attempt and re-reads
                // cap at the 6th (task_output exempt; edits clear the ledger
                // below since the state changed).
                let call_key = tool_call_key(&tc.name, &args);
                let was_blocked: bool;
                let subagent_ran: bool;
                let result: Result<crate::tools::Outcome, crate::tools::ToolError>;
                let elapsed_ms: u64;
                if parallel_batch {
                    let blocked = batch_blocked[idx].clone();
                    was_blocked = blocked.is_some();
                    // Parallel set is read-only by construction: no
                    // subagent, no ledger-clearing mutation.
                    subagent_ran = false;
                    if let Some(msg) = blocked {
                        result = Err(crate::tools::ToolError::Msg(msg));
                        elapsed_ms = 0;
                    } else if let Some(slot) = batch_results[idx].take() {
                        result = slot.0;
                        elapsed_ms = slot.1;
                    } else {
                        // Scope thread panicked or was skipped: fail closed
                        // rather than silently dropping the call.
                        result = Err(crate::tools::ToolError::Msg(format!("{}: parallel execution failed", tc.name)));
                        elapsed_ms = 0;
                    }
                } else {
                    let blocked = loop_blocked(&tc.name, &args, &seen, &read_counts)
                        .or_else(|| subagent_guard(&tc.name, &args, &read_counts, subagent_calls))
                        // Repeat native-redirectable bash (`ls` again after the
                        // advisory) never executes: the model already has the
                        // output, and arg variants (`ls` vs `ls -la`) dodge the
                        // identical-call key. First occurrence always runs.
                        .or_else(|| {
                            if !tc.name.eq_ignore_ascii_case("bash") {
                                return None;
                            }
                            bash_native_blocked(&crate::tools::tool_target(&tc.name, &args), bash_nudged)
                        })
                        // todowrite consecutive-identical guard (session
                        // de530754: 16x identical). Unlike the output ledger
                        // below (which can only react post-run), a streak of
                        // 7 identical results is pre-execution visible, so the
                        // 8th identical call never executes. Status flips
                        // reset the streak — only no-progress repeats block.
                        .or_else(|| todowrite_guard(&tc.name, todowrite_streak));
                    was_blocked = blocked.is_some();
                    subagent_ran = tc.name.eq_ignore_ascii_case("subagent") && blocked.is_none();
                    let t0 = std::time::Instant::now();
                    result = match blocked {
                        Some(msg) => Err(crate::tools::ToolError::Msg(msg)),
                        None => reg.call(sc, &tc.name, &args),
                    };
                    elapsed_ms = t0.elapsed().as_millis() as u64;
                }
                if !was_blocked {
                    round_executed = true;
                }
                let (preview, is_err) = match &result {
                    Ok(o) => (tool_preview(&o.output), false),
                    Err(e) => (tool_preview(&e.to_string()), true),
                };
                on_tool(ToolEvent::Done {
                    name: tc.name.clone(),
                    target: target.clone(),
                    preview,
                    error: is_err,
                    duration_ms: elapsed_ms,
                });
                let result_ok = result.is_ok();
                let rendered = match result {
                    Ok(o) => {
                        // Cap what flows back into context (full blobs stay out of
                        // the loop; the model can re-read files if it needs more).
                        // The overflow spills to a `tool_full` trace (16KB cap)
                        // so resume/debug keeps the data without re-billing it
                        // every round — the opencode lesson (4.2GB verbatim
                        // persistence) in reverse: keep replay lean, archive
                        // full output out-of-context.
                        let mut out = o.output;
                        let mut note = String::new();
                        if o.truncated || out.len() > 4000 {
                            let total = out.len();
                            let mut full = std::mem::take(&mut out);
                            let mut spill: String = full.chars().take(16_000).collect();
                            if spill.len() < full.len() {
                                spill.push_str(&format!("\n…[spill truncated at 16000 of {total} chars]"));
                            }
                            let _ = store.add_trace(session_id, "tool_full", &serde_json::json!({
                                "tool": tc.name, "target": target,
                                "total_chars": total, "output": spill,
                            }).to_string());
                            crate::tools::truncate_to(&mut full, 4000);
                            out = full;
                            note = format!("\n[truncated {total} chars to 4000; full output archived]");
                        }
                        format!("tool {} →\n{out}{note}", tc.name)
                    }
                    Err(e) => format!("tool {} failed: {e}", tc.name),
                };
                // Third identical execution: name the call and tell the
                // model to stop. Fires once per call per turn so the nudge
                // doesn't itself become transcript spam.
                let mut rendered = rendered;
                let key = call_key;
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
                // Bash-native redirect: cat/grep/find/ls via bash have
                // dedicated tools. Nudge once per turn, then stay quiet.
                if !bash_nudged && tc.name.eq_ignore_ascii_case("bash") && !was_blocked {
                    let cmd = crate::tools::tool_target(&tc.name, &args);
                    if let Some(nudge) = bash_native_nudge(&cmd) {
                        rendered.push_str(&nudge);
                        bash_nudged = true;
                    }
                }
                // todowrite consecutive-identical ledger: status flips
                // (0/4 -> 1/4) reset the streak; only back-to-back repeats
                // advance it. Advisory on the 4th, hard block from the 8th
                // (see todowrite_guard); streak advances on executed calls.
                if tc.name.eq_ignore_ascii_case("todowrite") && !was_blocked {
                    let h = output_key(&rendered);
                    if todowrite_streak > 0 && h == todowrite_last {
                        todowrite_streak += 1;
                    } else {
                        todowrite_streak = 1;
                        todowrite_last = h;
                    }
                    if let Some(nudge) = todowrite_nudge(todowrite_streak) {
                        rendered.push_str(&nudge);
                    }
                }
                // Identical-OUTPUT ledger (session de530754: 8x glob
                // "(no matches)", 2x identical kb dumps — all with varied
                // args, so the identical-call key never fired). task_output
                // polls are progress, not loops — exempt.
                if !tc.name.eq_ignore_ascii_case("task_output") {
                    let h = output_key(&rendered);
                    let n = out_counts.entry(h).or_insert(0);
                    *n += 1;
                    if *n >= 3 {
                        rendered.push_str(&format!(
                            "\n\nLoop guard: this exact result already came back {n} times with no new information — stop re-running what produces it. State your answer or call only tools that return NEW information."
                        ));
                        // Force a tools-stripped answer round (capped).
                        if forced_answers < 2 {
                            force_answer = true;
                        }
                    }
                }
                // Proxy-read ledger: a subagent task naming a file reads it
                // inside the child trajectory, so share the re-read ledger
                // (stuck session cc686408 dodged the read cap with 30
                // subagent proxy reads). Executed subagents also spend the
                // per-turn budget enforced above.
                if subagent_ran {
                    subagent_calls += 1;
                    for p in mentioned_paths(&target) {
                        *read_counts.entry(p).or_insert(0) += 1;
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
                    // New workspace state: identical-call ledger restarts
                    // (re-running the same check after an edit is new info).
                    // The output/text/todowrite ledgers restart too: prior
                    // outputs are stale once the tree changes.
                    seen.clear();
                    out_counts.clear();
                    todowrite_streak = 0;
                    text_streak = 0;
                    let target = crate::tools::tool_target(&tc.name, &args);
                    if !target.is_empty() {
                        read_counts.remove(&target);
                    }
                }
                // Bash-applied edits (session 7920b8fc: sed/python patches
                // via bash never set `mutated`, so the verify gate stayed
                // blind and the turn released as verified with untested
                // changes). A successful mutating-looking bash invalidates
                // prior verification like an edit does.
                // ponytail: keyword heuristic; a miss only costs the verify
                // nudge, never correctness.
                if result_ok
                    && tc.name.eq_ignore_ascii_case("bash")
                    && bash_looks_mutating(&crate::tools::tool_target(&tc.name, &args))
                {
                    mutated = true;
                    verified = false;
                    seen.clear();
                    out_counts.clear();
                    todowrite_streak = 0;
                    text_streak = 0;
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
            // Idle accounting for the nudge below: a round that attempted
            // a mutation resets the count; pure background-poll rounds
            // neither reset nor advance it.
            if round_acted {
                idle_rounds = 0;
            } else if !round_only_polls {
                idle_rounds += 1;
            }
            // Nudges are context overhead billed every round (~50-100
            // tokens x 120 rounds = up to 12k/turn) AND cache-prefix
            // breakers: the growing suffix invalidates the stable prefix
            // the provider would otherwise serve from cache. Send one only
            // when the loop actually needs steering: periodic checkpoint,
            // repeat streak, idle spin, or unverified drift. Steered user
            // input always joins (that is real input, not scaffolding).
            let mut nudge = String::new();
            if (round + 1) % 8 == 0 {
                nudge.push_str("Checkpoint: done vs remaining, then CONTINUE (finish edits + green verify) or give the final answer.");
            }
            if repeat_streak >= 3 {
                // Loop guard is load-bearing (stuck-session fix): name the
                // stall and demand a new approach.
                if !nudge.is_empty() {
                    nudge.push(' ');
                }
                nudge.push_str(&format!(
                    "Loop guard: {repeat_streak}-th identical tool round -- do NOT repeat the same call; change args/tool or answer now."
                ));
            }
            // Idle-round nudge: advisory only (research turns legitimately
            // explore), but a work request 12 rounds deep with zero edits
            // is spinning -- prod it to act or name the blocker. Fires every
            // 12th idle round so long loops keep getting prodded.
            if work_requested && idle_rounds > 0 && idle_rounds.is_multiple_of(12) {
                if !nudge.is_empty() {
                    nudge.push(' ');
                }
                nudge.push_str(&format!(
                    "Progress note: {idle_rounds} rounds, no edit yet -- make the change now (then verify) or state what is missing."
                ));
                on_stage("idling\u{2026}");
            }
            // Mid-loop verify prod: edits stacked with no green run drift
            // (stuck session cc686408 landed edits but never verified
            // before the budget ran out). Advisory until the end-gate
            // forces it.
            if mutated && !verified && (round + 1).is_multiple_of(12) {
                if !nudge.is_empty() {
                    nudge.push(' ');
                }
                nudge.push_str("Verify now: edits ran with no passing test/build yet -- run it and fix failures instead of stacking more edits.");
            }
            // Steered input joins the active turn (Go takeSteered parity):
            // stored raw, sent vaulted, answered before anything else.
            let steered_raw = store.take_steered(session_id);
            if !steered_raw.trim().is_empty() {
                let _ = store.add_turn(session_id, "user", &steered_raw);
                let steered_sub = store.vault.substitute(session_id, &steered_raw).map_err(AgentError::Msg)?;
                if !nudge.is_empty() {
                    nudge.push_str("\n\n");
                }
                nudge.push_str(&format!("Steered input (answer this too):\n{steered_sub}"));
                on_stage("steered \u{2713}");
            }
            if !nudge.is_empty() {
                messages.push(llm::Message { role: "user".into(), content: nudge, thinking: String::new() });
            }
            // Skip empty assistant echoes so the model doesn't pattern-match
            // its own filler ("I'll query...") into a retry loop.
            // (Continuation nudges above are deterministic scaffolding:
            // re-added by the loop, never persisted.)
            // Reasoning lives in traces, not turns (see persist_stopped):
            // replay only carries user/assistant, so a thinking turn would
            // be write-only bloat. Archive the segment for debug instead.
            let capped_thinking: String = thinking_buf.chars().take(4000).collect();
            if !capped_thinking.trim().is_empty() {
                let resolved = store.vault.resolve(session_id, &capped_thinking).map_err(AgentError::Msg)?;
                let _ = store.add_trace(session_id, "thinking", &resolved);
            }
            if !last_text.trim().is_empty() {
                messages.push(llm::Message { role: "assistant".into(), content: store.vault.substitute(session_id, &last_text).map_err(AgentError::Msg)?, thinking: String::new() });
                let interim = store.vault.resolve(session_id, &last_text).map_err(AgentError::Msg)?;
                // Identical-TEXT streak (session de530754: 5x + 4x verbatim
                // filler while tooling on). Empty texts are neutral; a
                // mutation already reset the streak above. Three in a row
                // with continued tool calls forces an answer round.
                let h = text_key(&interim);
                if h == text_hash {
                    text_streak += 1;
                } else {
                    text_hash = h;
                    text_streak = 1;
                }
                if text_streak >= 3 && round_executed && forced_answers < 2 {
                    force_answer = true;
                }
                store.add_turn(session_id, "assistant", &interim)?;
                last_nonempty = last_text.clone();
            }
            last_text.clear();
            thinking_buf.clear();
        }
        // Exhausted ceiling with tools still pending: always lead with an
        // explicit budget notice (session 7920b8fc ended with a bare echo
        // of the last filler text, which read as done with work remaining).
        // The last model text follows for context, then resume pointers.
        let tail = ceiling_tail(&last_nonempty, self.max_steps, mutated, verified, work_requested);
        // User transparency: resolve any refs the model echoed before storing.
        let tail = store.vault.resolve(session_id, &tail).map_err(AgentError::Msg)?;
        store.add_turn(session_id, "assistant", &tail)?;
        let (needs_continue, continue_reason) =
            continue_status(mutated, verified, work_requested, true);
        Ok(TurnOutcome { text: tail, usage, needs_continue, continue_reason })
    }

    /// One tools-stripped round asking the model to state done-vs-remaining
    /// after a silent turn end. Never fails the turn: a still-silent model
    /// yields the canned resume pointer.
    #[allow(clippy::too_many_arguments)]
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

/// Side-effect-free tools safe for concurrent execution inside one model
/// batch: pure reads plus skill-body loads (bundle lookup, no I/O beyond).
/// Everything else (bash/edit/write/patch/mcp/subagent/question/web*,
/// kb_query which may call the LLM, task_output polling) stays sequential
/// so ordering, approval, and ledger semantics never change.
fn is_parallel_tool(name: &str) -> bool {
    matches!(
        name.to_lowercase().as_str(),
        "read" | "glob" | "grep" | "lsp" | "skill"
    )
}

/// Canonicalizes one invocation for loop detection: lowercase name plus
/// sorted args. task_output returns "" (exempt — polling a background task
/// to completion is progress, not a loop). subagent canonicalizes to
/// agent + mentioned paths: full task text is unique per call, which let
/// stuck session cc686408 fire 30 subagents with no repeat ever tripping.
fn tool_call_key(name: &str, args: &HashMap<String, serde_json::Value>) -> String {
    if name.eq_ignore_ascii_case("task_output") {
        return String::new();
    }
    if name.eq_ignore_ascii_case("subagent") {
        let agent = args
            .get("agent")
            .or_else(|| args.get("name"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_lowercase();
        let task = args
            .get("task")
            .or_else(|| args.get("prompt"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        return format!("subagent:{}:{}", agent, mentioned_paths(task).join(","));
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

/// Per-turn subagent budget: one subagent fans out to its own multi-round
/// trajectory, so unbounded delegation turns one stuck turn into dozens of
/// billed trajectories (stuck session cc686408: 30 subagents, 2-10 min each).
const SUBAGENT_BUDGET: usize = 8;

/// Path-like tokens in free text (`src/server/sessions.js`, backtick- or
/// `file:line`-wrapped). Used to canonicalize subagent loop keys and to
/// catch proxy re-reads of a read-capped file.
fn mentioned_paths(text: &str) -> Vec<String> {
    let mut out: Vec<String> = text
        .split(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '`' | '(' | ')' | ',' | ';'))
        .filter_map(|tok| {
            let t = tok.trim_matches(|c| matches!(c, '.' | ',' | ':' | ';' | '!' | '?' | ')'));
            if t.len() >= 3 && t.len() < 200 && t.contains('/') && t.contains('.') && !t.contains("://") {
                Some(t.split(':').next().unwrap_or(t).to_string())
            } else {
                None
            }
        })
        .filter(|p| p.contains('/') && p.contains('.'))
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Pre-execution loop guard: the 4th identical call blocks and the 6th
/// re-read of one path blocks. task_output is exempt (empty key); callers
/// clear `seen` after a mutation since the state changed.
fn loop_blocked(
    name: &str,
    args: &HashMap<String, serde_json::Value>,
    seen: &HashMap<String, usize>,
    read_counts: &HashMap<String, usize>,
) -> Option<String> {
    let key = tool_call_key(name, args);
    if !key.is_empty() && seen.get(&key).copied().unwrap_or(0) >= 3 {
        return Some(format!(
            "loop blocked: identical `{}` call already ran 3 times this turn with no new result — change approach (different args or tool) or give the final text answer now.",
            tool_label(name, args)
        ));
    }
    let target = crate::tools::tool_target(name, args);
    read_repeat_blocked(name, &target, read_counts.get(&target).copied().unwrap_or(0))
}

/// Subagent pre-execution guard: per-turn budget plus proxy-read of a
/// read-capped file (suffix-matched: tasks cite abs paths while reads are
/// keyed by the given arg). Returns the denial message when blocked.
fn subagent_guard(
    name: &str,
    args: &HashMap<String, serde_json::Value>,
    read_counts: &HashMap<String, usize>,
    subagent_calls: usize,
) -> Option<String> {
    if !name.eq_ignore_ascii_case("subagent") {
        return None;
    }
    let task = args
        .get("task")
        .or_else(|| args.get("prompt"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    for p in mentioned_paths(task) {
        if let Some((k, n)) = read_counts
            .iter()
            .find(|(k, _)| *k == &p || p.ends_with(k.as_str()) || k.as_str().ends_with(p.as_str()))
        {
            if *n >= 5 {
                return Some(format!(
                    "subagent blocked: `{k}` already read {n} times this turn (proxy reads share the ledger) — act on what you have (grep/glob for the next fact, then edit) instead of delegating another read of it."
                ));
            }
        }
    }
    if subagent_calls >= SUBAGENT_BUDGET {
        return Some(format!(
            "subagent budget exhausted: {subagent_calls} subagents already ran this turn — act directly with read/grep/edit instead of delegating further."
        ));
    }
    None
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

/// Bash-native redirect: `grep`/`cat`/`find`/`ls` via bash burn rounds
/// the dedicated tools do in one call. Matches only when the command
/// starts with the native binary (piped `git diff | grep` still needs
/// shell, so it never nudges). Caller fires once per turn.
/// Sequential `;`/`&&` chains decompose: `pwd; ls -la` is a bare `ls`
/// wearing a prefix (smoke test 2026-11-02 used exactly this shape twice,
/// dodging the first-token classifier). Piped commands still need shell
/// (`git diff | grep foo`), so any `|` exempts the whole line. Returns the
/// native tool for the first redirectable segment, if any. Compounds that
/// also verify (`cargo test && ls`) are exempt everywhere — blocking them
/// would kill the test run the verify gate requires.
fn native_segment(cmd: &str) -> Option<(String, &'static str)> {
    if cmd.contains('|') || is_verify_command(cmd) {
        return None;
    }
    let normalized = cmd.replace("&&", ";").replace("||", ";");
    for seg in normalized.split(';') {
        let first = seg.split_whitespace().next().unwrap_or("").to_lowercase();
        if first.is_empty() {
            continue;
        }
        if let Some(native) = native_tool_for(&first) {
            return Some((first, native));
        }
    }
    None
}

fn bash_native_nudge(cmd: &str) -> Option<String> {
    let (first, native) = native_segment(cmd)?;
    Some(format!(
        "\n\nTool guidance: `bash: {first}` has a native `{native}` tool — use it instead of bash for file search/read/listing; reserve bash for git/cargo/pnpm/tests."
    ))
}

/// Hard cap behind the nudge: the advisory alone doesn't redirect (smoke
/// test 2026-11-02: 4x bare `ls` ignored it, `ls`/`ls -la` arg variants
/// also dodge the identical-call key). Once the advisory has fired, any
/// further native-redirectable bash call this turn is blocked outright —
/// the model already has the listing, and the error names the native
/// tool. First occurrence always runs (advisory path, not this).
fn bash_native_blocked(cmd: &str, nudged: bool) -> Option<String> {
    if !nudged {
        return None;
    }
    let (first, native) = native_segment(cmd)?;
    Some(format!(
        "bash redirected: `bash {first}` already ran once via bash this turn — use the native `{native}` tool from here on (reserve bash for git/cargo/pnpm/tests)."
    ))
}

/// Classifier shared by the advisory nudge and the hard redirect above.
fn native_tool_for(first: &str) -> Option<&'static str> {
    match first {
        "grep" | "rg" | "ag" => Some("grep"),
        "cat" | "head" | "tail" | "wc" | "less" | "more" => Some("read"),
        "find" | "ls" | "fd" => Some("glob"),
        _ => None,
    }
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

/// todowrite consecutive-identical ledger (session de530754: 16x
/// identical "Todos updated: 0/4", zero progress — the same re-plan shape
/// as stuck session 7614829e). Advisory on the 4th consecutive identical
/// result, hard block from the 8th. Consecutive-identical, NOT absolute
/// count: legit status flips (0/4 -> 1/4 -> 2/4 as subtasks complete, seen
/// live in session 265e3298) reset the streak and are never blocked —
/// identical output carries no new information by definition.
fn todowrite_nudge(streak: usize) -> Option<String> {
    if streak != 4 {
        return None;
    }
    Some("\n\nLoop guard: 4 identical todowrite results in a row — execute the list (read/grep/edit/bash) instead of re-planning it.".into())
}

fn todowrite_guard(name: &str, streak_before: usize) -> Option<String> {
    if !name.eq_ignore_ascii_case("todowrite") || streak_before < 7 {
        return None;
    }
    Some(format!(
        "todowrite blocked: {n} identical plan updates in a row with no new information — act on the existing list (or state the answer) instead of re-planning it.",
        n = streak_before + 1
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
        "unittest",
        "selftest",
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

/// Verify-after-fix tracking: does this bash command plausibly mutate the
/// workspace (so a green verify run from before it no longer proves the
/// tree)? Substring match on the lowered command. Interpreters only count
/// with a write signal (`open(`, `write`) so `python3 -c "print(1)"`
/// stays a read. A miss only costs the verify nudge, never correctness.
fn bash_looks_mutating(cmd: &str) -> bool {
    const SIGNALS: &[&str] = &[
        "sed -i",
        "sed --in-place",
        "perl -",
        "patch ",
        "git apply",
        "tee ",
        "cargo fix",
        "mv ",
        "cp ",
        "rm ",
        "mkdir ",
        "touch ",
        ">",
    ];
    let lower = cmd.to_lowercase();
    if SIGNALS.iter().any(|s| lower.contains(s)) {
        return true;
    }
    let interpreter = ["python", "node", "ruby", "php", "perl"].iter().any(|s| lower.contains(s));
    interpreter && (lower.contains("open(") || lower.contains("write"))
}

/// Ceiling tail: the turn hit the tool-round budget with work still
/// pending. Always leads with an explicit exhaustion notice (never a bare
/// echo of the last filler text) plus the resume pointers that apply, so
/// the transcript reads as interrupted, not done.
fn ceiling_tail(
    last_nonempty: &str,
    max_steps: usize,
    mutated: bool,
    verified: bool,
    work_requested: bool,
) -> String {
    let mut tail = if last_nonempty.trim().is_empty() {
        format!("Tool budget exhausted after {max_steps} tool rounds with work still pending. Say `continue` to resume: the next turn should list exactly what remains and the precise next tool calls (names + arguments).")
    } else {
        format!("Tool budget exhausted after {max_steps} tool rounds with work still pending. Last model text: {last_nonempty}\n\nSay `continue` to resume: the next turn should list exactly what remains and the precise next tool calls (names + arguments).")
    };
    if mutated && !verified {
        tail.push_str("\n\n");
        tail.push_str(UNVERIFIED_POINTER);
    }
    if !mutated && work_requested {
        tail.push_str("\n\n");
        tail.push_str(NO_PROGRESS_POINTER);
    }
    tail
}

/// No-progress gate input: does this request ask for a change? Substring
/// match on the lowered text. Deliberately excludes diagnostic verbs
/// (`check`, `chk`, `investigate`, `what changed`) — a read-only question
/// correctly ends with zero edits, and nagging it would burn rounds.
/// A false positive costs at most two extra rounds, then the turn releases.
fn looks_like_work(text: &str) -> bool {
    const HINTS: &[&str] = &[
        "fix", "bug", "broken", "doesn", "not work", "fail", "error",
        "implement", "repair", "refactor", "migrat", "push",
        "wire", "missing", "wrong", "add",
        // Mutation verbs the gate must not miss (session 265e3298:
        // "analyse and copy to lumina global config" ran 161 turns with
        // zero edits while the gate stayed disarmed — no hint matched).
        // Deliberately no "port" (matches report/support/import).
        // False positives cost at most 2 forced rounds, then release.
        "copy", "sync", "clone", "move",
    ];
    let lower = text.to_lowercase();
    HINTS.iter().any(|h| lower.contains(h))
}

/// Resume pointer carried by turns whose code edits never saw a passing
/// verification run — the next `continue` must verify until green.
const UNVERIFIED_POINTER: &str = "Note: this turn's edits were never verified (no passing test/build run) — say `continue` to verify: run the relevant tests/build and fix failures until green.";

/// Resume pointer for turns released by the no-progress gate: the request
/// asked for a change but the turn ends with zero edits, so say so
/// explicitly instead of reading as done (stuck session 9fd55938 ran 24
/// explore rounds then a filler final with the task untouched).
const NO_PROGRESS_POINTER: &str = "Note: no edit/write/patch ran this turn, so the requested change is NOT done — say `continue` to proceed: make the change, then verify it (run the relevant tests/build and fix failures until green).";




/// Scoped subagent run: fresh context with a builtin (general|explore) system
/// prompt, restricted tool specs, max 6 steps, no further delegation.
async fn run_subtask(
    router: &Router,
    cfg: &crate::config::Config,
    vault: &crate::vault::Vault,
    parent: &SecCtx,
    req: &crate::tools::SubagentReq,
    lsp: &Arc<crate::lsp::Manager>,
) -> Result<(String, crate::llm::Usage), AgentError> {
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
        crate::tools::truncate_to(&mut prompt, 6000);
        prompt.push_str("\n…[delegation prompt truncated at 6000 chars]");
    }
    let mut description = req.description.clone();
    if description.len() > 2000 {
        crate::tools::truncate_to(&mut description, 2000);
        description.push_str("…[truncated]");
    }
    let mut prompt_text = format!("You are the {} subagent. {}\n\nScope: use only the provided tools; you cannot launch subagents. When done, summarize findings with file references.", def.name, def.body);
    if !description.is_empty() {
        prompt_text.push_str("\n\nTask context: ");
        prompt_text.push_str(&description);
    }
    prompt_text.push_str(crate::vault::VAULT_GUIDANCE);
    prompt_text.push('\n');
    // Explore is read-only; general gets everything but subagent.
    let specs: Vec<ToolSpec> = Runner::tool_specs_filtered(cfg.websearch.disabled, cfg.lsp.disabled)
        .into_iter()
        .filter(|t| {
            if req.agent.eq_ignore_ascii_case("explore") {
                matches!(
                    t.name.as_str(),
                    "read" | "glob" | "grep" | "lsp" | "skill"
                        | "kb_query" | "webfetch" | "websearch" | "task_output"
                )
            } else {
                t.name != "subagent"
            }
        })
        .collect();
    let reg = Registry {
        kb_query: Some({
            let kb_override = cfg.kb.memory_dir.clone();
            std::sync::Arc::new(move |ws, q, b| {
                let ov = if kb_override.trim().is_empty() { None } else { Some(kb_override.as_str()) };
                crate::kb::query_in(ws, q, b, ov).map_err(|e| e.to_string())
            })
        }),
        lsp_query: Some({
            let lsp = lsp.clone();
            std::sync::Arc::new(move |ws, op, path, line, ch, filter| {
                lsp.query(ws, op, path, line, ch, filter).map_err(|e| e.to_string())
            })
        }),
        kb_memory_dir: cfg.kb.memory_dir.clone(),
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
        sandbox: crate::sandbox::Profile::from_config(&cfg.sandbox),
    };
    let mut messages = vec![
        llm::Message { role: "system".into(), content: prompt_text, thinking: String::new() },
        llm::Message { role: "user".into(), content: vault.substitute(&parent.session, &prompt).map_err(AgentError::Msg)?, thinking: String::new() },
    ];
    let mut last = String::new();
    let mut sub_usage = crate::llm::Usage::default();
    // Same guards as the parent loop, locally scoped: identical calls
    // hard-stop after 3 executions and re-reads share a ledger, so a
    // subagent cannot spin where the parent would be stopped.
    let mut seen: HashMap<String, usize> = HashMap::new();
    let mut read_counts: HashMap<String, usize> = HashMap::new();
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
        let u = handle.await.map_err(|e| AgentError::Msg(format!("subagent task: {e}")))??;
        sub_usage.add(&u);
        if calls.is_empty() {
            return vault.resolve(&parent.session, &last).map(|t| (t, sub_usage)).map_err(AgentError::Msg);
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
            let result = match loop_blocked(&tc.name, &args, &seen, &read_counts) {
                Some(msg) => Err(crate::tools::ToolError::Msg(msg)),
                None => reg.call(&sc, &tc.name, &args),
            };
            let ok = result.is_ok();
            let rendered = match result {
                Ok(o) => {
                    let mut out = o.output;
                    let mut note = String::new();
                    if o.truncated || out.len() > 4000 {
                        let total = out.len();
                        crate::tools::truncate_to(&mut out, 4000);
                        note = format!("\n[truncated {total} chars to 4000]");
                    }
                    format!("tool {} →\n{out}{note}", tc.name)
                }
                Err(e) => format!("tool {} failed: {e}", tc.name),
            };
            let key = tool_call_key(&tc.name, &args);
            if !key.is_empty() {
                *seen.entry(key).or_insert(0) += 1;
            }
            if tc.name.eq_ignore_ascii_case("read") {
                let target = crate::tools::tool_target(&tc.name, &args);
                if !target.is_empty() {
                    *read_counts.entry(target).or_insert(0) += 1;
                }
            }
            if ok && (tc.name.eq_ignore_ascii_case("edit") || tc.name.eq_ignore_ascii_case("write") || tc.name.eq_ignore_ascii_case("patch")) {
                seen.clear();
            }
            let sub = vault.substitute(&parent.session, &format!("[tool result {}]\n{rendered}", tc.name)).map_err(AgentError::Msg)?;
            messages.push(llm::Message { role: "user".into(), content: sub, thinking: String::new() });
        }
        let sub = vault.substitute(&parent.session, &last).map_err(AgentError::Msg)?;
        messages.push(llm::Message { role: "assistant".into(), content: sub, thinking: String::new() });
        last.clear();
    }
    vault.resolve(&parent.session, &last).map(|t| (t, sub_usage)).map_err(AgentError::Msg)
}

impl Runner {
    /// Rough token estimate (chars/4 over messages + tool specs).
    pub fn estimate_tokens(messages: &[llm::Message], specs: &[ToolSpec]) -> usize {
        let chars: usize = messages.iter().map(|m| m.content.len() + m.role.len()).sum::<usize>()
            + specs.iter().map(|s| s.name.len() + s.description.len() + s.schema.to_string().len()).sum::<usize>();
        chars / 4
    }

    /// Compact once the estimate crosses 60% of the advertised window (or
    /// 60 turns regardless): tool-result turns are 2KB each, so waiting for
    /// 80% leaves no headroom for the multi-round tool loop that follows.
    /// Opencode compacts incrementally on token budget, not turn count.
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
        if turns.len() < 12 {
            return Ok(());
        }
        let model = if sess.model.is_empty() { router.model_for("executor", "build") } else { sess.model.clone() };
        let window = router.context_window(&model).await as usize;
        // Unknown window: fall back to a conservative default instead of
        // never compacting (unbounded growth ends in a provider hard-reject
        // mid-turn).
        let window = if window == 0 { 64_000 } else { window };
        // Turn-count backstop: 60+ turns of tool spam compacts even under
        // the token threshold (each tool round replays the whole prefix).
        if turns.len() >= 60 {
            Self::compact_session(store, router, cfg, session_id).await?;
            return Ok(());
        }
        let msgs: Vec<llm::Message> = turns
            .iter()
            .map(|t| llm::Message { role: t.role.clone(), content: t.text.clone(), thinking: String::new() })
            .collect();
        let used = Self::estimate_tokens(&msgs, &Self::tool_specs());
        if used * 100 < window * 60 {
            return Ok(());
        }
        Self::compact_session(store, router, cfg, session_id).await?;
        Ok(())
    }

    /// Keep the first 2 + last 10 turns, summarize the middle via the fast
    /// tier (naive truncation note when no LLM is reachable), and persist a
    /// summary turn. Mirrors Go auto-compaction (rules/diffs/memory pointers
    /// are rebuilt from disk, never summarized away). The wider tail (10 vs
    /// 6) preserves the most recent tool results verbatim so the model does
    /// not re-read files it just saw.
    pub async fn compact_session(
        store: &Store,
        router: &Router,
        cfg: &crate::config::Config,
        session_id: &str,
    ) -> Result<String, AgentError> {
        let turns = store.turns(session_id)?;
        if turns.len() < 12 {
            return Ok("already compact".into());
        }
        let keep_head = 2.min(turns.len());
        let keep_tail = 10.min(turns.len() - keep_head);
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
                _ => format!("(offline fallback: LLM summarizer unreachable — below is the verbatim head of the {} omitted turns, not a model summary)\n{}", middle.len(), digest.chars().take(2000).collect::<String>()),
            }
        };
        let ids: Vec<String> = middle.iter().map(|t| t.id.clone()).collect();
        let summary = format!("Prior context summary ({} turns compacted):\n{summary}", ids.len());
        let n = store.compact_replace(session_id, &ids, &summary)?;
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
        let specs = Runner::tool_specs_filtered(true, false);
        let names: Vec<&str> = specs.iter().map(|s| s.name.as_str()).collect();
        assert!(!names.contains(&"websearch"), "{names:?}");
        assert!(names.contains(&"webfetch"), "{names:?}");
        let all_specs = Runner::tool_specs_filtered(false, false);
        let all: Vec<&str> = all_specs.iter().map(|s| s.name.as_str()).collect();
        assert!(all.contains(&"websearch"), "{all:?}");
    }

    #[test]
    fn tool_specs_filtered_hides_lsp() {
        let specs = Runner::tool_specs_filtered(false, true);
        let names: Vec<&str> = specs.iter().map(|s| s.name.as_str()).collect();
        assert!(!names.contains(&"lsp"), "{names:?}");
        assert!(names.contains(&"grep"), "{names:?}");
    }

    #[test]
    fn parallel_set_is_read_only() {
        for t in ["read", "glob", "grep", "lsp", "skill", "READ"] {
            assert!(is_parallel_tool(t), "{t}");
        }
        for t in ["bash", "edit", "write", "patch", "subagent", "task_output", "kb_query", "question", "webfetch", "mcp"] {
            assert!(!is_parallel_tool(t), "{t}");
        }
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
    fn subagent_key_canonicalizes_to_agent_and_paths() {
        let k1 = tool_call_key("subagent", &a(&[("agent", "general"), ("task", "Read src/server/sessions.js activityFor and report verbatim")]));
        let k2 = tool_call_key("subagent", &a(&[("agent", "general"), ("task", "Something totally different about src/server/sessions.js tail")]));
        assert_eq!(k1, k2, "same agent + same file must share a key or repeat detection never fires");
        let k3 = tool_call_key("subagent", &a(&[("agent", "general"), ("task", "Read src/public/app.js and report")]));
        assert_ne!(k1, k3, "different files differ");
    }

    #[test]
    fn mentioned_paths_extracts_file_tokens() {
        let ps = mentioned_paths("Read `src/server/sessions.js:1275` and src/public/app.js verbatim");
        assert_eq!(ps, vec!["src/public/app.js".to_string(), "src/server/sessions.js".to_string()]);
        assert!(mentioned_paths("just think about the fix, no files").is_empty());
        assert!(mentioned_paths("see https://example.com/docs for details").is_empty(), "URLs are not paths");
    }

    #[test]
    fn subagent_guard_blocks_proxy_read_and_budget() {
        let mut counts = HashMap::new();
        counts.insert("src/server/sessions.js".to_string(), 5usize);
        let blocked = subagent_guard(
            "subagent",
            &a(&[("task", "Read /home/herbert/ai-manager/src/server/sessions.js tail via subagent since direct reads are blocked")]),
            &counts,
            0,
        )
        .expect("proxy read of a capped file blocks");
        assert!(blocked.contains("already read 5 times"), "{blocked}");
        assert!(subagent_guard("subagent", &a(&[("task", "read src/other.js")]), &counts, 0).is_none());
        let over = subagent_guard("subagent", &a(&[("task", "explore the repo")]), &HashMap::new(), SUBAGENT_BUDGET)
            .expect("exhausted budget blocks");
        assert!(over.contains("budget exhausted"), "{over}");
        assert!(subagent_guard("read", &a(&[("path", "x")]), &counts, 99).is_none(), "only subagents are guarded");
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
    fn todowrite_budget_nudges_then_blocks() {
        assert!(todowrite_nudge(3).is_none());
        assert!(todowrite_nudge(4).expect("4th consecutive identical nudges").contains("instead of re-planning"));
        assert!(todowrite_nudge(5).is_none(), "fires once, not spam");
        assert!(todowrite_guard("todowrite", 6).is_none(), "7th identical still runs");
        let block = todowrite_guard("todowrite", 7).expect("8th consecutive identical blocks");
        assert!(block.contains("re-planning"), "{block}");
        assert!(todowrite_guard("read", 9).is_none(), "other tools unaffected");
    }

    #[test]
    fn text_key_ignores_case_and_spacing() {
        let a = text_key("Checking X — pulling Y.");
        let b = text_key("  checking   x — pulling  y. ");
        assert_eq!(a, b, "filler repeats must hash equal");
        assert_ne!(a, text_key("Checking X — pulling Z."), "new content must differ");
    }

    #[test]
    fn output_key_is_deterministic() {
        assert_eq!(output_key("tool glob →\n(no matches)"), output_key("tool glob →\n(no matches)"));
        assert_ne!(output_key("tool glob →\n(no matches)"), output_key("tool grep →\n(no matches)"));
    }

    #[test]
    fn bash_native_redirect_escalates_to_block() {
        // First occurrence always runs (advisory, never blocked).
        assert!(bash_native_nudge("ls").is_some());
        assert!(bash_native_blocked("ls", false).is_none());
        // Repeat — including arg variants that dodge the identical-call
        // key (`ls` vs `ls -la`) — is blocked with the native tool named.
        let msg = bash_native_blocked("ls", true).expect("2nd ls blocks");
        assert!(msg.contains("glob"), "{msg}");
        let msg = bash_native_blocked("ls -la /tmp", true).expect("arg variant blocks");
        assert!(msg.contains("glob"), "{msg}");
        assert!(bash_native_blocked("cat todo.py", true).expect("2nd cat blocks").contains("read"));
        // Verify/build commands are never redirected, before or after.
        assert!(bash_native_blocked("cargo test", true).is_none());
        assert!(bash_native_blocked("git diff --stat HEAD", true).is_none());
        assert!(bash_native_blocked("cargo test", false).is_none());
        // `pwd;`-prefixed chains are bare native calls in disguise.
        assert!(bash_native_nudge("pwd; ls -la").unwrap().contains("native `glob`"));
        assert!(bash_native_blocked("pwd; ls -la", true).expect("chained ls blocks").contains("glob"));
        // Piped grep still needs shell; verify compounds are never blocked.
        assert!(bash_native_nudge("git diff | grep foo").is_none());
        assert!(bash_native_blocked("cargo test && ls", true).is_none(), "verify compound must run");
    }

    #[test]
    fn bash_native_nudge_redirects_file_ops() {
        assert!(bash_native_nudge("grep -r foo src").unwrap().contains("native `grep`"));
        assert!(bash_native_nudge("cat src/main.rs").unwrap().contains("native `read`"));
        assert!(bash_native_nudge("ls src").unwrap().contains("native `glob`"));
        assert!(bash_native_nudge("git diff --stat HEAD").is_none(), "git still needs bash");
        assert!(bash_native_nudge("cargo test").is_none());
        assert!(bash_native_nudge("git diff | grep foo").is_none(), "piped grep still needs shell");
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
        assert!(is_verify_command("python3 -m unittest discover"));
        assert!(is_verify_command("python3 todo.py selftest"), "custom selftest entrypoint counts");
        assert!(is_verify_command("go test ./..."));
        assert!(!is_verify_command("ls -la"), "listing is not verification");
        assert!(!is_verify_command("cat tsconfig.json"), "tsconfig trap: no bare tsc");
        assert!(!is_verify_command("pnpm install"), "install is not verification");
        assert!(!is_verify_command("git diff --stat HEAD"), "diff is not verification");
        assert!(!is_verify_command("echo done"), "echo is not verification");
    }

    #[test]
    fn bash_mutation_detection() {
        // Session 7920b8fc: sed/python patches via bash must arm the gate.
        assert!(bash_looks_mutating("sed -i 's/a/b/' src/main.rs"));
        assert!(bash_looks_mutating("python3 - <<'EOF'\nopen('src/x.rs','w').write('hi')\nEOF"));
        assert!(bash_looks_mutating("patch -p1 < fix.diff"));
        assert!(bash_looks_mutating("echo hi > newfile.txt"), "redirection creates files");
        assert!(bash_looks_mutating("cargo fix --allow-dirty"));
        assert!(!bash_looks_mutating("ls -la"), "listing is not a mutation");
        assert!(!bash_looks_mutating("grep -r foo src"), "search is not a mutation");
        assert!(!bash_looks_mutating("cargo build"), "build output is not a source edit");
        assert!(!bash_looks_mutating("cargo clippy --tests"), "lint is not a mutation");
        assert!(!bash_looks_mutating("python3 -c \"print(1)\""), "pure compute is not a mutation");
        assert!(!bash_looks_mutating("cat src/main.rs"), "read is not a mutation");
    }

    #[test]
    fn ceiling_tail_never_reads_as_done() {
        // Session 7920b8fc: the ceiling echoed bare filler with no pointer.
        let t = ceiling_tail("Remaining lints mapped, applying the fixes.", 120, false, false, true);
        assert!(t.contains("Tool budget exhausted after 120"), "{t}");
        assert!(t.contains("Remaining lints mapped"), "last text kept for context, got {t}");
        assert!(t.contains("continue"), "{t}");
        assert!(t.contains("NOT done"), "zero-edit ceiling keeps the no-progress pointer, got {t}");
        let t = ceiling_tail("", 120, true, false, true);
        assert!(t.contains("Tool budget exhausted"), "{t}");
        assert!(t.contains("never verified"), "{t}");
        let t = ceiling_tail("filler", 120, true, true, true);
        assert!(t.contains("Tool budget exhausted"), "{t}");
        assert!(!t.contains("never verified"), "verified edits need no pointer, got {t}");
    }

    #[test]
    fn looks_like_work_matches_fix_requests_not_diagnostics() {
        assert!(looks_like_work("fix the orange glow"));
        assert!(looks_like_work("session doesnt turn orange, u can try"));
        assert!(looks_like_work("add a collapse toggle"));
        assert!(looks_like_work("push the branch"));
        assert!(looks_like_work("analyse and copy to lumina global config"), "copy requests are work");
        assert!(looks_like_work("sync the fork"));
        assert!(!looks_like_work("send me the weekly report"), "report must not match (no port hint)");
        assert!(!looks_like_work("chk the logs and see whats the issue"));
        assert!(!looks_like_work("what changed in the last commit"));
        assert!(!looks_like_work("check the weather"));
        assert!(!looks_like_work("are we finished?"));
        assert!(!looks_like_work("read a.txt"));
        assert!(!looks_like_work("say nothing"));
    }

    #[test]
    fn continue_status_classifies_unfinished_turns() {
        // Unverified edits always need another turn (both exits).
        assert_eq!(continue_status(true, false, true, false), (true, ContinueReason::NeedsVerify));
        assert_eq!(continue_status(true, false, false, true), (true, ContinueReason::NeedsVerify));
        // Zero-edit work requests need action (both exits).
        assert_eq!(continue_status(false, false, true, false), (true, ContinueReason::NeedsAction));
        assert_eq!(continue_status(false, false, true, true), (true, ContinueReason::NeedsAction));
        // Clean finish continues only when the budget gave out.
        assert_eq!(continue_status(false, false, false, true), (true, ContinueReason::BudgetExhausted));
        assert_eq!(continue_status(true, true, false, true), (true, ContinueReason::BudgetExhausted));
        assert_eq!(continue_status(false, false, false, false), (false, ContinueReason::None));
        assert_eq!(continue_status(true, true, false, false), (false, ContinueReason::None));
        // Synthetic prompts re-arm the work gate and forbid summaries.
        for r in [ContinueReason::NeedsVerify, ContinueReason::NeedsAction, ContinueReason::BudgetExhausted] {
            let p = r.auto_prompt(1, 3);
            assert!(p.contains("fix"), "prompt must carry a work hint, got {p}");
            assert!(p.contains("Do not end with"), "prompt must forbid summaries, got {p}");
        }
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
            .run_prompt(&store, &router, &cfg, &sess.id, &runner.begin_turn(&store, &sess.id, "read a.txt").unwrap(), "", &[], |_, _| {}, |_| {}, |_| {})
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
            .run_prompt(&store, &router, &cfg, &sess.id, &runner.begin_turn(&store, &sess.id, "say nothing").unwrap(), "", &[], |_, _| {}, |_| {}, |_| {})
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
            .run_prompt(&store, &router, &cfg, &sess.id, &runner.begin_turn(&store, &sess.id, "patch a.txt").unwrap(), "", &[], |_, _| {}, |_| {}, |_| {})
            .await
            .unwrap();
        assert_eq!(hits.load(Ordering::SeqCst), 5, "edit + 2 refused finishes + failing verify + release");
        assert_eq!(std::fs::read_to_string(dir.path().join("a.txt")).unwrap(), "beta", "edit must apply");
        assert!(out.text.contains("never verified"), "released turn must carry the pointer, got {out:?}");
        let turns = store.turns(&sess.id).unwrap();
        assert!(!turns.iter().any(|t| t.text.contains("Unverified changes")), "gate demand stays in-memory; transcript holds real turns only");
    }

    /// Scripted gateway serving fixed payloads in order (one POST per round).
    fn scripted_e2e(
        payloads: Vec<&'static str>,
    ) -> (Router, crate::config::Config, std::sync::Arc<AtomicUsize>) {
        let (router, cfg, hits, _) = scripted_e2e_capture(payloads);
        (router, cfg, hits)
    }

    /// Same, plus captured request bodies for asserting what the loop sent
    /// (e.g. tools:[] on forced answer rounds).
    fn scripted_e2e_capture(
        payloads: Vec<&'static str>,
    ) -> (Router, crate::config::Config, std::sync::Arc<AtomicUsize>, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = std::sync::Arc::new(AtomicUsize::new(0));
        let hits2 = hits.clone();
        let bodies = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let bodies2 = bodies.clone();
        std::thread::spawn(move || {
            for p in payloads {
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
                bodies2.lock().unwrap().push(String::from_utf8_lossy(&body).into_owned());
                hits2.fetch_add(1, Ordering::SeqCst);
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    p.len(),
                    p
                );
                let _ = s.write_all(resp.as_bytes());
            }
        });
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
        (router, cfg, hits, bodies)
    }

    /// No-progress gate: a fix request ending with zero edits forces action
    /// rounds (cap 2), then releases with a resume pointer — the turn must
    /// not read as done when nothing changed (stuck session 9fd55938).
    #[tokio::test]
    async fn no_progress_gate_forces_action_rounds_then_releases() {
        let read_call = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"1\",\"function\":{\"name\":\"read\",\"arguments\":\"{\\\"path\\\":\\\"a.txt\\\"}\"}}]}}]}\n\n";
        let (router, cfg, hits) = scripted_e2e(vec![
            read_call,
            "data: {\"choices\":[{\"delta\":{\"content\":\"looks broken, checking\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"still checking\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"blocked: need the css file\"}}]}\n\n",
        ]);
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "alpha").unwrap();
        let ws = dir.path().display().to_string();
        let db = dir.path().join("t.db").display().to_string();
        let store = Store::open(Some(Box::leak(db.into_boxed_str()))).unwrap();
        let sess = store.create_session(&ws, "build", "bypass", "").unwrap();
        let runner = Runner::new();
        let out = runner
            .run_prompt(&store, &router, &cfg, &sess.id, &runner.begin_turn(&store, &sess.id, "fix the orange glow").unwrap(), "", &[], |_, _| {}, |_| {}, |_| {})
            .await
            .unwrap();
        assert_eq!(hits.load(Ordering::SeqCst), 4, "explore + 2 refused finishes + release");
        assert!(out.text.contains("blocked: need the css file"), "released with the final text, got {out:?}");
        let turns = store.turns(&sess.id).unwrap();
        assert_eq!(
            turns.iter().filter(|t| t.text.contains("No changes made yet")).count(),
            0,
            "control-plane demands stay in-memory; transcript holds real turns only"
        );
    }

    /// No-progress release names the miss: the released text carries a
    /// resume pointer instead of reading as done (stuck session 9fd55938:
    /// 24 explore rounds, filler final, task untouched).
    #[tokio::test]
    async fn no_progress_release_carries_pointer() {
        let read_call = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"1\",\"function\":{\"name\":\"read\",\"arguments\":\"{\\\"path\\\":\\\"a.txt\\\"}\"}}]}}]}\n\n";
        let (router, cfg, hits) = scripted_e2e(vec![
            read_call,
            "data: {\"choices\":[{\"delta\":{\"content\":\"looks broken, checking\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"still checking\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"blocked: need the css file\"}}]}\n\n",
        ]);
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "alpha").unwrap();
        let ws = dir.path().display().to_string();
        let db = dir.path().join("t.db").display().to_string();
        let store = Store::open(Some(Box::leak(db.into_boxed_str()))).unwrap();
        let sess = store.create_session(&ws, "build", "bypass", "").unwrap();
        let runner = Runner::new();
        let out = runner
            .run_prompt(&store, &router, &cfg, &sess.id, &runner.begin_turn(&store, &sess.id, "fix the orange glow").unwrap(), "", &[], |_, _| {}, |_| {}, |_| {})
            .await
            .unwrap();
        assert_eq!(hits.load(Ordering::SeqCst), 4, "explore + 2 refused finishes + release");
        assert!(out.text.contains("need the css file"), "final text preserved, got {out:?}");
        assert!(out.text.contains("NOT done"), "released text must name the miss, got {out:?}");
    }

    /// Non-work requests (no keywords) end with text untouched — the gate
    /// must not nag read-only questions.
    #[tokio::test]
    async fn non_work_finish_passes_without_nag() {
        let (router, cfg, hits) = scripted_e2e(vec![
            "data: {\"choices\":[{\"delta\":{\"content\":\"sunny\"}}]}\n\n",
        ]);
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().display().to_string();
        let db = dir.path().join("t.db").display().to_string();
        let store = Store::open(Some(Box::leak(db.into_boxed_str()))).unwrap();
        let sess = store.create_session(&ws, "build", "bypass", "").unwrap();
        let runner = Runner::new();
        let out = runner
            .run_prompt(&store, &router, &cfg, &sess.id, &runner.begin_turn(&store, &sess.id, "check the weather").unwrap(), "", &[], |_, _| {}, |_| {}, |_| {})
            .await
            .unwrap();
        assert_eq!(hits.load(Ordering::SeqCst), 1, "no guard round trip for plain answers");
        assert_eq!(out.text, "sunny");
    }

    /// Task inheritance: a bare follow-up ("continue") keeps the
    /// no-progress gate armed via the session's original task (stuck
    /// session 7614829e "continue" turn: 124 tools, zero edits, silent).
    #[tokio::test]
    async fn followup_inherits_work_from_original_task() {
        let (router, cfg, hits) = scripted_e2e(vec![
            "data: {\"choices\":[{\"delta\":{\"content\":\"still fine\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"still fine\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"blocked: need the file\"}}]}\n\n",
        ]);
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "alpha").unwrap();
        let ws = dir.path().display().to_string();
        let db = dir.path().join("t.db").display().to_string();
        let store = Store::open(Some(Box::leak(db.into_boxed_str()))).unwrap();
        let sess = store.create_session(&ws, "build", "bypass", "").unwrap();
        store.add_turn(&sess.id, "user", "fix the orange glow").unwrap();
        let runner = Runner::new();
        let out = runner
            .run_prompt(&store, &router, &cfg, &sess.id, &runner.begin_turn(&store, &sess.id, "continue").unwrap(), "", &[], |_, _| {}, |_| {}, |_| {})
            .await
            .unwrap();
        assert_eq!(hits.load(Ordering::SeqCst), 3, "bare follow-up must still force action rounds");
        assert!(out.text.contains("blocked: need the file"), "released with the final text, got {out:?}");
        let turns = store.turns(&sess.id).unwrap();
        assert_eq!(
            turns.iter().filter(|t| t.text.contains("No changes made yet")).count(),
            0,
            "control-plane demands stay in-memory; transcript holds real turns only"
        );
    }

    /// Identical outputs force an answer round (session de530754: 8x glob
    /// "(no matches)", 2x identical kb dumps with varied args). Three
    /// identical tool results in a row strip tools for exactly one round;
    /// the 4th request must carry `"tools":[]` plus the Answer-round demand.
    #[tokio::test]
    async fn identical_outputs_force_answer_round() {
        fn echo(cmd: &str) -> &'static str {
            let body = serde_json::json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "1", "function": {"name": "bash", "arguments": serde_json::json!({"command": cmd}).to_string()}}]}}]});
            Box::leak(format!("data: {body}\n\n").into_boxed_str())
        }
        let (router, cfg, hits, bodies) = scripted_e2e_capture(vec![
            echo("echo same"),
            echo("echo  same"),
            echo("echo same #x"),
            "data: {\"choices\":[{\"delta\":{\"content\":\"all good\"}}]}\n\n",
        ]);
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().display().to_string();
        let db = dir.path().join("t.db").display().to_string();
        let store = Store::open(Some(Box::leak(db.into_boxed_str()))).unwrap();
        let sess = store.create_session(&ws, "build", "bypass", "").unwrap();
        let runner = Runner::new();
        let out = runner
            .run_prompt(&store, &router, &cfg, &sess.id, &runner.begin_turn(&store, &sess.id, "check the weather").unwrap(), "", &[], |_, _| {}, |_| {}, |_| {})
            .await
            .unwrap();
        assert_eq!(hits.load(Ordering::SeqCst), 4, "3 tool rounds + 1 forced answer round");
        assert!(out.text.contains("all good"), "forced round returns its text, got {out:?}");
        let bodies = bodies.lock().unwrap();
        // Empty tool lists are omitted from the wire body (not `"tools":[]`),
        // so assert the absence of tool definitions plus the demand text.
        assert!(!bodies[3].contains("\"name\":\"bash\""), "4th request must advertise no tools");
        assert!(bodies[3].contains("Answer round"), "4th request must carry the demand");
        assert!(bodies[0].contains("\"name\":\"bash\""), "earlier rounds keep tools");
        let turns = store.turns(&sess.id).unwrap();
        assert_eq!(turns.iter().filter(|t| t.text.starts_with("[tool result bash]")).count(), 3);
    }

    /// A non-compliant gateway can still emit tool calls on a tools-stripped
    /// round (Anthropic would 400 them as undeclared). The loop drops them:
    /// the 4th scripted call must never execute.
    #[tokio::test]
    async fn forced_round_drops_defiant_tool_calls() {
        fn echo(cmd: &str) -> &'static str {
            let body = serde_json::json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "1", "function": {"name": "bash", "arguments": serde_json::json!({"command": cmd}).to_string()}}]}}]});
            Box::leak(format!("data: {body}\n\n").into_boxed_str())
        }
        let (router, cfg, hits, _) = scripted_e2e_capture(vec![
            echo("echo same"),
            echo("echo  same"),
            echo("echo same #x"),
            echo("echo defiant"),
            "data: {\"choices\":[{\"delta\":{\"content\":\"done\"}}]}\n\n",
        ]);
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().display().to_string();
        let db = dir.path().join("t.db").display().to_string();
        let store = Store::open(Some(Box::leak(db.into_boxed_str()))).unwrap();
        let sess = store.create_session(&ws, "build", "bypass", "").unwrap();
        let runner = Runner::new();
        let out = runner
            .run_prompt(&store, &router, &cfg, &sess.id, &runner.begin_turn(&store, &sess.id, "check the weather").unwrap(), "", &[], |_, _| {}, |_| {}, |_| {})
            .await
            .unwrap();
        assert_eq!(hits.load(Ordering::SeqCst), 5, "3 tool rounds + held round + empty-final round");
        assert!(out.text.contains("done"), "turn completes, got {out:?}");
        let turns = store.turns(&sess.id).unwrap();
        assert_eq!(turns.iter().filter(|t| t.text.starts_with("[tool result bash]")).count(), 3, "defiant 4th call must not execute");
    }

    /// Verbatim filler text with continued tooling forces an answer round
    /// (session de530754: 5x + 4x identical status lines). Outputs differ
    /// here, so only the text streak can fire.
    #[tokio::test]
    async fn identical_filler_text_forces_answer_round() {
        fn status_call(n: u32) -> &'static str {
            let body = serde_json::json!({"choices": [{"delta": {"content": "checking status", "tool_calls": [{"index": 0, "id": n.to_string(), "function": {"name": "bash", "arguments": serde_json::json!({"command": format!("echo {n}")}).to_string()}}]}}]});
            Box::leak(format!("data: {body}\n\n").into_boxed_str())
        }
        let (router, cfg, hits, bodies) = scripted_e2e_capture(vec![
            status_call(1),
            status_call(2),
            status_call(3),
            "data: {\"choices\":[{\"delta\":{\"content\":\"done here\"}}]}\n\n",
        ]);
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().display().to_string();
        let db = dir.path().join("t.db").display().to_string();
        let store = Store::open(Some(Box::leak(db.into_boxed_str()))).unwrap();
        let sess = store.create_session(&ws, "build", "bypass", "").unwrap();
        let runner = Runner::new();
        let out = runner
            .run_prompt(&store, &router, &cfg, &sess.id, &runner.begin_turn(&store, &sess.id, "check the weather").unwrap(), "", &[], |_, _| {}, |_| {}, |_| {})
            .await
            .unwrap();
        assert_eq!(hits.load(Ordering::SeqCst), 4, "3 tool rounds + 1 forced answer round");
        assert!(out.text.contains("done here"), "forced round returns its text, got {out:?}");
        let bodies = bodies.lock().unwrap();
        assert!(!bodies[3].contains("\"name\":\"bash\""), "4th request must advertise no tools");
        assert!(bodies[3].contains("Answer round"), "4th request must carry the demand");
    }

    /// Idle-round nudge: 12 mutation-free rounds on a work request prod
    /// the model mid-loop (observable as an `idling…` stage); the turn
    /// still ends through the normal no-progress gate afterwards.
    #[tokio::test]
    async fn idle_rounds_nudge_then_gate_releases() {
        let mut payloads: Vec<String> = (0..12)
            .map(|i| {
                let body = serde_json::json!({
                    "choices": [{
                        "delta": {
                            "tool_calls": [{
                                "index": 0,
                                "id": i.to_string(),
                                "function": {
                                    "name": "read",
                                    "arguments": serde_json::json!({"path": format!("a{i:02}.txt")}).to_string()
                                }
                            }]
                        }
                    }]
                });
                format!("data: {body}\n\n")
            })
            .collect();
        // scripted_e2e takes 'static strs; leak the round payloads.
        let leaked: Vec<&'static str> = payloads
            .drain(..)
            .map(|s| Box::leak(s.into_boxed_str()) as &'static str)
            .collect();
        let mut all = leaked;
        all.push("data: {\"choices\":[{\"delta\":{\"content\":\"almost\"}}]}\n\n");
        all.push("data: {\"choices\":[{\"delta\":{\"content\":\"almost\"}}]}\n\n");
        all.push("data: {\"choices\":[{\"delta\":{\"content\":\"done-ish\"}}]}\n\n");
        let (router, cfg, hits) = scripted_e2e(all);
        let dir = tempfile::tempdir().unwrap();
        for i in 0..12 {
            // Distinct contents: identical outputs across varied reads are
            // the output-ledger's business (forced answer round), not this
    // test’s — keep reads distinct so only the idle path fires.
            std::fs::write(dir.path().join(format!("a{i:02}.txt")), format!("content {i}")).unwrap();
        }
        let ws = dir.path().display().to_string();
        let db = dir.path().join("t.db").display().to_string();
        let store = Store::open(Some(Box::leak(db.into_boxed_str()))).unwrap();
        let sess = store.create_session(&ws, "build", "bypass", "").unwrap();
        let runner = Runner::new();
        let stages = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let s2 = stages.clone();
        let out = runner
            .run_prompt(
                &store,
                &router,
                &cfg,
                &sess.id, &runner.begin_turn(&store, &sess.id, "fix the thing").unwrap(),
                "",
                &[],
                |_, _| {},
                move |st: &str| {
                    s2.lock().unwrap().push(st.to_string());
                },
                |_| {},
            )
            .await
            .unwrap();
        assert_eq!(hits.load(Ordering::SeqCst), 15, "12 reads + 2 refused finishes + release");
        assert!(
            stages.lock().unwrap().iter().any(|s| s.contains("idling")),
            "idle nudge must surface as a stage, got {:?}",
            stages.lock().unwrap()
        );
        assert!(out.text.contains("done-ish"), "released with the final text, got {out:?}");
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
        assert_eq!(turns.len(), 2 + 10 + 1, "kept head+tail+summary, got {}", turns.len());
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
