# Lumina — workspace rules

This repo (`8nite/lumina`) is a **Rust + React** monorepo: `src/` (Rust)
is the maintained stack. The Go tree (`cmd/`, `internal/`, `pkg/`,
`go.mod`/`go.sum`) was deleted from this branch (`rust-revamp`) on
2026-11-01 after green parity baselines on both stacks; `main` is still
Go until this branch is tested and merged.
Keep changes boring, minimal, and reversible.

## How to work here

- `make build` builds everything (web SPA first, then the Rust binary → `bin/lumina`).
- `cargo test` (aka `make test`) — full Rust suite; must stay green.
- `cargo clippy --tests` (aka `make lint`) — keep new warnings at zero.
- `pnpm --dir web test && pnpm --dir web build` — web typecheck + bundle.
- Plan mode is read-only: research and blueprints go in `plan.md` or
  `.lumina/plans/*.md`. Flip to build mode (`Tab`) before editing code.
- Commit messages: short, imperative (`docs:`, `feat:`, `fix:`).
- Server port is fixed at `127.0.0.1:4089` (code default in `src/config.rs`
  and `src/main.rs`) — do not change it and do not start competing daemons
  on other ports. Ephemeral test daemons must use random free ports and be
  stopped afterwards.

## Rust-first — Go is gone from this branch, do not reintroduce it

- New behaviour goes into `src/*.rs`, with a Rust test. Do not recreate
  `cmd/`, `internal/`, `pkg/`, `go.mod`/`go.sum`, or Go build targets —
  the Go reference lives on `main` until merge.
- If you spot logic on `main`'s Go tree that has no Rust equivalent, port
  it to `src/` (test included) and note it in `memory/rust-port.md` —
  do not fix it in Go. Read-only parity diffing against `main` is fine.
- No pushes, merges, or branch deletes without explicit approval. `main`
  stays Go until this branch is fully tested and deliberately merged.
- Anything the Go tree does is expected to exist in Rust; when in doubt,
  check `memory/rust-port.md` (parity audits) before assuming a gap.
- Build/test/lint entry points are Cargo + Make.

## Architecture (`src/`, see `plan.md`)

- `src/main.rs` — single CLI entry (TUI, `run`, `serve`, `web`, `attach`, `session`, `plugin`, `kb`, `evolve`, `models`, `auth`, `pair`, `openapi`, `vault`).
- `src/server.rs` — axum daemon: REST + SSE + PTY WS on `127.0.0.1:4089` (fixed port, see above), embedded SPA (`rust-embed`).
- `src/security.rs` — plan/build fence + 4-mode broker + credential/Google hard-deny.
- `src/llm.rs` — BYO-LLM adapters (OpenAI-compat, Anthropic, Ollama, Copilot, Responses) + 3-tier router.
- `src/agent.rs` — turn loop, compaction, loop guards, subagents, steer/queue.
- `src/tools.rs` + `src/begin_patch.rs` — tool registry, `*** Begin Patch ***` engine.
- `src/kb.rs` — Markdown + `graphify` RAG (no vector DBs, no Google services).
- `src/harness.rs` — Harness² self-improvement (quality-floor gated).
- `src/store.rs` / `src/vault.rs` / `src/checkpoints.rs` — SQLite sessions, session secret vault, shadow-git undo.
- `src/tui.rs` / `src/sdk.rs` / `src/pty.rs` / `src/approve.rs` — ratatui client, typed client, web terminal, permission broker.
- `web/` — React + Vite + Tailwind SPA, embedded at build time.

## Standing ops

- Durable notes live in `./memory/*.md` (YAML frontmatter + `[[wikilinks]]`).
- After landing a fix or lesson: `lumina kb update <topic> "<note>"`.
