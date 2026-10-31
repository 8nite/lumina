---
date_modified: 2026-10-31
tags: [lumina, rust-port, kb, server, agent-loop]
---

# Rust port (M0–M6 complete, branch `rust-revamp`)

Single-binary Rust port alongside the Go daemon (`src/main.rs` `mod agent|config|kb|llm|rules|security|server|store|tools`). `make build` unchanged; verify with `CGO_ENABLED=0 go test ./...` plus `cargo test` for the Rust side.

- M0 scaffold: config (`src/config.rs`), plan/build fence + 4-mode broker + credential/Google hard-deny (`src/security.rs`: `is_credential_path`, `is_google_target`, `plan_allows_write`, layer-0 deny first in `decide`), rules discovery (`src/rules.rs`: `workspace_root`, `discover`, frontmatter parse, `builtin_agents`, `Bundle`), axum server skeleton (`src/server.rs`).
- M1 agent loop: BYO-LLM router (`src/llm.rs`: `Router`, `split_model`, `normalize_effort`/`valid_effort`/`effort_budget`, `ModelMeta`), session store (`src/store.rs`), tool registry (`src/tools.rs`: `read|edit|write|bash|grep|glob|kb_query|memory_write|skill`; `..` traversal denied, args aliases `path|file|target` and `pattern|query`), agent runner (`src/agent.rs` `Runner::run_prompt` with compat tuple).
- M2 KB + APIs: `src/kb.rs` mirrors `internal/kb` with zero vector DBs — native `graph.json` traversal (BFS/DFS/path/explain/god-nodes, <2ms), constrained-vocab expansion, recursive `[[wikilink]]` resolution (`wikilinks`, `resolve_wikilinks`), dedup-on-write `memory_write`, `manifest_stamp`, cold-start `reflect`, singleflight `Sweeper`. CLI: `lumina kb [query|update|reflect|status]` (`src/main.rs` `cmd_kb`, dual graph paths `memory/graphify-out/graph.json` + `graphify-out/graph.json`). Server: session CRUD/fork/export/revert, `files`, `git/diff|branch`, `providers|router|models`, `kb/query|browse`, compat flags `(import_global_rules, import_global_skills)` threaded through `AppState` and `run_prompt`.
- Ops note: hyperedge `nodes[]` coerced on load, confidence defaulted, degenerate <2-node edges shrink-guarded (`Graph::load`).
- M3 TUI: ratatui client (`src/tui.rs`) + blocking `sdk.rs`, approval/question modals via `ApprovalGate` (`src/approve.rs`: block_in_place + oneshot, session grants, state resync incl. pending modals), SSE broker in `AppState`, `Tab`/`Shift+Tab`, `/resume` `/model` pickers, `@file`/`[[memory]]` complete, steer/queue.
- M4 web parity: shell-over-websocket PTY (`src/pty.rs`, verified 101 + `echo` round-trip), Copilot device flow + 0600 token store (`src/copilot.rs`, `auth device` CLI + `/api/v1/auth/device/*`), `pair` CLI, multi-provider `websearch` + redirect-checked `webfetch` (`src/websearch.rs`), per-request `LUMINA_TOKEN` auth on non-loopback, `[websearch]` config validation, TMPDIR hygiene + startup sweep.
- M5 Harness²: `src/harness.rs` (8 components, additive guardrails, worktree `probe`, quality-floor `compose`, `refs/lumina/harness` commit, library promote, offline pass), `evolve` CLI + endpoint + TUI `/evolve`.
- M6 remainder: Copilot session-exchange + `openai-responses` SSE + vision `images` + `context_window` (advertised, cached) in `src/llm.rs`; MCP stdio/SSE client (`src/mcp.rs`, `mcp` tool, `/api/v1/mcp/tools`); Node v22 plugin bridge (`src/plugins.rs` + vendored `plugins_host.mjs`, before/after + permission.ask + shell.env + chat.message hooks, `plugin list|doctor`); `subagent` (general|explore, depth-guarded) in `agent.rs`; shadow-git checkpoints (`src/checkpoints.rs`, `/undo` `/redo` + endpoints + traces); auto + manual `compact`; `patch` (unified diff) + `todowrite` tools; plan read-only bash + bash glob rules + tooling-doc deny in `security.rs`.
- Verify: `cargo build` clean, `cargo test` 44 passed, `cargo clippy` only schema/API-surface warnings; Go M0 gate green (`config rules security server`); live matrix green (CRUD, router hot-swap + planner fallback, SSE deltas, state, PTY WS, harness evolve commit).
- Deliberate deferrals: background async subagents (`task_output` needs async tasks; Rust runs subagents synchronously), full interactive PTY resize (line-execution covers the web xterm).

See also: [[project]], [[lumina-build]], [[lumina-live]], [[vault-placeholders]], [[loops-and-claims]]
