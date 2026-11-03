---
date_modified: 2026-11-03
tags: [efficiency, prompt-caching, usage-accounting, compaction, subagents]
related: [[conversation-efficiency]], [[context-from-providers]]
---

# Prefix-cache hit rates per session

Prefix-cache hit% per session from sessions.tokens_cache_read/input: long steady sessions converge 60-71%; each compact costs one full-price turn over rewritten history (4 compacts + 5 subagents -> 40% over 161 turns); subagents start ice-cold, prefer fewer/larger calls and main-turn iteration; new sessions pay cold prefix (54 turns -> 32%); mid-session router model swap resets cache namespace. Code already optimal: stable system prompt (agent.rs:150), append-only history, breakpoint on last user block (llm.rs:1041), standing-ops never touches messages (server.rs:828).

See also: [[conversation-efficiency]], [[context-from-providers]].
Telemetry landed: Usage::cache_hit_pct (src/llm.rs, clamped, tested) + tracing::info per turn.completed (src/server.rs, silent unless RUST_LOG=info) with session/model/input/output/cache_read/hit_pct. No behavior change.
