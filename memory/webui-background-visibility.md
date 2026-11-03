---
date_modified: 2026-11-03
tags: [web-ui, sidebar, sse, background-turns]
---

# WebUI background-turn visibility

Session `3bbace61` finished its turn while the user looked elsewhere and
nothing ever said so. Two gaps, both fixed:

- Sidebar had no per-session working indicator. `SessionView` now carries
  `running: bool`, served by `list_sessions` / `get_session` from the
  daemon's live turn set (`src/server.rs`); the sidebar renders a pulsing
  dot from `s.running` (`web/src/components/Sidebar.tsx`). Verified live:
  list returns `running: True` mid-turn, `False` when idle.
- Nothing announced background completions: `turn.completed` was only
  handled for the active session, and no toast/title/badge existed
  anywhere. `App.tsx` now marks sessions running/idle on
  `turn.started/completed/failed` for every session and raises a clickable
  toast + tab title flash (`✓ turn done`) for non-visible sessions
  (auto-dismiss 15s, consumed on open).

Scope note: ephemeral `bash background:true` jobs still surface only as
live-step capsules in the open session — no persistent jobs panel yet.

See [[web-ui-sticky-scroll]], [[stuck-session-fix]], [[session-resume-workspace]].
