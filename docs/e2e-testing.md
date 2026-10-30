# Lumina Web-Mode E2E + Auto-Compact Stress Test

Date: 2026-10-29 (re-executed same day, fresh run below; prior run archived in
git history). Target: `8nite/lumina` worktree `<workspace-checkout>`.
Daemon: `bin/lumina serve --port 17432` with `LUMINA_DB=/tmp/lumina-e2e-*/lumina.db`
and `LUMINA_CONFIG=/tmp/lumina-nonexistent-e2e.toml` (offline: zero providers,
canned replies). Workspace: temp git repo `/tmp/lumina-ws-e2e-*`. Browser:
`agent-browser` headless.

## Latest run (2026-10-29, session `643389f1`, 54 turns / ~73K chars)

Same procedure as §1–§4, re-executed end to end:

- Build: `make build` OK (SPA + 28M static binary). `go test ./...` all PASS,
  `pnpm --dir web test` PASS. `go vet ./...` clean.
- Web: zero console/page errors, 100% `127.0.0.1` network traffic. UI created
  the session, SSE `turn.completed` rendered. `Build→Plan` toggle persisted
  (`agent: plan`), perm cycle `ask→auto-edit→auto-approve→bypass→ask` via API,
  back to `build/bypass`. All drawers render (Review Unified/Split, Term input,
  Graph, Memory, Harness Inspect+Evolve). New since last run: `Auto` tier picker
  and image-attach (`🖼`) button in the composer (matches uncommitted TurnInput
  work in tree).
- Large context: 25 sequential ~2.8KB prompts, 2 → 52 turns, then UI prompt →
  54 turns. `Compact()` temp unit test PASS (60→31, header + 8 truncations;
  52-turn shape → 41). Post-window probes succeed.
- Undo/unrevert `ok`, sanitize/files/branch/CSP/providers/router/kb/harness
  real data, PTY `101`, restart persistence 54/54 turns.
- Cleanup: daemon stopped, all `/tmp/lumina-*` scratch removed.

## 1. Build & unit baseline

- `CGO_ENABLED=0 go test ./...` — all packages PASS (after two fixes below).
- `pnpm --dir web test` (`tsc --noEmit`) — PASS. `make build` — SPA + 28M static binary OK.

### Fixes required to compile (uncommitted working tree)

1. `internal/llm/router.go` — a mangled line (`} if m := ...`) broke `go vet`
   (`expected ';'`). Restored newline + `// Bare model id …` comment.
2. `internal/server/server_test.go` — `s.ListCopilotModels` renamed to
   `s.ListProviderModels` to match `server.go` field rename.

## 2. Web E2E (TP-10 matrix) — PASS

| Check | Result |
|---|---|
| `GET /openapi.json` 3.1.0, SPA `/` 200, CSP `default-src 'self'; …` | PASS |
| Headless browser: console errors / page errors | 0 / 0 |
| Network requests (doc/JS/CSS/API/SSE) | 100% `127.0.0.1`, zero external/Google |
| UI: New session → prompt → `turn.completed` renders via SSE | PASS (`27f83334`) |
| `Build→Plan` toggle in UI persisted to daemon (`agent: plan`) | PASS |
| Perm cycle `ask→auto-edit→auto-approve→bypass→ask` via API | PASS |
| Drawers: Review (Unified/Split, no-changes), Term (input), Graph, Memory, Harness (Inspect+Evolve), Connect (device flow) | all render, no errors |
| `POST revert` / `unrevert` (shadow-Git) | `{"status":"ok"}` both |
| `export?sanitize=1`, files, git/branch, providers, router, kb/query, harness | real data, no stubs |
| PTY WS handshake | `101 Switching Protocols` + valid accept key |
| Kill + restart daemon → sessions/turns | 2 sessions, 61 turns intact |

Note: with the real `~/.config/lumina/config.toml` (lumina provider) and no
`LUMINA_API_KEY`, prompts fail with `turn.failed` (401/404 from upstream or
ollama fallback). Offline `LUMINA_CONFIG` is required for keyless E2E.

## 3. Large-context loop → auto-compact — PASS (truncation only)

- Fire-and-forget ×55 parallel prompts loses most turns (SQLite `BUSY`, no
  retry in `RunPrompt`) — only 4/55 users recorded. Sequential send + wait for
  `export` +2 is lossless and matches UI behavior.
- Sequential ×25 prompts (~2.8KB each): 7 → 57 turns (~84K chars), crossing the
  40-message prompt-construction window in `RunPrompt`. Post-window probe:
  57 → 59 turns, then UI prompt → 61 turns. No errors.
- `Compact()` unit check (temp `TestZZCompactE2E`, removed after): 60 mixed
  turns → 31 out, header `[compacted 30 earlier turns; rules, diffs, and
  ./memory pointers preserved]`; 8 tool outputs >2000 chars truncated with
  `…[truncated by /compact]`; ≤keepRecent history passes through; 59-turn
  real shape → 41 out. PASS.

### Caveat (load-bearing)

`plan.md` §5.2 promises 80%-of-window Tier-1/2 summarization. That is **not
implemented**: no token counting, no 80% trigger, `Compact()` is never called
from `RunPrompt` (only the `len(msgs) > 40` break). What works is the 40-msg
cap + 2000-char tool truncation. Semantic auto-compact is future work.

## 5. `/connect` → `/model` fix (2026-10-30)

Symptom: after a successful `/connect` (device auth) to GitHub, `/model`
stayed empty ("no live models").

Root cause (reproduced): with `api_key_cmd = "gh auth token"` failing
(`gh` missing/logged out), `NewRouter` returned an error and `loadStack`
silently set `router = nil` — so `/providers` still advertised copilot and
`/connect` still succeeded (needs no router), but `/models` failed with
"no router configured" and the TUI skipped the provider. Second latent bug:
a stale construction-time key would shadow a fresh device token in
`exchange()` (store fallback only applies when the key is empty).

Fix (full `go test -count=1 ./...` green):

- `internal/llm/router.go`: secret-resolution failure → empty key, provider
  still registered (true config errors stay fatal). Added
  `RefreshCopilotKey` (+ `Copilot.SetKey`, drops cached session).
- `internal/server/server.go`: new `OnAuthRefresh` hook, fired in
  `handleDevicePoll` after the 0600 store write (in-process only, never logged).
- `cmd/lumina/main.go`: wires `OnAuthRefresh` → live router; `loadStack`
  now logs router errors to stderr instead of swallowing them.
- `internal/tui/model.go`: `/model` surfaces the first daemon error
  (`no live models: <err>`) instead of only hinting at `/connect`.

Live mock proof (isolated daemon + mock device/exchange/models servers,
`api_key_cmd = "exit 3"`): pre-auth `/models` → clear 502, device
start → poll `authorized`, post-auth `/models` → 2 models with **no
restart**; post-restart `/models` → 2 models via store fallback.
Live confirmation (2026-10-30, real GitHub): the user's `gh` PAT is
rejected by the exchange with 404 while the device token succeeds —
`exchange()` now retries with the store token on failure, and
`/api/v1/models?provider=copilot` returns the real model list.

Also fixed two typos in a concurrently-landed TUI helper
(`append(out, …)` → `append(out.Files/Notes, …)`).

## 6. Picker switch test, live (2026-10-30)

- `/api/v1/models?provider=copilot` → 47 models (picker source populated).
- Simulated the TUI picker's select (`PATCH` session model + `PUT` router
  executor) with `copilot/gpt-4o-mini-2024-07-18`: switch persisted, and the
  turn reached the live gateway (`api.individual.githubcopilot.com`) but
  failed honestly — 19k-token system prompt vs the model's 12288-token cap
  (`model_max_prompt_tokens_exceeded`). Picker/switch/auth all working;
  that model is just too small for Lumina's system prompt.
- `copilot/claude-sonnet-5`: gateway says `model_not_supported` on this
  endpoint (account/gateway-side availability, not a Lumina bug).
- `copilot/gpt-4o-2024-11-20`: `turn.completed` with the exact expected
  token. Full loop proven: list → switch → live reply.
- Note: the test left the persisted default executor on
  `copilot/gpt-4o-2024-11-20` (same as picking in `/model`); test session
  remains in the local DB.

## 7. Picker lists whatever is connected (2026-10-30)

lumina was configured all along — both pickers just hardcoded copilot.
TUI `/model` skipped every non-copilot provider; the web picker did the same
*and* mishandled the providers shape (objects, not strings).

- `internal/llm/anthropic.go`: added `ListModels` so all five protocols
  satisfy `ModelLister` (asserted in test); daemon + TUI already select on
  the interface, so no per-protocol code anywhere.
- `internal/tui/model.go`: list every provider, skip failures, qualify bare
  ids as `provider/id`.
- `web/src/App.tsx` + `client.ts`: same — all providers, tolerate failures,
  handle object-or-string provider entries, qualify bare ids.
- Verified live: `/models` returns 28 lumina + 47 copilot models; web
  dropdown renders both (`lumina/muse-spark-1.3-contributor`,
  `copilot/...`, …); router executor set to lumina via API.

## 4. Cleanup

Daemon stopped, `/tmp/lumina-e2e-*`, `/tmp/lumina-ws-*`, temp test file, and
SIDs removed. Repo diff limited to the two compile fixes; 20+ other dirty files
are pre-existing uncommitted work, untouched.

## 8. Window, compaction, BUSY + behavioral evals fix (2026-10-30)

Four gaps closed (all five providers now advertise windows via `/models` /
`/api/show`; gauge shows used-only when unknown):

1. **Provider-aware compaction wired into `RunPrompt`** (`internal/agent/agent.go`):
   newest-first history under 80% of the advertised window, newest-40
   ceiling when unknown, re-compacted every agentic round. This also fixes a
   latent inversion: the old oldest-first 40-msg cap silently dropped the
   *current* user prompt once history passed 40 messages.
2. **Pre-flight window check**: a compacted prompt that still exceeds the
   advertised window fails fast with model + sizes (the 19k-prompt vs 12k-model
   incident now errors before any provider request instead of mid-turn).
3. **`SQLITE_BUSY` root-caused**: `busy_timeout` was set on one pooled
   connection only. `Store` now uses `SetMaxOpenConns(1)` (one daemon owns the
   DB; all statements serialize) plus a bounded busy-retry on every write path
   for cross-process contention (`lumina run` sharing `LUMINA_DB`).
4. **Behavioral eval suite** (`internal/agent/behavioral_test.go`): 4 fast,
   deterministic, local-only tests asserting harness actions, not prose —
   over-window fails fast with zero provider hits, 200-turn history compacts
   under 80% keeping newest / evicting oldest, 16×25 concurrent writes lose
   nothing, unknown window keeps the newest-40 ceiling.

Verified: `CGO_ENABLED=0 go test -count=1 ./...` all PASS,
`go test -race ./internal/agent/` clean, `go vet` clean.
