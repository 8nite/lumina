---
date_modified: 2026-10-30
tags: [reliability, sessions, sse, permissions, context, tui, web]
---

# Long-Session Hardening

- Per-session prompt single-flight: steer/queue while a turn runs, no concurrent `Exec`; `SQLITE_BUSY` avoided with single-connection pool + `busy_timeout=5000` + bounded write retry.
- Terminal SSE (`turn.completed`/`failed`) never shed; web + CLI SSE auto-reconnect with backoff; `working` clears on terminal event.
- Web permission modal answers `permission.asked` (`allow_once`/`session`/`deny`) with no CLI attached.
- Live context gauge (`turn.context`, `~chars/4` vs provider-advertised window, used-only when unknown) in TopBar/working panel/TUI header/turn footers; `RunPrompt` compacts newest-first to 80% of window and pre-flights over-window turns.
- TUI space key types literal space instead of triggering completion/navigation.
- See [[test-plan]] TP-04.7/08.7/08.8/09.5/10.4. Related: [[verbose-streaming]], [[subagents-websearch]].
