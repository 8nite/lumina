---
date_modified: 2026-11-03
tags: [plugin, models, opencode, zen, router]
---

# Zen twin suppression (endpoint-bound ids)

- Zen/Go catalog twins (same `canonical_id`) are endpoint-bound: each id 400s on the other endpoint.
- Plugin lists the enriched Go id, suppresses the unusable Zen twin; Zen-only ids translate and stay listed. Live: 55 models, spark filter shows only servable ids.
- Copilot usable-filter (c652b72, 2026-11-03): `list_models` hides unusable models in three layers — structural filter drops responses-only endpoints (`copilot_chat_compatible`), a live probe classifies chat usability per model with a fail-open verdict (`classify_copilot_probe`), and the cached probe result hides dead models on later calls. Tests: structural-first, probe-finds-usable, cached-hides-dead.

See also: [[opencode-zen-plugin]], [[model-picker-filter]], [[opencode-zen-models]]
