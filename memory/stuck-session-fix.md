---
date_modified: 2026-11-01
tags: [reliability, sessions, sse, loop-guards]
---

# Stuck-session fix (56d41a74)

Last session ran 1028 turns (224 reads, 171 bash, ~299k context chars):
turn completed backend-side (interim + final assistant pair at 13:21:56,
claim released), but the agent re-paged the same files (SessionView.tsx
region) and the web spinner outlived the turn.

- Agent: same-file re-read evades the identical-call key (different
  offsets). Added `read_repeat_nudge` in `src/agent.rs`: 4th read of one
  path per turn tells the model to use grep/glob or batch, then act.
  Test: `agent::loop_key_tests::reread_nudge_fires_once_on_fourth_read`.
  Hardened: nudge is advisory (session had 15 identical-guard hits yet
  kept looping), so added `read_repeat_blocked` — 6th+ read of one path
  per turn never reaches the filesystem, returns a short error instead;
  edit/write/patch to that path resets the count (re-read after change is
  legitimate). Test: `reread_block_fires_on_sixth_read`.
- Daemon: SSE `broadcast::channel(256)` drops `turn.completed` under
  long-turn bursts → spinner forever. Bumped to 1024 in `src/server.rs`.
  Still lossy by design (`Lagged(_) => continue` skips silently, no
  resync while connected), so added a client watchdog in `web/src/App.tsx`:
  while `working`, poll `syncActiveSession` every 15s — it already stops
  the spinner + refetches history when `running==false`.
- Verify-after-fix gate (log evidence: 23 edit/write/patch vs 4 verify
  runs; post-13:09 edits never verified, `parseTool shape NOT FOUND -
  already patched or drifted` at 14:11): `src/agent.rs` tracks
  `mutated`/`verified` per turn — edit/write/patch sets mutated (clearing
  verification), only a bash verify command (`is_verify_command`: cargo
  test/clippy/build, tsc/vite, pnpm/npm build/test incl. `--dir` form,
  pytest, go/jest/vitest/eslint, make test/build/lint) whose output lacks
  the `[exit N]` failure marker counts. Ending the turn unverified forces
  up to 2 verification rounds, then releases with `UNVERIFIED_POINTER`
  (`say continue to verify…`) on both the normal finish and the
  max-steps tail. System prompt now states the rule outright. Tests:
  `verify_command_detection`,
  `unverified_edit_forces_verify_rounds_then_pointer` (5 scripted rounds:
  edit + 2 refused finishes + failing verify + release).
- v2 double-check (2026-11-01, opencode `dev` branch + issue #58062):
  `session-ui/timeline/detail.ts` compact preset (the default) is
  shell grouped+collapsed, edit grouped+collapsed, thinking
  grouped+collapsed; `current-tool-state.ts` `currentContentDefaultOpen`
  starts error cards collapsed too; plain reads render as one
  comma-separated row (`currentToolGroupedRead`); user `!` shells reuse
  the agent shell renderer and also collapse (by design, contested in
  #58062). Our `SessionView` matches: all rows start collapsed
  (incl. errors), reads group to "N reads", bash monospace one-liner,
  turn diff 5-file limit (`turnDiffFileLimit`). Intentional deviations:
  thinking always expanded sentence-per-line (explicit in-session user
  request), disclosure kept in row state not persisted.
- Timeline ordering (769e50d, 2026-11-01): persisted tool outputs reuse
  the `user` role (`[tool result …]`), so `isToolTurn` must be checked
  before `role === 'user'` in `SessionView` or they render as chat
  bubbles instead of collapsing into `ToolGroup`.
- Kept the session's own hardening (workspace `normalize_workspace`,
  SDK/client retry + `/state` check, 50s prompt cap → 202 + SSE, effort
  step-down, web resync). `cargo test` 115 pass, `cargo clippy --tests`
  clean, `pnpm --dir web build` green.
- Deploy note: daemon on `:4089` runs the installed
  `~/.local/bin/lumina`, not the repo tree — repo-side fixes need
  `make install` + daemon restart coordinated with ai-manager (do not
  kill `:4089` unilaterally).
- The `:4089` daemon is owned by `/home/herbert/ai-manager` (node
  `src/server/index.js`), not stray: killing it caused a ~2min
  ECONNREFUSED gap until the manager re-spawned it. Do not kill `:4089`
  again; coordinate restarts with the manager. Fixed port is
  `127.0.0.1:4089` (other apps bind `:7432`).

See [[long-session-fixes]].

## Second case (81bdb764, 2026-11-01): quit-early, opposite shape

20s turn, 11 explore calls (glob×3 same tree, bash ls×2, grep, read×2,
kb_query), zero edits, then the SAME filler twice (interim + final:
"Fixing the missing completion glow — testing it live.") and
`running:false`. Task untouched. Same model as 56d41a74
(`opencode_go/muse-spark-1.3-contributor`), so not a model difference —
the verify gate can't fire without mutations. Fix: no-progress gate in
`src/agent.rs` (`looks_like_work` keyword heuristic, exempts plan mode +
diagnostic verbs like chk/check/investigate; max 2 forced action rounds,
silent release — no pointer, no breakage risk without changes).
Tests: `looks_like_work_matches_fix_requests_not_diagnostics`,
`no_progress_gate_forces_action_rounds_then_releases`,
`non_work_finish_passes_without_nag`.

## Backend-continuation audit (browser closed → turn continues)

Verified in code, 2026-11-01: turn runs in a detached `tokio::spawn`
drain (`src/server.rs` prompt: spawn happens before any await, oneshot
result is `let _`-ignored) — client disconnect/abort cannot cancel it.
`emit` ignores send errors; SSE `events()` drop only ends that client's
stream (`Lagged => continue` is display-only). Approval/question waits
are bounded 600s (`src/approve.rs`) — Ask mode + closed browser errors
into the transcript, turn continues. Frontend `prompt()` has no
AbortController and already assumes server-side continuation (retry
checks `/state` first). Real gap found + fixed: drain-task panic
skipped `release_turn` + `running.remove` → every later prompt steered
into a dead drain (permanent wedge). Fix: `TurnRelease` Drop guard in
`src/server.rs` (test: `turn_release_guard_frees_claim_and_running`).
Noted, unchanged: daemon restart mid-turn → 409s until the 600s stale
claim window (per-process owner uuid, Go parity); transcript persists
per round so `continue` resumes.

## Third case (d3539f35, 2026-11-01): turn died mid-work, no trace

Same orange-glow task, new binary WITH the no-progress gate. Gate
fired correctly once (demand at 14:51:07, model explored 6 more tools),
then the transcript ends with an interim fluff row and `running:false`
— the round-10 model call failed (gateway/stream error) and drive_one's
Err path persisted nothing (turn.failed is SSE-only). Fix in
`src/server.rs` drive_one: one retry after 5s cool-down, then persist
`Turn failed (<err>). Say continue to resume.` as an assistant turn +
`tracing::warn!` (daemon stderr now kept at `.lumina/daemon.log`).
Tests: `drive_one_retries_transient_failure` (500 then green),
`drive_one_persists_failure_note` (500 always → 2 attempts + note).

## Fourth case (7614829e, 2026-11-01): same prompt submitted twice

Two exact user prompts at 15:02:27 and 15:04:53 with no final assistant
turn between them; `/state` still `running:true`. The other `user` rows
are persisted tool results, not repeats. The second submit arrived
during the active run and was steered into it (default
`delivery=steer`; same-owner `claim_turn` refreshes `inflight_since`).
Likely automatic source: `web/src/api/client.ts` `api.prompt()` retried
a 429/502/503/504 after an inconclusive `/state` probe and re-POSTed the
same text (same flaw in `src/sdk.rs` `submit_delivery`); transcript
cannot prove retry vs manual resend. Fix: request-id idempotency —
client/SDK mint one `request_id` per submit and reuse it across retries;
daemon keeps `{session, request_id}` 600s and answers a resubmit with the
live status (`steered`/`accepted` + `deduped:true`) without touching the
inbox, claim, or queue. `web/src/App.tsx` `send()` additionally
single-flights concurrent submits. Test:
`prompt_idempotency_dedups_within_ttl`.
7614829e duplicate-prompt fix landed (6ad0f21): request_id idempotency in daemon prompt (seen-key map 600s, deduped status), client and SDK reuse one key across retries, App send single-flights. Live-verified deduped:true on resubmit. Cargo 122 pass, clippy at baseline, web tsc+build green.

## Fifth case (7614829e, 2026-11-01): bare "continue" spun 124 tools, zero edits, no gate

Same orange-glow session: a follow-up `continue` prompt carries no intent
by itself, so `looks_like_work("continue")` is false and the no-progress
gate stayed disarmed while the model ran ~40 rounds (43 todowrite
re-plans, 66 explore bashes, SessionView re-reads) with zero edits and no
finish attempt for the end-gates to catch. Fix in `src/agent.rs`
(uncommitted): inherit the session's original task — first non-tool `user`
turn — for gate decisions (`work_requested` = current prompt OR first
task); plus an idle-round nudge — consecutive mutation-free rounds counter
(`edit`/`write`/`patch` resets; `task_output`-only polling rounds neither
reset nor advance), advisory prod every 12th idle round on work requests
(`idling…` stage). Tests:
`followup_inherits_work_from_original_task` (bare follow-up forces action
rounds, both demands hit transcript), `idle_rounds_nudge_then_gate_releases`
(12 reads surface `idling` stage, turn still ends via no-progress gate).

## Fifth case (7614829e "continue" turn, 2026-11-01): gates blind on follow-ups + todo/bash spin

"continue" turn ran 15:20:10→15:31:50 (~124 tools: 43 todowrite re-plans
stuck at 0/3, 66 explore bashes, 9 reads, 6 greps), zero edits, ended
with duplicated filler and no gate fired — the prompt text "continue"
matches no work keyword, so `work_requested` was false. Fixes in
`src/agent.rs`: (1) inherit the session's original task (first non-tool
user turn) for gate decisions, so bare follow-ups keep both end-gates
armed; (2) idle-round nudge — consecutive mutation-free rounds on a work
request append an advisory prod every 12th round (`idling…` stage;
task_output-only polls don't count). Tests:
`followup_inherits_work_from_original_task`,
`idle_rounds_nudge_then_gate_releases`.
