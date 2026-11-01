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
- Verify: `cargo build` clean, `cargo test` 78 passed (unit + live-protocol simulators + fake-server SSE/MCP/HTTP fixtures), `cargo clippy --tests` only schema/API-surface warnings; full `go test ./...` green; live matrix green (CRUD, router hot-swap + planner fallback, SSE deltas, state, PTY WS, harness evolve commit).
- Live-model hardening (real Spark backend): `.done`-only fallbacks for `output_text`/`function_call_arguments`/`output_item` (gateways batch text/args without preceding deltas); pre-turn shadow-git snapshots (post-turn snapshots made `/undo` a no-op); skip empty assistant echoes (self-echoed "I'll query..." text looped the model into 16-turn retries); replay skips the just-stored user turn by id (`src/agent.rs`, else the model sees its own prompt twice); empty-final guard with pure-empty passthrough (no guard trip when zero tool calls); 4KB tool-result cap into context; shared reqwest client; 600s per-request stream timeout (fails loud instead of hanging forever); cross-daemon 409 + per-session server-side queue with background drain (verified 3 queued turns drain in order, 6/6 transcript); disconnect-proof prompt drain (`src/server.rs`: first turn + queue drain in a detached task, result back over oneshot — client disconnect no longer cancels the in-flight turn or leaks `running`/claim); per-turn `kb.refreshed` standing-ops; `turn.context` gauge + ms/chars on `turn.completed`.
- E2E suite (`tests/e2e-live.sh`, 18 sections, live Spark backend): sessions CRUD/fork/export/state, exact reply, tool turn, approval ask→allow_once→completed, question ask→answer→completed (retried prompt + quoted tool-result echo), shadow-git undo/redo content restore, 20-turn compact→9, kb_query answered from graph-less memory (MEMORY.md [[links]] + filename fallback), SSE fanout to 2 subscribers, PTY WS echo, providers/models/router hot-swap + config-file persist round-trip, harness evolve commit + library promote, plugin `tool.execute.before` block under bypass, vision attach description, SPA + CSP, TUI pty boot, queue 202 + background drain of all 3 turns. Result: **47 passed, 0 failed**.
- Deliberate deferrals: background async subagents (`task_output` needs async tasks; Rust runs subagents synchronously), full interactive PTY resize (line-execution covers the web xterm).
- Parity pass (`ee2c159`): cross-daemon 409 via `Store::claim_turn`/`release_turn` + `daemon_owner` (600s slot); custom primaries (`agents/*.md` `mode: primary`, Tab-cycle, `/api/v1/agents`, plan|build-or-primary validation); slash commands (`/api/v1/commands`, `$ARGUMENTS` expansion, `Sdk::commands|agents|auth_start|auth_poll`); `turn.context` gauge via `emit_context` (used vs advertised window); `/connect` wizard (provider → device code → tick poll).
- Harden pass (`2ead19e`): `persist_router_model` hot-swap + config-file survive-restart; per-turn `standing_ops` (`cfg_path`, `KbSweeper`, `refresh_memory_manifest`, `kb.standing_ops` flag); shared `server::openapi_spec` (`GET /openapi.json` + CLI `openapi`); TUI thinking toggle (`thinking_shown`, `/thinking`, status line).

See also: [[project]], [[lumina-build]], [[lumina-live]], [[vault-placeholders]], [[loops-and-claims]]

## Parity audit + close-out (2026-10-31)

Audit (`internal/*` vs `src/*`, routes, tools, hooks, transports) found the
Rust port at functional parity or beyond (superset: `compact`, `state`,
`mcp/tools`, `commands`, `agents` routes) except:

- Fixed: `src/agent.rs` unicode escape (`\u2014` → `\u{2014}`) broke `cargo test`.
- Ported session secret vault (`src/vault.rs`, 13 detectors + date guards +
  stable per-session `{placeholder_xxx}` refs, byte-identical guidance):
  `Store.vault` (Arc-shared, memory-only), substitution on history/user/tool
  results/subagent/compact-digest paths, resolution on assistant/tool-arg
  paths, `lumina vault check` CLI. Fail-closed. 7 tests.
- Ported background tasks: `bash background:true` (detached thread) +
  `subagent background:true` (tokio::spawn) + `task_output` poll tool
  (`task-N` ids, daemon-lifetime registry). Tool-spec descriptions updated.
- Ported loop guards: per-call 3rd-identical nudge + identical-round streak
  nudge (`tool_call_key` exempts `task_output`; `tool_label` prefixes
  targets). 2 tests.
- Ported question dismissal: `ApprovalGate::dismiss`, `POST
  /api/v1/questions/{id}` `{"dismissed": true}` → "question dismissed by
  user" on the waiting turn, `Sdk::dismiss_question`, openapi spec line.
- Build flip: `make build`/`test`/`dev`/`lint`/`openapi-gen` now drive Rust
  (`bin/lumina` = Rust binary; `cargo` on PATH in Makefile); Go via
  `build-go`/`test-go`/`lint-go`/`openapi-gen-go`. README + AGENTS.md updated.
- Verify: `cargo test` 89 passed, `cargo clippy --tests` 22 warnings
  (pre-existing surface only), `CGO_ENABLED=0 go test ./...` green, live
  daemon smoke (session CRUD, dismiss-404, state, commands) clean.
- Remaining deliberate deferrals: full interactive PTY resize (line-execution
  covers web xterm); background subagents run synchronously-in-task (no
  `task_output`-for-subagent streaming — poll only).
- Serve close-out (`src/server.rs` `spa_fallback` + `spa_mime`): embedded SPA
  assets served with real MIME types (hashed `assets/*` immutable, entry docs
  `no-cache`); unknown paths fall back to `index.html` for client routing.
  Serving `index.html` for `/assets/*.js` broke `<script type=module>`
  loading. CSP widened to `default-src 'self'; connect-src 'self' ws:;
  style-src 'self' 'unsafe-inline'; font-src 'self'` so inline styles and the
  `ws:` PTY terminal survive (bare `default-src 'self'` killed both).
- Serve banner (`src/main.rs`): boot prints resolved `host:port` + absolute DB
  path (`LUMINA_DB` or `cwd/.lumina/lumina.db`, `eprintln` so it shows without
  `RUST_LOG`). A restart from another directory silently switches databases —
  the banner makes the wrong-cwd visible instead of surfacing as web 404s.
  Related: [[long-session-fixes]] session-gone recovery.
- Full tool-round transcript persist (`src/agent.rs` `run_prompt`): every
  interim assistant turn and every `[tool result …]` user turn is stored as it
  streams, so transcripts/`/resume` replay show all rounds; continuation
  nudges stay scaffolding (re-added, never persisted); guard prompt + canned
  resume pointer also persisted. Fixes blind `continue` from 0-char tails.

## Deep-audit round 2 (2026-10-31)

Three explorer subagents diffed Go vs Rust module-by-module; verified each
claim by reading both sides, then ported in priority order:

- LLM transport: Anthropic `x-api-key` (was Bearer — real break), `/v1/models`
  path, vision on every user turn, `max_tokens` floor under thinking,
  per-block tool emission + `message_stop` termination; Ollama `images` +
  `think:true`; Copilot chat headers (`Accept/Openai-Intent/X-Initiator`);
  SSE multi-line `data:` join + `finish_reason` flush; effort helpers
  (`effort_index`, `efforts_or_default`, `clamp_effort`).
- Copilot session: store-token fallback on exchange 404 (Go parity),
  `drop_copilot_session` wired into device-poll success (no restart needed).
- Agent loop: `on_stage` callback → `turn.progress` SSE (tool/steer/queue
  visibility), headless `›/→` stderr log, `turn.completed.text` + TUI repair,
  subagent upgrade (custom agents, model override + provider check, prompt
  caps, description, explore tool allowlist, `LUMINA_SUBAGENCY_DEPTH`,
  sessionID-followup rejection), loop/call keys unchanged.
- Tools: `*** Begin Patch ***` engine (`src/begin_patch.rs`: add/update/
  delete/move, fuzzy seek, heredoc, per-file gates, partial-progress errors)
  alongside unified diff; `patchText` keys; external read/write allowlists
  (`expand_home`, temp/system/home, credential override); `do_read` line
  numbers + dir listing + binary reject; `do_grep` path/literal/case/include;
  `do_glob` path base + limit + credential filter; `do_edit` replaceAll +
  oldString keys; `bash` workdir + timeout aliases + uncapped background;
  `webfetch` timeout aliases; question schema (`{question,header,multiple,
  options:[{label,description}]}` + bare strings, `[][]string` answers,
  unwired guidance, dismiss-as-guidance error); todowrite strictness + Go
  summary; AfterCall full args+output; websearch Exa/Firecrawl/Tavily-answer/
  Parallel/Tinyfish-429 shapes.
- Server/TUI/CLI: `DELETE /api/v1/sessions/{id}` (+`session delete`, store
  method + test — supersedes Go's stub error), `run --format json`
  `session_id` key, `auth device` alias, `serve --cors` accept,
  `turn.progress` classify + cross-session filter + missed-start recovery,
  `question.resolved`/`permission.resolved` modal clearing, Esc→dismiss API,
  `/resume`+`/continue` skip current, `/variants`, bare `/thinking` toggle,
  TUI `-c` fallback + session id on exit, `/model` session persist,
  SSE-reconnect resync + transcript rebuild, resync QModal shape.
- Verify: `cargo test` 96 passed, clippy 16 (all pre-existing surface),
  `CGO_ENABLED=0 go test ./...` green, live daemon smoke (CRUD, DELETE,
  dismiss-404, state) clean.
- Still deferred: steer-vs-queue protocol (queue covers it), TUI variant/
  model/connect pickers + transcript scroll keys + autocomplete descriptions,
  `OnBackgroundDone` push (poll covers it), PTY resize; Go-side concurrent
  edits (variant matrix, session delete) tracked as they land.

## Deferred-fixes round 3 (2026-10-31)

- Steer/queue protocol (was web-visible breakage: web client already sent
  `delivery` but the server ignored it): store steer inbox, agent drains it
  each round (stored raw, sent vaulted, `steered ✓` stage), server honors
  `delivery` (steer default; 202 `{status,session_id[,position]}` + progress
  events), leftover steer chains as queued, `turn.started` carries text,
  idle effort 400-validation, SDK `submit_delivery`, TUI Enter-steers /
  Ctrl+X-arms-queue (local queue removed), headless run unchanged.
  e2e S17 updated to steer+queue shapes (7-turn drain).
- TUI pickers: `/variant` popup at current rung (● marker), `/model
  provider/model` direct set + session persist, `/connect` expiry-aware
  deadline (`expires_in` plumbed through device-start), provider picker
  skipped (server only supports copilot).
- Scroll: Up/Down 3-line scroll, Home/End, half-page PgUp/PgDn, Esc snaps to
  tail, scroll banner (`… N↑ · pgdn/end to follow …`), viewport memory,
  load_transcript rebuild semantics.
- Background-done push: daemon-lifetime hook (server installs SSE
  `turn.progress "background done: id (kind)"`; headless prints to stderr);
  session-scoped task records; 2 tests.
- PTY: verified Go is also line-execution (no resize protocol either side;
  web xterm sends none) — parity confirmed by live WS round-trip, not a gap.
- Verify: `cargo test` 98 passed, clippy 16 (pre-existing only),
  `go test ./...` 14/14, `make build` clean, e2e-live.sh S17 updated.
- Still open (polish, no protocol impact): TUI model-list typeahead filter,
  input history, autocomplete descriptions, `init` scaffold depth, steer
  delivery on `lumina run -c` chaining (run is single-turn).
