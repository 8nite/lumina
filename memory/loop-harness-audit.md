---
date_modified: 2026-11-01
tags: [lumina-memory]
---

Reviewed turn loop vs opencode/codex/claude-code; fixed 12 issues: atomic compaction (compact_replace txn), retry reuses one user turn (begin_turn), control-plane demands in-memory only, claim heartbeat, identical-call hard-stop (4th attempt) with clear-on-mutate, subagent loop guards (seen/read ledgers, 4KB truncation, no bash in explore), compact-vs-turn 409 guard, 64k default window, SSE seq ids, DB-backed idempotency+steer inbox, usage summing + subagent cost attribution.
