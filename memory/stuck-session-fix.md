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
