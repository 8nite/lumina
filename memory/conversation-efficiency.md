---
date_modified: 2026-11-02
tags: [efficiency, prompt-caching, compaction, usage-accounting, agent-loop]
related: [[loops-and-claims]], [[long-session-fixes]], [[context-from-providers]]
---

# Conversation-log efficiency fixes (opencode comparison)

Compared `~/.local/share/opencode/opencode.db` (4.2GB, 3860 sessions,
345M input / 5.3B cache-read tokens = 94% cache hits, $204) against
`.lumina/lumina.db` (12MB). Opencode wins API cost via prompt caching and
per-session accounting; Lumina won storage via 4KB caps. Fixed Lumina to
keep both advantages:

- **Prompt caching** (`src/llm.rs`): Anthropic `cache_control: ephemeral`
  on system + last-user + tools breakpoints; parse
  `cache_creation/read_input_tokens` (Anthropic), `cached_tokens` variants
  (OpenAI-compat/Responses). `Usage` gains `cache_read/cache_write` with
  `add()`; usage accumulation switched from per-frame `saturating_add`
  (double-counted repeated totals) to `max`.
- **Usage persistence** (`src/store.rs`, `src/server.rs`): `sessions` gains
  `tokens_input/output/cache_read/cache_write` (auto-migrated), `add_usage()`
  per turn, `usage` trace rows, `turn.completed` SSE carries
  `cache_read/write_tokens`, session API exposes totals.
- **Nudge tax** (`src/agent.rs`): per-round "Continue the task..." user
  message (~50-100 tok x up to 120 rounds) now sent only when needed
  (8-round checkpoint, repeat streak >= 3, 12-round idle/verify prod, or
  steered input). Keeps the provider prefix stable for cache hits.
- **Thinking write-only** (`src/agent.rs`): reasoning segments now go to
  `thinking` traces, not `thinking` turns (replay only reads
  user/assistant, so thinking turns were never re-sent yet inflated
  compaction/estimates).
- **Compaction earlier** (`src/agent.rs`): trigger at 60% window (was 80%),
  min 12 turns (was 20), 60-turn backstop regardless of estimate; tail
  keeps 10 turns (was 6) so fresh tool results survive verbatim.
- **Tool-full spill** (`src/agent.rs`): truncated outputs archive full text
  (16KB) to `tool_full` traces; replay stays at the 4KB cap.
- **Parallel reads** (`src/agent.rs`): pure `read/glob/grep/lsp/skill`
  batches run on scoped threads; mixed batches stay sequential. Ledger and
  render stay sequential in call order.

Verification: `cargo test` 147 passed; `cargo clippy --tests` shows only
pre-existing warnings. `make build` web step untouched (additive JSON
fields only).

## Smoke test (2026-11-02, `opencode_go/muse-spark-1.3-contributor`)

Ran headless `lumina run --bypass` in `/tmp/opencode/lumina-smoke1`:
"make a small Python CLI todo manager with selftest, verify green".
Result: app built and green (`todo.py`, `selftest: all green`, exit 0),
turn cost persisted (`tokens_input=428563, output=12961, cache_read=200637`
— ~47% prefix-cache hit inside one turn). Issues found:

- **Verify-gate false negative (fixed):** `python3 todo.py selftest` ran
  green twice but `is_verify_command` didn't recognize it, so the turn
  carried a spurious "never verified" pointer next to the model's own
  "Verified green". Fixed by adding `unittest` + `selftest` to the VERIFY
  list (`src/agent.rs:1200`) with regression asserts. `do_bash` only
  stamps `[exit N]` on failure, so the exit-marker check itself is sound.
- Model burned ~4 rounds on bare `ls` before the identical-call guard
  stopped it (bash-native nudge fired but was ignored twice) — guards
  held, loop completed.
- Housekeeping: model deleted a stray `stderr.log` from the workspace
  during cleanup — keep harness capture files OUTSIDE the task workspace.
- `kb_query` correctly no-op'd once ("no knowledge base configured").
- Pre-existing repo `memory/*.md` churn (16:40) was NOT from this test
  (ran 17:08+, own `.lumina` in scratch) — no workspace-attribution leak.

## Fixes applied + re-test (same prompt, fresh dirs)

- **Verify gate** (`unittest`/`selftest`): smoke2 confirmed fixed — green
  `selftest` recognized, no spurious "never verified" pointer.
- **Bash-native redirect escalation** (`src/agent.rs`): first native-
  redirectable bash runs with advisory; repeats blocked outright
  (arg-variant-proof, unlike the identical-call key). Plus segment-aware
  classifier (`native_segment`): `pwd; ls -la` chains now redirect;
  piped commands (`git diff | grep`) and verify compounds
  (`cargo test && ls`) exempt via `is_verify_command`.
- **No-KB repeat** (`src/kb.rs`): skip note now says do NOT call kb_query
  again this turn.
- Smoke3 results (same task): **136k input / 6k output / 54k cached**
  vs 336k/19k/169k (smoke2) vs 429k/13k/201k (smoke1) — ~60% fewer input
  tokens, ~11 rounds vs ~22-24. Live evidence: 2x `bash redirected`
  blocks (both `pwd; ls` arg variants), model switched to `glob` x3;
  kb_query fired once; `tool_full` spillover wrote 2 traces; selftest
  `SELFTEST OK` confirmed independently, demo `todos.json` removed.

