---
date_modified: 2026-11-01
tags: [web-ui, model-picker, session-model, router]
---

# WebUI model switch

- `web/src/App.tsx` `applyModel` now does `setSession(s)` with the patched session returned by `api.patchSession(session.id, {model: id})` — previously discarded the result, so the header button stayed stale until the next state sync. Still calls `api.setRouter('executor', id)` + `refreshModels()`.
- Verified in browser on a live session (`iceman` → `muse-spark-1.3`, incl. spark filter): `session.model` + `router.executor` persist.

See also: [[model-picker-filter]], [[resync-variants]], [[opencode-zen-plugin]]
