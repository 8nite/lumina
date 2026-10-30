---
date_modified: 2026-10-30
tags: [tui, web, sse, streaming, observability, thinking, turn-summary]
---

# Verbose Run Output

- **Behavior:** `session.thinking`, `session.tps`, `session.grouping`, `mini.*` config keys; `run` has **no** `--verbose` — prints the final reply only; `Session.Message` parts with `ToolState` Running/Streaming/Completed/Error plus separate `Reasoning` parts. See [[subagents-websearch]] for the tools producing this activity.
- **Daemon (`internal/agent`, wired in `cmd/lumina/main.go:runServe`):**
  - `Runner.OnDelta` fires per model chunk via `completeStream` (used by `RunPrompt` + `runChildLoop`, so subagent children stream too); published as SSE `turn.delta {delta, thinking}`.
  - `Runner.OnToolStart/OnToolDone` bracket each executed call through `execTool` (approval waits excluded; 300-rune single-line preview + `duration_ms` + `error` flag); published as `turn.tool {phase, tool, target, preview?, error?, duration_ms?}`. All `callWithApproval` paths route through `execTool`.
  - `Runner.OnTurnEnd` reports `TurnStats{Model, DurationMs, InputTokens, OutputTokens}` (tokens only when the provider reports `Usage`; tok/s omitted, never estimated). `Server.NoteTurnEnd` stashes it; the prompt goroutine merges it into `turn.completed {text, model, duration_ms, input_tokens, output_tokens}`.
  - `lumina run` intentionally has **no** `--verbose`.
- **TUI (`internal/tui/model.go`):**
  - Thinking follows `session.thinking`: **hidden by default**, streams into `PendingThinking` but renders only after `/thinking show` (`/thinking hide`, bare `/thinking` toggles; listed in `/` help).
  - `turn.tool` appends grouped step lines (`· \`tool\` target` → `· \`tool\` target → preview|error`); foreign-session events filtered.
  - `turn.completed` appends the final markdown plus a turn footer (`agent · model · 2.5s · 40 tok/s`; empty when the event carries no summary). Tests in `internal/tui/stream_test.go`.
- **Web (`web/src/App.tsx`, `SessionView.tsx`):** live block while working — stage, tool-step pills (amber = running, red = error, hover = target + preview, `duration_ms` on done), streaming text, thinking in collapsed `<details>` (hidden by default). On `turn.completed` the transcript refetches (keeps `tool`/`tool_error` roles so pills persist) and the last Response header shows `· model · 2.5s · 40 tok/s`.
- **Tests:** `internal/agent/stream_test.go` (chunk forwarding, usage return, tok/s, `Summary()`, inbox FIFO, steer-consumption into transcript), `internal/server` queue-chain + summary-merge tests, TUI footer/thinking/queue-toggle tests. Full `CGO_ENABLED=0 go test ./...` green; `pnpm --dir web test && build` green.
- **Inbox while working (steer/queue):** `Runner.Steer` (joins the active turn at the next round boundary, recorded in-transcript) and `Runner.Queue` (chains after completion; leftovers auto-chain). Server holds one in-flight turn per session (`turn.progress` announces `steered:`/`queued #N`); `POST prompt` takes `delivery: steer|queue` (default steer). TUI: Enter steers, Ctrl+X then Enter queues, Esc cancels. Web: same in the composer with a `will queue…` hint.
- **Progression on by default:** TUI/web stream stages, tool steps, and text live with no flags; headless `lumina run` prints the same progression to stderr (`› thinking…`, `▸ read main.go`, `→ preview`, `› done model · 2.5s · 40 tok/s`) while stdout keeps only the final answer.
- **Known gap (not this change):** web has no `permission.asked` approval UI (TUI modal only).
