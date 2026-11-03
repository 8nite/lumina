---
date_modified: 2026-11-03
tags: [efficiency, loop-guards, over-exploration, regression]
related: [[conversation-efficiency]], [[loops-and-claims]]
---

# Session de530754: over-complication on a read-only audit

Prompt: "help to check if lumina are using the same rule, plugins and skill
as in opencode". Result: CORRECT answer (loader proof with file:line
refs), but 105 turns / 69 tool calls / 1.23M input tokens for a question
needing ~8 reads. Breakdown of waste:

- 16x identical `todowrite` ("Todos updated: 0/3 completed") — re-planning
  with zero progress (same shape as stuck session 7614829e: 43 re-plans).
- 8x `glob` → "(no matches)" with varied patterns — dodges the
  identical-call key (name+args), same empty result every time.
- 2x identical kb_query MEMORY.md dumps, 14 reads, 7 bashes.
- 5x + 4x verbatim assistant filler ("Comparing Lumina vs opencode
  configs — pulling rules, plugins, and skills.") — invisible to all
  guards, which only tracked tool-call keys.

Root cause: every loop guard keyed on identical CALLS. Varied args with
identical results, varied filler with continued tooling, and unbounded
re-planning all slipped through.

## Fixes (src/agent.rs, all unit + e2e tested, 181 green)

- Identical-OUTPUT ledger: hash of final rendered result per call
  (task_output exempt); 3rd repeat appends a loop-guard note and arms a
  tools-stripped answer round. Catches varied-args/same-result classes
  (glob-misses, kb dumps, todowrite spam).
- todowrite budget: advisory 4th, hard block 6th+ (`todowrite_guard` in
  the pre-execution chain — the only guard that can prevent, not just
  react, since the signal exists pre-execution).
- Identical-TEXT streak: same assistant text 3 rounds with continued
  tooling arms a forced answer round. Max 2 forced rounds/turn, then
  release; non-compliant tool calls on a stripped round are dropped
  (Anthropic would 400 them as undeclared).
- New ledgers reset on edit/write/patch (prior outputs stale post-mutation),
  mirroring `seen.clear()`.
- Tests: pure-unit (guard thresholds, key normalization) + 3 e2e via
  scripted gateway with request-body capture (force on identical outputs,
  defiant-call dropping, force on identical filler). Caught + fixed one
  real regression this way: idle-rounds test used 12 identical 1-line
  files (now distinct — identical outputs ARE the output-ledger's case).

Note: the `Answer round` demand is in-memory only (like other control-plane
nudges); request bodies omit `"tools"` when empty (OpenAI adapter skips
empty vec) so tests assert absence of tool definitions + demand text.

Addendum (session 265e3298): the first todowrite-guard version counted
absolute calls per turn (nudge 4th, block 6th). That session's 29
todowrite calls included real 0/4 -> 2/4 progress, which an absolute
block would have broken. Refined to consecutive-identical streaks
(nudge 4th, block 8th): identical output carries no new information by
definition, while status flips reset the streak and are never blocked.
Same session also showed the guards never fired (0 hits) because the
serving daemon runs bin/lumina built 21:08, before the guards landed in
target/debug (21:39). Lesson: fixes go live only after `make build`
AND daemon restart — never restart mid-turn (strands the inflight
claim). Cache check on 265e3298: tokens_cache_read 274,764 / input
682,753 = ~40% served from cache (vs 61% in de530754 — variance is
normal). cache_write 0 is expected: the zen gateway (OpenAI-Responses)
uses implicit prefix caching with no creation counter.

Addendum 2 (session 265e3298 re-check): the turn was still live and the
same waste continued (29 todowrite, 11x identical at the end, filler
repeats) — because the serving daemon ran bin/lumina from 21:08, before
the guards landed (21:39). Lesson learned the hard way: fixes only go
live after `make build` AND daemon restart; verified the daemon binary
lags the repo by checking guard strings in the binary. Also fixed a
no-progress-gate blind spot the session exposed: "analyse and copy ..."
matched no work hint (161 turns, zero edits, gate disarmed) — added
copy/sync/clone/move (not port: matches report/support/import).
Daemon restarted 22:20 UTC with the fresh binary after confirming no
inflight turn; health-checked via openapi.json (200) + headless run.
