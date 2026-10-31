---
date_modified: 2026-10-31
tags: [lumina, live-test]
---

# lumina live wiring (verified)

- Transport: OpenAI Responses API (`/v1/responses` SSE; `instructions` + `input`, `response.output_text.delta` text deltas, function-call args reassembled on `response.completed`).
- Session header: `x-lumina-session: <uuid>` per request, mirrored as `x-opencode-session: <same-uuid>` — opencode zen gateway 400s (`MissingSessionID`) without it; harmless elsewhere. Covered by `TestRoutingSessionHeaders`.
- Credentials: strictly via `api_key_env` / `api_key_cmd` indirection (plaintext keys rejected at load); endpoint and model set per-provider in `~/.config/lumina/config.toml`, never recorded here.
- Lumina: openai-responses adapter + agentic tool loop (max 8 rounds) + session-context system block.
- Live-verified: reply, write/edit files, bash with output feedback.
See also: [[lumina-build]]
