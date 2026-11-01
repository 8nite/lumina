---
date_modified: 2026-11-01
tags: [plugin, models, opencode, zen, router]
---

# `models.list` hook + `plugins/opencode-zen.mjs`

- New plugin hook `models.list` (`src/plugins_host.mjs` context `models`, `src/server.rs` `plugin_models` + `merge_models`): plugins replace/enrich the core `/models` list per provider. Upsert-by-`id` (plugin wins on metadata), then sort by id. Silent/missing plugin keeps the core list. Global host only (`Cache::global`, `~/.config/lumina/plugins`), 10s budget on the blocking pool.
- Core `src/llm.rs` `openai_models` now parses **only** generic OpenAI/OpenRouter metadata (`context_length`, `supported_efforts`); opencode-specific schema was removed from core — it belongs in the plugin. Test renamed to `openai_models_parses_generic_window_and_efforts`.
- `plugins/opencode-zen.mjs` (bundled in-repo, install via `cp` to `~/.config/lumina/plugins/`): for opencode providers (name match or `opencode.ai` base URL) fetches the Zen catalog (`https://opencode.ai/zen/v1/models`, override `OPENCODE_ZEN_MODELS_URL`) using the provider's own key (`api_key_env`/`api_key_cmd` — never a pasted secret). Same key works for Go + Zen per opencode docs.
- Merge semantics: Go id ↔ Zen twin matched via shared `canonical_id` (e.g. `muse-spark-1.3-contributor` ↔ `muse-spark-1.3` share `meta/muse-spark-1.3`); Go id keeps its id but gains the twin's `limit.context` → `context_window` and `capabilities.reasoning.effort` → `supported_efforts`. Zen-only ids are added; core-only ids kept; failures return `{}` so the core list stands.
- Rust tests: `opencode_zen_plugin_merges_zen_catalog` (twin enrichment via `canonical_id`, zen-only add, id sort), `opencode_zen_plugin_ignores_other_providers` (copilot passthrough). Live: 57 models, spark filter shows go + zen ids.
- Documented in `plan.md` plugin-hook table (`models.list` row).

See also: [[opencode-zen-models]], [[model-picker-filter]], [[model-switch-ui]], [[context-from-providers]]
