---
date_modified: 2026-10-31
tags: [reliability, sessions, sse, permissions, context, tui, web]
---

# Long-Session Hardening

- Per-session prompt single-flight: steer/queue while a turn runs, no concurrent `Exec`; `SQLITE_BUSY` avoided with single-connection pool + `busy_timeout=5000` + bounded write retry.
- Terminal SSE (`turn.completed`/`failed`) never shed; web + CLI SSE auto-reconnect with backoff; `working` clears on terminal event.
- Web permission modal answers `permission.asked` (`allow_once`/`session`/`deny`) with no CLI attached.
- Live context gauge (`turn.context`, `~chars/4` vs provider-advertised window, used-only when unknown) in TopBar/working panel/TUI header/turn footers; `RunPrompt` compacts newest-first to 80% of window and pre-flights over-window turns.
- TUI space key types literal space instead of triggering completion/navigation.
- Turn timeout 10m→30m (`internal/server/server.go` `runTurn`): 120-round auto-continue turns on slow gateways were cut mid-task and forced a manual `continue`.
- Web review panel default-on (`web/src/App.tsx`): missing `lumina.reviewOpen` key means open, only explicit `'0'` stays closed; persisted on toggle.
- Web session-gone recovery (`web/src/App.tsx` `syncActiveSession`): double-404 (state + export) means the daemon restarted with another DB/cwd, so the spinner stops with `session not found on daemon — ... start a new session` instead of `working…` forever; on-screen transcript is the last synced copy.
- Web model picker refresh-on-open (`Composer`/`HomeView`/`SessionView` `onOpenModels` → `refreshModels`): opening the model dropdown re-fetches `/models` so a just-added provider key shows without reload.
- See [[test-plan]] TP-04.7/08.7/08.8/09.5/10.4. Related: [[verbose-streaming]], [[subagents-websearch]], [[resync-variants]].
