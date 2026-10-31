//! Agentic turn loop: prompt → stream → tool calls → repeat.
//! Mirrors `internal/agent` `RunPrompt` (M1: core loop, no subagents/shadow-git;
/// those stay behind their own milestones).

use crate::{
    approve::ApprovalGate,
    llm::{self, Chunk, Request, Router, ToolSpec},
    rules::Bundle,
    security::{Agent, PermissionMode},
    store::Store,
    tools::{Registry, SecCtx},
};
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
}

impl Runner {
    pub fn new() -> Self {
        Self { max_steps: 16, approval: None }
    }

    fn tool_specs() -> Vec<ToolSpec> {
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
            ToolSpec { name: "glob".into(), description: "List workspace paths by glob.".into(), schema: obj(&["pattern"]) },
            ToolSpec { name: "kb_query".into(), description: "Query Markdown+graphify memory before grepping code. Prefer for durable knowledge.".into(), schema: obj(&["query", "budget"]) },
            ToolSpec { name: "memory_write".into(), description: "Save a durable fact to ./memory/<topic>.md (deduped, secrets rejected).".into(), schema: obj(&["topic", "content"]) },
            ToolSpec { name: "skill".into(), description: "Load a full SKILL.md body by name (only name+description are advertised).".into(), schema: obj(&["name"]) },
            ToolSpec { name: "question".into(), description: "Ask the user a clarifying question with options (interactive sessions only).".into(), schema: obj(&["questions"]) },
            ToolSpec { name: "websearch".into(), description: "Search the web for current info (needs a provider key; pick with [websearch] provider).".into(), schema: obj(&["query", "provider"]) },
            ToolSpec { name: "webfetch".into(), description: "Fetch one HTTP(S) URL as markdown/text/HTML (cap 100KB).".into(), schema: obj(&["url", "format"]) },
        ]
    }

    fn system_prompt(bundle: &Bundle, agent: Agent) -> String {
        let mut s = bundle.system_prompt.clone();
        s.push_str("\nYou are Lumina, a local-first coding agent. Answer with tool calls when action helps; otherwise reply directly.\n");
        if agent == Agent::Plan {
            s.push_str("PLAN MODE: research and blueprint only. Never edit source files; scoped writes to plan.md or .lumina/plans/*.md only.\n");
        }
        s.push_str("Tool guidance: prefer kb_query over grep for durable knowledge; batch independent reads; quote file:line references.\n");
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
        store.add_turn(session_id, "user", text)?;

        let mut messages = vec![llm::Message {
            role: "system".into(),
            content: Self::system_prompt(&bundle, agent),
            thinking: String::new(),
        }];
        for t in store.turns(session_id)? {
            // Replay persisted history (skip the user turn just stored — re-added below).
            if t.role == "user" || t.role == "assistant" {
                messages.push(llm::Message { role: t.role, content: t.text, thinking: String::new() });
            }
        }
        messages.push(llm::Message { role: "user".into(), content: text.into(), thinking: String::new() });

        let reg = Registry {
            kb_query: Some(std::sync::Arc::new(|ws, q, b| {
                crate::kb::query(ws, q, b).map_err(|e| e.to_string())
            })),
            bundle: Some(bundle),
            websearch_provider: cfg.websearch.provider.clone(),
        };
        let (runner_headless, runner_gate) = match &self.approval {
            Some(g) => (false, g.clone()),
            None => (true, ApprovalGate::default()),
        };
        let sc = SecCtx { workspace: workspace.clone(), session: sess.id.clone(), agent, mode, headless: runner_headless, gate: runner_gate };
        let agent_name = sess.agent.clone();
        let model = sess.model.clone();
        let final_text = self
            .loop_until_done(router, &reg, &sc, &agent_name, &model, effort, &mut messages, &mut on_event)
            .await?;
        store.add_turn(session_id, "assistant", &final_text)?;
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
        messages: &mut Vec<llm::Message>,
        on_event: &mut (dyn FnMut(&str, &str) + Send),
    ) -> Result<String, AgentError> {
        let mut last_text = String::new();
        for _ in 0..self.max_steps {
            let (tx, mut rx) = mpsc::unbounded_channel::<Chunk>();
            let req = Request {
                model: model.into(),
                messages: messages.clone(),
                tools: Self::tool_specs(),
                max_tokens: 0,
                reasoning_effort: effort.into(),
            };
            let role = if agent_name == "plan" { "planner" } else { "executor" };
            // Spawn the stream pump (Router is Clone + 'static); drain below.
            let router2 = router.clone();
            let role2 = role.to_string();
            let agent2 = agent_name.to_string();
            let handle = tokio::spawn(async move { router2.chat_stream(&role2, &agent2, req, tx).await });
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
                return Ok(last_text);
            }
            // Execute requested tools, feed results back as user messages.
            for tc in &calls {
                let args: HashMap<String, serde_json::Value> = tc.args.clone();
                let result = reg.call(sc, &tc.name, &args);
                let rendered = match result {
                    Ok(o) => format!("tool {} →\n{}{}", tc.name, o.output, if o.truncated { "\n[truncated]" } else { "" }),
                    Err(e) => format!("tool {} failed: {e}", tc.name),
                };
                on_event("", &rendered);
                messages.push(llm::Message { role: "user".into(), content: format!("[tool result {}]\n{rendered}", tc.name), thinking: String::new() });
            }
            messages.push(llm::Message { role: "assistant".into(), content: last_text.clone(), thinking: String::new() });
            last_text.clear();
        }
        Ok(last_text)
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


