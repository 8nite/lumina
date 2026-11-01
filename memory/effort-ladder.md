---
date_modified: 2026-11-01
tags: [models, router, effort, web-ui, tui]
---

# Canonical effort ladder (de-hardcoded)

- Union ladder `none < minimal < low < medium < high < xhigh < max` (`src/llm.rs`): `normalize_effort`/`valid_effort`/`effort_label`/`effort_index` (`none`→0 … `max`→6, `""`/`auto`→-1) and `effort_budget` (`minimal`→1024 API floor, `none`→0 thinking omitted) all handle the full vocabulary; server 400 message updated to `auto|none|minimal|low|medium|high|xhigh|max`.
- `efforts_or_default` passes advertised tokens through verbatim (deduped, never filtered to a hardcoded subset), so provider vocabularies like `minimal`/`none` survive to pickers and wire; empty advertisement falls back to the 8-step global ladder. `effort_step_down` extended to the full ladder (`low`→`minimal`→`none`→`""` default).
- Pickers are per-model: web `Composer.tsx` uses the advertised ladder for the selected model (`modelEfforts` from plugin-enriched `/models`, global `EFFORTS` fallback); TUI `EFFORTS` is 8 items (`auto` + 7 rungs).

See also: [[effort-stepdown]], [[resync-variants]], [[opencode-zen-plugin]], [[model-picker-filter]]
