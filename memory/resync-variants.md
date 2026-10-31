---
date_modified: 2026-10-31
tags: [sessions, resync, reconnect, variants, effort, tui, sdk, lumina]
---

# Session resync + effort variants + loop guard

- Resync endpoint (`internal/server/server.go`): `GET /api/v1/sessions/{id}/state` returns one atomic payload (session + `running` + pending permission/question) for a (re)loaded client that missed SSE. `pendingForSession` re-renders modals missed while disconnected (else a blocked turn looks like stuck `working…` with no way to answer); `isRunning` recovers the spinner after a missed `turn.started`, transcript refetch covers a missed `turn.completed`. `PATCH sessions/{id}` updates agent/permission/model/title. `/continue` picks the latest previous session via SQLite `ORDER BY updated_at DESC` even without a store attached.
- TUI resync (`internal/tui/model.go`): `RequestResyncMsg` → `resyncCmd` → `ResyncMsg` → `applyResync`; `transcriptToLines` rebuilds the timeline. `Init` loads authoritative history on `-c`/`-s` attach; `/resume` picker drops the current session; `closeEmptyPickers` (a failed fetch must not leave an empty picker). Missed-started recovery: a progress row for an apparently-idle session (or a delta for one) rebuilds live state. Question modal is cyan, approval modal yellow; `toolGroup`/`toolLineStyle`/`statusStyle` color timeline groups.
- Effort variants (`internal/llm/llm.go`): `EffortVariants` (UI order), `NormalizeEffort` (`""`/`auto` → `""`), `ValidEffort`, `EffortLabel` (`""` renders `auto`), `EffortBudget` (Anthropic thinking budget in tokens). TUI `/variant` picker (`variantIndex`/`applyVariant`, `Effort` field, `variantPill`). Covered by `variant_matrix_test` + `effort_matrix_test` (TUI) and `effort_matrix_test` (server).
- Agent loop guard (`internal/agent/agent.go`): softCap 8 / hardCap 40 rounds; an identical tool round (same calls + args, via `roundCallKey`/`toolCallKey`) 3× in a row injects a nudge to change approach instead of re-calling — `task_output` polling is exempt (re-polling to completion is progress). Model in-between text is persisted before its tool turns; `toolLabel` renders call summaries.
- SDK (`pkg/sdk/sdk.go`): `GetSession` (includes `running`), `SessionState` (resync payload), `ExportTranscript`/`TranscriptTurn` (full persisted timeline).
- Tests added: `variant_matrix_test`, `continue_tool_test`, `resync_test` (TUI); `effort_matrix_test`, `resync_test`, `sessions_list_test` (server).
- See also: [[long-session-fixes]], [[verbose-streaming]], [[prompts-parity]], [[lumina-live]], [[project]]
