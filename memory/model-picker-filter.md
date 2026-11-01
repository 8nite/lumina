---
date_modified: 2026-11-01
tags: [web-ui, model-picker, session-model, composer]
---

# Composer model picker + session model label

- Filter: `web/src/components/Composer.tsx` filters `modelOptions` by the custom input (case-insensitive substring); typing `spark` surfaces `opencode_go/muse-spark-1.3-contributor`. Verified live in browser on ephemeral daemon.
- Default label: `web/src/App.tsx` falls back to `session?.model || homeModel || modelOptions[0] || 'loading…'` instead of the literal `"model"` on a new session (ponytail: show executor default).
- Apply fix (uncommitted 2026-11-01): `applyModel` now `setSession(s)` with the patched session after `api.patchSession(session.id, {model: id})`, so the header label updates immediately instead of going stale until the next state sync. Still calls `api.setRouter('executor', id)` + `refreshModels()`.

See also: [[opencode-zen-models]], [[resync-variants]], [[verbose-streaming]], [[model-switch-ui]]
