---
date_modified: 2026-11-03
tags: [web-ui, model-picker, session-model, composer]
---

# Composer model picker + session model label

- Filter: `web/src/components/Composer.tsx` filters `modelOptions` by the custom input (case-insensitive substring); typing `spark` surfaces `opencode_go/muse-spark-1.3-contributor`. Verified live in browser on ephemeral daemon.
- Default label: `web/src/App.tsx` falls back to `session?.model || homeModel || modelOptions[0] || 'loading…'` instead of the literal `"model"` on a new session (ponytail: show executor default).
- Apply fix (uncommitted 2026-11-01): `applyModel` now `setSession(s)` with the patched session after `api.patchSession(session.id, {model: id})`, so the header label updates immediately instead of going stale until the next state sync. Still calls `api.setRouter('executor', id)` + `refreshModels()`.
- TUI availability filter (1ec51c4, 2026-11-03): `/model` lists only providers with at least one live model (`live_models`, `filter_live_providers`) and never shows an empty model list (`model_picker_never_shows_empty_models`); unavailable providers are hidden, not errored.
- TUI enterprise connect (1ec51c4, 2026-11-03): `/connect` prompts for the enterprise base URL (`connect_prompts_for_enterprise_url`), empty input means the default host, submit continues the device flow against the given URL.

See also: [[opencode-zen-models]], [[resync-variants]], [[verbose-streaming]], [[model-switch-ui]]
