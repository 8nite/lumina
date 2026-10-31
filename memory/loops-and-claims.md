---
date_modified: 2026-10-31
tags: [agent-loop, loops, multi-daemon, sessions, lumina]
---

# Agent loops + cross-daemon turn claims

- Repeat ledger (`internal/agent/agent.go` `RunPrompt`): names any identical tool call on its 3rd execution. The round guard only caught 3 consecutive identical rounds; alternating read/todo spam slipped through. `task_output` polling stays exempt (re-polling to completion is progress).
- Cross-daemon turn claim (`sessions.inflight_owner`/`since`, 12 min stale takeover): a second daemon's prompt 409s with the holder identity instead of silently double-driving one session's transcript.
- Ops note: daemons must be restarted to pick up daemon-side fixes (old binaries lack `/state` and running flags).

See also: [[resync-variants]], [[long-session-fixes]], [[lumina-live]], [[project]]
