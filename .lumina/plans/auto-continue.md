# Auto-continue until green — BUILT (2026-11-04, Always mode)

## As built (differs from draft in one place)
- Signal lives in `src/agent.rs`, not the server: `TurnOutcome` gained
  `needs_continue: bool` + `continue_reason: ContinueReason`
  (`None|NeedsVerify|NeedsAction|BudgetExhausted`), classified by pure
  `continue_status(mutated, verified, work_requested, budget_exhausted)`
  at all three turn exits (normal, empty-final, ceiling).
- Loop runs in `src/server.rs:drive_one` (daemon/web/TUI) AND
  `src/main.rs` headless `run`, so both benefit from the single signal.
  Each auto turn persists a synthetic `auto-continue n/cap: fix: …`
  user turn (re-arms the work gate, forbids summary endings).
- Caps: `[agent] auto_continue=true`, `max_auto_continues=3`
  (`src/config.rs:AgentConfig`); per-iteration breaks on stop flag,
  identical consecutive tails, context 85%+ (`context_full`).
- Tests: `continue_status` truth table, 2 `drive_one` loop tests with a
  route-aware `chat_gateway` (GET /models answered uncounted so the
  window lookup can't eat a chat payload), `context_full` unit test.
  `cargo test` 203 pass, `cargo clippy --tests` clean.

## Current stops (verified in code)
- `src/agent.rs:475` — `for round in 0..self.max_steps` (`max_steps=120`, `src/agent.rs:104`).
- `src/agent.rs:571-572` verify gate: `mutated && !verified && verify_rounds < 2`.
- `src/agent.rs:585-586` no-progress gate: `!mutated && work_requested && progress_rounds < 2`.
- `src/agent.rs:492` forced-answer: `forced_answers < 2` — "tool use is disabled for exactly this round" (`src/agent.rs:496-498`).
- `src/agent.rs:1518-1532` `ceiling_tail` ends turn with "Say `continue` to resume".
- `src/server.rs:757-766` `drive_turn`: one `run_prompt` + one retry on transient error, then return. No auto-loop.

So today green requires the user to type `continue` manually, up to 120 rounds per turn.

## Proposal: bounded auto-continue in `drive_turn`
1. **Outcome signal** — change `run_prompt` return from `(String, Usage)` to include `needs_continue: bool + reason` (`NeedsVerify` | `NeedsAction` | `None`). Derive from existing flags (`mutated && !verified`, `!mutated && work_requested` at ceiling), not by parsing tail text.
2. **Loop** — in `src/server.rs:drive_turn`, after `Ok(outcome)`: while `outcome.needs_continue && auto_turns < cap && !stop_requested`: emit `on_stage("auto-continuing n/cap…")`, re-invoke `run_prompt` with synthetic user message `continue: <UNVERIFIED_POINTER|NO_PROGRESS_POINTER>`.
3. **Config** — `auto_continue: bool` (default ON for all unfinished turns per user choice) + `max_auto_continues: u8` (default 3 → max ~360 rounds). Per-request override `q.auto`.
4. **Carry-over fix** — new turn re-derives `work_requested` from text; `looks_like_work` (`src/agent.rs:1539`) does not match "verify/test" so the synthetic continue message must include a hint word (`fix`/`verify+fix`) or pass an explicit `work_requested=true` param. Ledgers (`seen`, `out_counts`) restart per turn by design — fine, workspace files persist.
5. **Safety caps (load-bearing)** — break on: cooperative `stop_requested`, context >85%, two identical consecutive tails (loop), or any turn ending clean (`needs_continue=None`). Never raise `max_steps` itself — cap total turns instead.
6. **Tests** — `ceiling_tail` classification test + `drive_turn` loop-cap test (fake runner returning `NeedsVerify` forever → stops at cap) + existing `cargo test` green.

## Why not unbounded
Stuck-session history (`cc686408` proxy-read dodge, `de530754` identical-output loops, `7920b8fc` unverified edits) is why the 2-round gates + 120 ceiling exist. Auto-continue without a turn cap re-opens infinite-loop / cost blowout. Cap at 3 auto-turns.

## To implement (build mode)
- `src/agent.rs`: return structured outcome; thread `work_requested` override for synthetic continues.
- `src/server.rs`: auto-loop in `drive_turn` + config plumbing.
- `cargo test`, `cargo clippy --tests`, `pnpm --dir web build`.
