# Lumina Verification & Test Plan (`test-plan.md`)

Verification matrix for `8nite/lumina` against `plan.md` (§1–§14) and `AGENTS.md`.

## Test Environment & Isolation Rules

- **Workspace:** `<workspace-checkout>`
- **Build target:** `CGO_ENABLED=0` static Go binary (`go1.22+`) + Vite/React 19 SPA (`node v22`, `pnpm`).
- **State isolation:** Every CLI / integration / daemon test MUST set `LUMINA_DB=/tmp/lumina-test-<run>/lumina.db` and bind ephemeral/isolated ports (`127.0.0.1:17432+`), never polluting `~/.local/share/lumina/lumina.db` or colliding with `:7432`.
- **Scratch hygiene:** All `/tmp/lumina-*` scratch directories created during testing are removed before exit.
- **No-secret / No-Google rule:** Never read live credential files; test credential guards solely against synthetic dummy filenames inside `/tmp/lumina-test-*` (`t.TempDir()`).

---

## Suite Overview

| Suite | Scope | Primary Targets | Status |
|---|---|---|---|
| `TP-00` | Build, Static Analysis & Supply-Chain Audit | `Makefile`, `go.mod`, `web/package.json` | `PASS` |
| `TP-01` | Config, Secret Indirection & No-Google Guards | `internal/config` | `PASS` |
| `TP-02` | Security Fence (`plan` vs `build`) & 4 Approval Modes | `internal/security`, `internal/tools` | `PASS` |
| `TP-03` | BYO-LLM Streaming Adapters & 3-Tier Router | `internal/llm` | `PASS` |
| `TP-04` | Agent Loop, Sessions, Sanitized Export & Shadow-Git Undo/Redo | `internal/agent`, `internal/mcp` | `PASS` |
| `TP-05` | Hierarchical `AGENTS.md` Rules, Skills & Lumina JS/TS Plugins | `internal/rules`, `internal/plugins` | `PASS` |
| `TP-06` | Markdown + `graphify` KB & Singleflight `standing-ops` | `internal/kb`, `memory/graphify-out/` | `PASS` |
| `TP-07` | Harness² (`Propose → Probe → Compose` & Improvement Library) | `internal/harness` | `PASS` |
| `TP-08` | Daemon HTTP/SSE/PTY & CLI Subcommands | `internal/server`, `pkg/sdk`, `cmd/lumina` | `PASS` |
| `TP-09` | Bubble Tea TUI State Machine | `internal/tui`, `cmd/lumina/tui_client.go` | `PASS` |
| `TP-10` | Embedded Web SPA Build, CSP & Live Headless Browser E2E | `web/`, live `lumina serve` | `PASS` |
| `TP-11` | GitHub Copilot Provider (SaaS/Enterprise/GHES), `/model` & Web Auth | `internal/llm` (copilot), daemon auth endpoints, TUI, web, CLI | `PASS` |

---

## Detailed Test Cases

### `TP-00` — Build, Static Analysis & Zero-Google Supply Chain (`plan.md §1, §4, §14`)

| ID | Check | Command / Action | Pass Criteria | Status |
|---|---|---|---|---|
| `TP-00.1` | Go static analysis | `CGO_ENABLED=0 go vet ./...` | Exit `0`, zero warnings | `PASS` |
| `TP-00.2` | Pure-Go unit suite | `CGO_ENABLED=0 go test -v ./...` | Exit `0`, all packages pass without CGO | `PASS` |
| `TP-00.3` | Race detector unit suite | `go test -race ./...` | Exit `0`, zero data races across SSE broker, sweeper, plugin host, store | `PASS` |
| `TP-00.4` | Web typecheck & bundle | `pnpm --dir web test && pnpm --dir web build` | `tsc --noEmit` + `vite build` exit `0`, `web/dist/index.html` emitted | `PASS` |
| `TP-00.5` | Full monorepo build | `make build` | Produces static `bin/lumina` with embedded `web/dist`; `ldd bin/lumina` reports `not a dynamic executable` (or static Go) | `PASS` |
| `TP-00.6` | Zero-Google dependency audit | Inspect `go.mod`, `web/package.json`, `web/index.html`, `web/src/**` | Zero Google AI/Cloud/Analytics/Fonts SDKs or URLs (`github.com/google/uuid` RFC4122 helper is local-only; verify zero network/cloud SDKs) | `PASS` |

---

### `TP-01` — Config, Secret Indirection & No-Google Guards (`plan.md §7.1, §8`)

| ID | Check | Scenario | Pass Criteria | Status |
|---|---|---|---|---|
| `TP-01.1` | Missing config defaults | `config.Load` on non-existent path | `127.0.0.1:7432`, `permission_mode="ask"`, `standing_ops=true`, `debounce=15` | `PASS` |
| `TP-01.2` | Plaintext `api_key` rejection | `api_key = "sk-live-..."` in `config.toml` | `Load` returns hard error refusing plaintext key | `PASS` |
| `TP-01.3` | Google endpoint rejection | `base_url` with `generativelanguage.googleapis.com`, `aiplatform.googleapis.com`, `fonts.googleapis.com`, `vertexai` | `Load` returns hard error (`blocked Google endpoint`) | `PASS` |
| `TP-01.4` | Google env var rejection | `api_key_env = "GEMINI_API_KEY"` / `"GOOGLE_API_KEY"` | `Load` returns hard error | `PASS` |
| `TP-01.5` | `api_key_env` & `api_key_cmd` indirection | Resolve secret from env var and shell `api_key_cmd` (`sh -c "printf tok"`) | Returns exact secret in memory without writing to disk/logs | `PASS` |
| `TP-01.6` | Permissions & router TOML tables | `[permissions]`, `[permissions.bash]`, `[router]`, `PersistModel` | Glob rules parsed accurately; `PersistModel` updates role in `config.toml` | `PASS` |

---

### `TP-02` — Security Fence (`plan` vs `build`) & 4 Approval Modes (`plan.md §6.6, §6.7, §7`)

| ID | Check | Scenario | Pass Criteria | Status |
|---|---|---|---|---|
| `TP-02.1` | `plan` + `bypass` edit block | `Authorize(AgentPlan, PermBypass, "edit"|"write", "src/main.go", ...)` | `Deny` (structural fence overrides `bypass`) | `PASS` |
| `TP-02.2` | `plan` writable exceptions | `write` to `plan.md` and `.lumina/plans/ses-1.md` inside workspace | `Allow`; verify out-of-workspace `/tmp/other/plan.md` or `../plan.md` cannot escape workspace | `PASS` |
| `TP-02.3` | `plan` bash allowlist & chaining/redirection bypass probe | `git status` (`Allow`) vs `rm -rf` (`Deny`) vs **adversarial:** `echo pwned > a.go`, `ls; touch pwned`, `git status && rm a.go`, `cat $(...)` | Any shell redirection/chaining/subshell in `plan` mode MUST be `Deny` | `PASS` |
| `TP-02.4` | Credential hard-deny across all modes & tools | `read` / `edit` / `write` / `bash` / `grep` against synthetic `.env`, `auth.json`, `lumina.json`, `.kube/config`, `id_rsa`, `secret.json`, `--from-literal=` under `PermBypass` | `Deny` for direct access AND `grep` walk must skip credential files | `PASS` |
| `TP-02.5` | Workspace path-escape guard on all file args | `tools.Call` with `{"path": "../escape.txt"}` AND `{"file": "../escape.txt"}` for `read`/`write`/`edit` | Both `path` and `file` aliases blocked outside `WorkspaceDir` | `PASS` |
| `TP-02.6` | `build` 4-mode permission matrix | `ask` vs `auto-edit` vs `auto-approve` vs `bypass` across `read`, `edit`, safe `bash`, destructive `bash` (`rm -rf`, `git reset --hard`), `mcp`, shared `AGENTS.md`/`skills/` writes | Matches `plan.md §6.6` matrix cell-for-cell | `PASS` |

---

### `TP-03` — BYO-LLM Streaming Adapters & 3-Tier Router (`plan.md §8`)

| ID | Check | Scenario | Pass Criteria | Status |
|---|---|---|---|---|
| `TP-03.1` | OpenAI-compatible SSE + tool calls + reasoning | Mock `/chat/completions` streaming `content`, `reasoning_content`, `tool_calls` | Concatenates delta/thinking and decodes `ToolCallRequest` args | `PASS` |
| `TP-03.2` | OpenAI Responses API (`/v1/responses`) SSE | Mock `/responses` streaming `response.output_text.delta` + `response.output_item.added` + `response.function_call_arguments.delta` | Emits text deltas and reassembled function call args on `response.completed` | `PASS` |
| `TP-03.3` | Anthropic `/v1/messages` SSE + tool support | Mock `/v1/messages` with `x-api-key`, `thinking_delta`, `text_delta`, `tool_use` | Streams text/thinking and supports tool schemas/calls | `PASS` |
| `TP-03.4` | Ollama `/api/chat` NDJSON | Mock local NDJSON stream | Streams chunks cleanly until `done: true` | `PASS` |
| `TP-03.5` | 3-Tier Router resolution | `ModelFor("planner", "plan")` fallback to `executor`; `ModelFor("executor", "plan")` promotion to `planner`; Google provider rejection | Matches `plan.md §8.2` | `PASS` |

---

### `TP-04` — Agent Loop, Sessions, Sanitized Export & Shadow-Git Checkpoints (`plan.md §5.2, §6.8, §6.9, §12`)

| ID | Check | Scenario | Pass Criteria | Status |
|---|---|---|---|---|
| `TP-04.1` | Session CRUD & `-c` workspace resume | `CreateSession`, `LatestSession(ws)`, `UpdateSession`, `ListTurns` | Most recent workspace session returned; turns ordered accurately | `PASS` |
| `TP-04.2` | Session fork (`ForkSession`) | Fork at latest turn and at specific `turnID` | `ParentID` set; ancestor turns copied up to `turnID`; original untouched | `PASS` |
| `TP-04.3` | Transcript secret sanitization (`SanitizeExport`) | Transcript containing `sk-...`, `AKIA...`, `Bearer ...`, `api_key=...`, `secret=...` | All secret tokens replaced with `[REDACTED]` | `PASS` |
| `TP-04.4` | Multi-round agentic tool loop & tool JSON schemas | Mock LLM emits `write` → `read` → final answer via `Runner.RunPrompt`; inspect `toolSpecs()` | Tool results fed back; `Schema` defined for tool parameters; `kb_query` & `skill` wired in `loadStack()` | `PASS` |
| `TP-04.5` | Shadow-Git `/undo` & `/redo` (`Snapshot` / `Revert`) | Mutate tracked + untracked files, `Snapshot`, mutate again, `Revert`, check user `git status --cached` | Working tree restored; user `.git/index` untouched; zero `.index.lock` leaks | `PASS` |
| `TP-04.6` | Local MCP stdio & SSE client | `mcp.Dial` against mock stdio server & mock SSE server + Google URL check | `initialize`, `tools/list`, `tools/call` succeed; Google SSE URL blocked | `PASS` |
| `TP-04.7` | Context estimate & turn summary | `EstimateContextTokens` on 1- and 3-message histories; `TurnStats.Summary` with/without ctx | Exact small-history value; monotonic growth; footer shows `ctx ~12.0k/200.0k`; omitted when unknown | `PASS` |

---

### `TP-05` — Hierarchical `AGENTS.md` Rules, Skills & Lumina JS/TS Plugins (`plan.md §6.1–§6.5`)

| ID | Check | Scenario | Pass Criteria | Status |
|---|---|---|---|---|
| `TP-05.1` | 4-Layer instruction precedence | Layer 0 core + `~/.config/lumina/AGENTS.md` + repo root `AGENTS.md` + subdir `AGENTS.md` + `.lumina/harness/{system_prompt,tool_guidance}.md` | Merged in exact Layer 0 → 1 → 2 → 3 order | `PASS` |
| `TP-05.2` | `CLAUDE.md` fallback & compat imports | Repo with `CLAUDE.md` and no `AGENTS.md` | `CLAUDE.md` loaded as Layer 2 fallback | `PASS` |
| `TP-05.3` | `SKILL.md`, custom agents & slash commands discovery | `.lumina/skills/*/SKILL.md`, `.lumina/agents/*.md`, `.lumina/commands/*.md` | Frontmatter parsed; sorted deterministically | `PASS` |
| `TP-05.4` | Lumina JS/TS plugin worker (`host.mjs`) | Load both factory-style (`rtk.mjs`) and v2 `setup(ctx)` (`guard.mjs`) plugins over Unix socket | Arg rewrite patches payload; thrown error / `{block}` blocks execution | `PASS` |
| `TP-05.5` | Out-of-repo plugin execution & tool-call wiring | Run `plugins.Host` when `cwd` is `/tmp/...` (outside Lumina repo) and check `tools.Registry` hook integration | `host.mjs` resolves from embedded copy when not on disk; hooks fire around tool calls | `PASS` |

---

### `TP-06` — Markdown + `graphify` KB & Singleflight `standing-ops` (`plan.md §9`)

| ID | Check | Scenario | Pass Criteria | Status |
|---|---|---|---|---|
| `TP-06.1` | Schema normalization & `#479` shrink-guard | `LoadGraph` with `{nodes:[...]}`, `{from,to}` edges, degenerate 1-node edge, unknown confidence | Degenerate edge dropped; `{from,to}` normalized; confidence coerced to `EXTRACTED\|INFERRED\|AMBIGUOUS` | `PASS` |
| `TP-06.2` | Graph queries & <2ms traversal on real `memory/graphify-out/graph.json` | Run `BFS`, `DFS`, `ShortestPath`, `Explain`, `Affected`, `GodNodes` on synthetic + real `./memory/graphify-out/graph.json` | Accurate results; real graph traversal completes in `<2ms` | `PASS` |
| `TP-06.3` | Step 0 constrained vocab expansion | `ExpandVocab("how does the session router work", 12)` | `<=12` tokens, 100% present in `g.Vocab()`, zero hallucinated synonyms | `PASS` |
| `TP-06.4` | Cycle-safe `[[wikilink]]` resolution | `a.md ↔ b.md → c.md` under budget | Terminates cleanly without infinite loop; respects note budget | `PASS` |
| `TP-06.5` | Singleflight `Sweeper` (`6x` hook-race fix) & `ManifestStamp` | 6 concurrent `Sweeper.Run("/ws", ...)` triggers; `ManifestStamp` | Sweep body runs exactly `1` time; `manifest.json` stores `/` root-relative paths | `PASS` |

---

### `TP-07` — Harness² Recursive Self-Improvement (`plan.md §10`)

| ID | Check | Scenario | Pass Criteria | Status |
|---|---|---|---|---|
| `TP-07.1` | 8-component overlay bootstrap & additive `guardrails.yaml` | `Loader.Ensure()`, `Set("guardrails.yaml", ...)` adding vs removing a check | Starter files/dirs created; removing any non-comment guardrail line is rejected | `PASS` |
| `TP-07.2` | Isolated `git worktree` `Probe` + `e2e-*.sh` gate | Probe 2 candidates (`good` score `0.9`, `bad` score `0.2`) + failing `e2e-check.sh` candidate | Candidates evaluated in `/tmp/lumina-probe-*`; failing `e2e-*.sh` rejected; zero leaked git worktrees afterward | `PASS` |
| `TP-07.3` | Quality-Floor `Compose` & `refs/lumina/harness` shadow commit | `Compose` with `H0 = 0.5`, then `CommitAll` | Only `good` lands; `refs/lumina/harness` updated without moving `HEAD` or user `.git/index` | `PASS` |
| `TP-07.4` | Level-2 Improvement Library `Promote` | `Library.Promote` into temp library dir | Writes Markdown note with YAML frontmatter (`name`, `date`, `rationale`) and `[[harness-evolution]]` wikilink | `PASS` |

---

### `TP-08` — Daemon REST/SSE/PTY & CLI Subcommands (`plan.md §5.4, §12.2`)

| ID | Check | Scenario | Pass Criteria | Status |
|---|---|---|---|---|
| `TP-08.1` | OpenAPI 3.1 & Bearer auth enforcement | `GET /openapi.json`; `GET /api/v1/sessions` with & without `LUMINA_TOKEN`; check `--host 0.0.0.0` without `LUMINA_TOKEN` | OpenAPI `3.1.0` lists all routes; `401` when token missing/wrong; non-loopback bind requires `LUMINA_TOKEN` | `PASS` |
| `TP-08.2` | Daemon ↔ SQLite session persistence & multi-turn continuity | Start `lumina serve` on isolated port + `LUMINA_DB`, `POST /api/v1/sessions`, send 2 prompts, restart daemon, `GET /api/v1/sessions` | Session IDs match between HTTP API and SQLite; turn 2 sees turn 1 history; sessions survive daemon restart | `PASS` |
| `TP-08.3` | Daemon functional endpoints audit | Test `/api/v1/files`, `/api/v1/git/diff`, `/api/v1/providers`, `/api/v1/kb/query`, `/api/v1/sessions/{id}/export?sanitize=1`, `/api/v1/sessions/{id}/revert`, `/api/v1/router`, `/api/v1/harness/evolve` | Return real workspace/store/KB/git data (not hardcoded empty stubs) | `PASS` |
| `TP-08.4` | SSE `/api/v1/events` real-time broadcast | Subscribe via `sdk.Client.Subscribe`, trigger `session.created`, `session.updated`, `turn.started`, `turn.completed` | Subscriber receives all events in order | `PASS` |
| `TP-08.5` | PTY WebSocket (`/api/v1/pty`) | RFC6455 upgrade handshake + send masked text frame `echo pty-ok` | `101 Switching Protocols` with valid `Sec-WebSocket-Accept`; receives unmasked text frame containing `pty-ok` | `PASS` |
| `TP-08.7` | Concurrent prompts single-flight | Slow `Exec` blocked on a channel; fire overlapping prompts while turn 1 runs | 2nd returns `steered`, 3rd (`queue`) returns `queued`; max in-flight `Exec` = 1; exactly 1 `turn.completed` | `PASS` |
| `TP-08.8` | `turn.context` gauge + completed ctx fields | `Exec` reports `NoteTurnEnd` with `ContextTokens`/`MaxTokens` | `turn.context` SSE carries `context_tokens`/`max_tokens`; `turn.completed` merges both fields | `PASS` |
| `TP-08.6` | CLI subcommands end-to-end | `lumina --version`, `--help`, `openapi`, `run`, `run -c`, `run -s`, `run --plan`, `run --format json`, `session list/show/fork/export --sanitize`, `plugin list/doctor`, `kb status/query/reflect/update`, `evolve`, `pair` | Exit `0` with expected outputs against isolated `LUMINA_DB` & temp git workspace | `PASS` |

---

### `TP-09` — Bubble Tea TUI State Machine (`plan.md §11.1`)

| ID | Check | Scenario | Pass Criteria | Status |
|---|---|---|---|---|
| `TP-09.1` | `Tab` & `Shift+Tab` mode cycling + daemon sync | Key `Tab` (`build→plan→build`) and `Shift+Tab` (`ask→auto-edit→auto-approve→bypass→ask`) against mock daemon | Local pills update AND `PATCH /api/v1/sessions/{id}` sent to daemon | `PASS` |
| `TP-09.2` | Slash commands (`/plan`, `/build`, `/mode`, `/compact`, `/undo`, `/redo`, `/resume`, `/fork`) | Execute each slash command in `Model` | Updates state AND syncs mode/undo/redo/fork to daemon when client attached | `PASS` |
| `TP-09.3` | `@file` & `[[memory]]` completions | `CompleteInput` on `@src/ma` and `[[mem` | Returns `@src/main.go` and `[[memory-notes]]` | `PASS` |
| `TP-09.4` | Inline permission modal & session-scoped SSE filtering | Receive `permission.asked`, select `Allow Session` + `Enter`; receive `turn.completed` for another `session_id` | Sends `AnswerPermission` to daemon; ignores SSE `turn.completed` from other sessions | `PASS` |
| `TP-09.5` | Space input, context pill & tool target | Type `hi` + space + `there`; receive `turn.context`; `turn.tool` with target; `turn.completed` with ctx | Input is `hi there`; header shows `ctx ~12.3k/200.0k`; status shows `tool: <name> <target>`; footer carries `ctx` summary | `PASS` |

---

### `TP-10` — Embedded Web SPA, CSP & Live Headless Browser E2E (`plan.md §11.2`)

| ID | Check | Scenario | Pass Criteria | Status |
|---|---|---|---|---|
| `TP-10.1` | Embedded SPA serving & HTTP CSP headers | `GET /` on live `lumina serve` (isolated port) | `200 OK`, serves built React bundle; strict `Content-Security-Policy` present; zero external font/CDN tags | `PASS` |
| `TP-10.2` | Zero external / Google network requests | Open `http://127.0.0.1:<port>/` in headless browser (`agent-browser`), inspect all network requests, console errors, page errors | Zero non-`127.0.0.1` requests; zero JS runtime errors | `PASS` |
| `TP-10.3` | Live UI interaction flow | Click `New` session → toggle `PLAN` / `BUILD` (`ModeBar` & `Tab`) → cycle `ASK`/`AUTO-EDIT`/`AUTO-APPROVE`/`BYPASS` → submit prompt → verify SSE reply renders in `ChatTimeline` → test `sessions`/`files`/`memory`/`harness` tabs & `diff`/`term`/`graph` drawers | All UI controls respond without errors and state persists via daemon API | `PASS` |
| `TP-10.4` | Context pill, approval modal & SSE reconnect | `turn.context` while working; `permission.asked` with no CLI; kill/retry SSE stream | TopBar shows `ctx ~x/y`; modal answers `allow_once/session/deny` and turn completes; client backoffs-reconnects, `working` clears on terminal event | `PASS` |

### `TP-11` — GitHub Copilot Provider, `/model` Connect & Device Web Auth

| ID | Check | Scenario | Pass Criteria | Status |
|---|---|---|---|---|
| `TP-11.1` | Token exchange + caching + Enterprise routing | Mock `copilot_internal/v2/token` (expiry, `endpoints.api`, `proxy-ep`), mock `/chat/completions`; two consecutive `Chat` calls; `BaseURL` override | `Bearer` session token + editor headers sent; exchange called exactly once (cached); `proxy-ep`/override win over fallback | `PASS` |
| `TP-11.2` | Device web-auth + token store hygiene | Mock `/login/device/code` + `/login/oauth/access_token` (pending → token); daemon `start`/`poll` | `user_code` returned, token never in any response; `0600` store written; `HasTokenStore` true; `pending` mapped correctly | `PASS` |
| `TP-11.3` | `/model` picker connect + daemon `/api/v1/models` | Mock providers/models endpoints; TUI `/model` → pick → enter; CLI `models`/`auth device` flows | PATCH session model + PUT router persist fire; CLI lists live models; device CLI prints URI/code and stores on approval | `PASS` |
| `TP-11.4` | Live endpoint verification (no secrets) | Real `POST github.com/login/device/code` (no auth) + real exchange with dummy token | Device code issued (proves client_id/host); exchange 401 (proves endpoint shape, key rejected) — never a real user token | `PASS` |
| `TP-11.5` | Image attach end-to-end (vision) | Mock: Responses `input_image` parts, chat `image_url` parts; CLI `--attach`, web attach button; live: model correctly describes the attached screenshot | `PASS` |
| `TP-11.6` | Reasoning-effort variants | `low/medium/high/xhigh/max` via Responses `reasoning.effort` + chat `reasoning_effort`; live gateway accepted all probed levels | `PASS` |

---

## Execution Log & Findings

Executed 2026-10-29 against `main` post-implementation. Isolation honored:
`LUMINA_DB=/tmp/lumina-test-*/lumina.db`, daemon ports `17432/17433`,
scratch in `/tmp/lumina-*` (removed after), synthetic-only fixtures
(`t.TempDir()`), no live credential reads (vault wrapper by name only).

### Suite results

| Suite | Result | Evidence |
|---|---|---|
| `TP-00` | PASS | `go vet ./...` 0 warnings; `go test ./...` all green; `go test -race ./internal/... ./pkg/...` green, zero data races; `pnpm test` + `pnpm build` green; `make build` emits static `bin/lumina` (`ldd`: not a dynamic executable); supply audit: no Google SDKs/URLs in `go.mod`/`web/package.json`/sources/bundle (only React dev error-code link) |
| `TP-01` | PASS | `TestDefaultsWhenMissing` (+`TestPermissionsTables`, matrix `TP-01.1/01.5/01.6`): `127.0.0.1:7432`, `ask`, `standing_ops=true`, `debounce=15`; plaintext `api_key` rejected; Google domains + `GEMINI/GOOGLE_API_KEY` env rejected; `api_key_env`/`api_key_cmd` resolve in-memory; `PersistModel` round-trips |
| `TP-02` | PASS | Matrix `TP-02.2–02.6` + package tests: plan+bypass denies source edits; out-of-workspace `plan.md` escapes denied; adversarial `>`,`>>`,`;`,`&&`,`\|`,`$()`,backticks,`<` denied in plan mode; credential/`.kube`/Google hard-deny in bypass; `path`+`file` aliases fenced; grep walk skips secret files; build matrix matches plan table cell-for-cell |
| `TP-03` | PASS | Matrix `TP-03.1–03.4` + adapter tests: OpenAI SSE deltas/thinking/tool-calls; Responses text + function-call reassembly; Anthropic text/thinking/`tool_use` streaming **and** `input_schema` sent; Ollama NDJSON; router planner↔executor promotion + Google rejection |
| `TP-04` | PASS | `TestAgenticToolLoop` + matrix `TP-04.4`: 3-round mock-LLM loop, files written, JSON schemas sent, kb/skill wired; sanitize redacts `sk-/AKIA/Bearer`; checkpoint hygiene (user `.git/index` untouched, zero `.lock` leaks, untracked files survive); MCP stdio + SSE transports, Google SSE URL blocked |
| `TP-05` | PASS | Matrix `TP-05.1/05.5` + `TestLayerOrder`: Layer 0→1→2→3 order asserted; `CLAUDE.md` fallback; skills/agents/commands discovery; factory + v2 plugin hooks; embedded `host.mjs` works with cwd outside the repo; registry `BeforeCall/AfterCall` fire around tool calls |
| `TP-06` | PASS | Matrix `TP-06.2` (real `memory/graphify-out/graph.json`: 11 nodes, traversal `707µs` < 2ms) + package tests: schema normalization, shrink-guard, constrained vocab (zero invented tokens), cycle-safe wikilinks, 6-way singleflight (max concurrency 1, exactly-once) |
| `TP-07` | PASS | Matrix `TP-07.2` + package tests: `Ensure` + additive-only guardrails; failing `e2e-*.sh` rejects high-score candidate with zero worktree leaks; quality floor lands only winners on `refs/lumina/harness`; library notes carry frontmatter + `[[harness-evolution]]` |
| `TP-08` | PASS | Live daemon `:17432/:17433` + unit tests: OpenAPI 3.1; non-loopback bind refused without `LUMINA_TOKEN`; store-backed sessions survive restart with turn continuity; files/git-diff/providers/kb/export/revert return real data; SSE fan-out to 2 clients; PTY 101 + echo; full CLI matrix green on isolated DB |
| `TP-09` | PASS | Matrix `TP-09.1/09.2/09.4` + TUI state tests: Tab/Shift+Tab PATCH the daemon; `/plan /build /mode /undo /redo /resume /fork` sync; permission modal answers via `AnswerPermission`; foreign-session events ignored |
| `TP-10` | PASS | Headless `agent-browser` E2E vs live daemon: zero console errors; network log 100% `127.0.0.1` (doc/JS/CSS/API/SSE); New → PLAN toggle → prompt → live `muse-spark-1.3-contributor` reply rendered in timeline; memory tab + graph drawer render; inline SVG favicon (no 404) |

### Issues found & fixed during this run

1. `config.toml` `[permissions.bash]` subtable crashed TOML decode (map collision) → deferred `toml.Primitive` decoding; added `standing_ops` + `debounce=15` defaults per this plan.
2. Plan-mode shell allowed `>`, `;`, `&&`, `\|`, `$()`, backticks → hard deny on any metacharacter.
3. Out-of-workspace `plan.md` writes (`/tmp/other/plan.md`, `../plan.md`) passed the plan fence → workspace containment enforced in `IsPlanWritable`.
4. Grep *pattern* `"SECRET"` tripped the credential fence → authorize on searched path only; walker skips secret files.
5. `write` via `{"file": ...}` alias bypassed the escape guard → both aliases checked.
6. Anthropic adapter dropped `tool_use` responses and never sent `input_schema` → both implemented.
7. MCP SSE transport had no Google-URL check → `CheckNoGoogle` in `Dial`.
8. `//go:embed` does **not** mark a plain `import "embed"` as used (verified on go1.22 + go1.26) → blank import `_ "embed"`.
9. `gitRoot` missed bare `.git` dirs (no `HEAD`) → directory markers accepted.
10. Harness overlay resolved under nested cwd → repo-root overlay first, workspace-dir overlay second.
11. Server sessions were memory-only (restart loss, CLI/daemon split) → SQLite-backed via `InsertSession`, unified id space, real transcripts, shadow revert/redo.
12. Non-loopback `serve` without token was allowed → hard refusal advising `lumina pair`.
13. TUI swallowed cross-session SSE and never answered permission modals → session filter + `AnswerPermission` wiring + daemon-synced slash commands.
14. `.kube/**` paths were not credential-fenced → whole tree hard-denied.
15. `run` swallowed trailing flags (`run "x" -c`) → position-independent flag pre-scan.
16. Verification matrix file itself referenced pre-change APIs (`c.KB.Debounce`, old `Authorize` arity) → updated to current signatures.

### TP-11 execution (GitHub Copilot provider)

Implemented `copilot` protocol: token exchange with in-memory cache (5-min
refresh margin), Enterprise routing (`proxy-ep` / `endpoints.api` / explicit
`base_url` override wins), required editor headers, OpenAI-compatible chat
reuse, `GET /models` listing, daemon device web-auth
(`POST /api/v1/auth/device/{start,poll}` — token stored `0600`, never
returned), TUI `/model` picker + direct set with daemon sync, web
`CopilotConnect` component, CLI `models` + `auth device`, per-user
`base_url`/`token_url` overrides in `config.toml`.
 verified: unit + matrix tests green (exchange caching, override routing,
device pending→token, 0600 store, models/PATCH/PUT wiring); live probes with
no secrets — real `POST github.com/login/device/code` issued a user code
(after correcting the client_id to `Iv1.b507a08c87ecfe98`), and a dummy-token
exchange hit the real endpoint shape (auth rejected, not 404).
