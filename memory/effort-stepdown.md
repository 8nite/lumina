---
date_modified: 2026-11-01
tags: [models, router, effort, zen, reliability]
---

# Effort step-down on 400 rejections

- `chat_responses`/`chat_openai` retry down the effort ladder on effort-rejection 400s (e.g. `iceman` rejects `max`) instead of failing the turn with 502.
- Helpers in `src/llm.rs`: `effort_step_down` (max→xhigh→high→medium→low→"" default) + `is_effort_error` (400 body blames `reasoning_effort`); ladder stays a preference order, never asserted blindly.
- Tests: `effort_step_down_walks_to_default`, `effort_error_detects_rejection`, `effort_rejection_steps_down_and_recovers` (mock 400 then recover). Verified live `iceman`+max → OK.

See also: [[zen-twin-suppression]], [[opencode-zen-models]], [[resync-variants]]
