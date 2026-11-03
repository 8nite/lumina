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

Note: the ` Answer round` demand is in-memory only (like other control-plane
nudges); request bodies omit `"tools"` when empty (OpenAI adapter skips
empty vec) so tests assert absence of tool definitions + demand text.
