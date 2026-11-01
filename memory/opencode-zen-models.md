---
date_modified: 2026-11-01
tags: [models, opencode, zen, router]
---

# Opencode zen/go `/models` schema (historical)

- Zen schema: context window at `limit.context`, reasoning ladder at `capabilities.reasoning.effort` (not `context_length`/`supported_efforts`). Original live capture: 28 models, `muse-spark-1.3` ctx=1048576 efforts=`[minimal,low,medium,high,xhigh]`.
- **Superseded 2026-11-01:** core `src/llm.rs` `openai_models` no longer parses this schema — it keeps only the generic OpenAI/OpenRouter parse so core stays provider-neutral. All zen enrichment moved to the `models.list` plugin hook; see [[opencode-zen-plugin]] for the current path. The schema facts above are still what the plugin maps (`limit.context` → `context_window`, `capabilities.reasoning.effort` → `supported_efforts`, matched via shared `canonical_id`).

See also: [[opencode-zen-plugin]], [[model-picker-filter]], [[context-from-providers]]
