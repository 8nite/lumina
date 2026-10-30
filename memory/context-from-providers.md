---
date_modified: 2026-10-30
tags: [llm, providers, context-window, router, testing]
---

# Provider-Sourced Context Windows

- Context window now comes from providers, never hardcoded: `ModelMeta.context_window` parsed from gateway `/models` (`context_length`/`context_window`), Ollama `/api/show` (`model_info` `*.context_length`), Copilot `/models`; `Router.ContextWindow` with per-provider caching and 15s caps, unknown renders as used-only gauge. Mock test servers now route `/models` separately from chat rounds.
- Consumed by the live context gauge in [[long-session-fixes]] (`turn.context`, `~chars/4` vs provider window). Related: [[lumina-live]], [[project]].
