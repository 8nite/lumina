# Lumina — workspace rules

This repo (`8nite/lumina`) is a Go + React monorepo. Keep changes boring,
minimal, and reversible.

## How to work here

- `make build` builds everything (web SPA first, then the Rust binary → `bin/lumina`).
- `cargo test` (aka `make test`) — full Rust suite; must stay green. `make test-go` keeps the Go reference green.
- `pnpm --dir web test && pnpm --dir web build` — web typecheck + bundle.
- Plan mode is read-only: research and blueprints go in `plan.md` or
  `.lumina/plans/*.md`. Flip to build mode (`Tab`) before editing code.
- Commit messages: short, imperative (`docs:`, `feat:`, `fix:`).
- Server port is fixed at `127.0.0.1:4089` — do not change it and do not
  start competing daemons on other ports. Ephemeral test daemons must use
  random free ports and be stopped afterwards.

## Architecture (see `plan.md`)

- `cmd/lumina` — single CLI entry (TUI, `run`, `serve`, `web`, `session`, `plugin`, `kb`, `evolve`).
- `internal/server` — daemon REST + SSE + PTY WS on `127.0.0.1:4089` (fixed port, see above).
- `internal/security` — plan/build fence + 4-mode broker + credential/Google hard-deny.
- `internal/llm` — BYO-LLM adapters (OpenAI-compat, Anthropic, Ollama) + 3-tier router.
- `internal/kb` — Markdown + `graphify` RAG (no vector DBs, no Google services).
- `internal/harness` — Harness² self-improvement (quality-floor gated).

## Standing ops

- Durable notes live in `./memory/*.md` (YAML frontmatter + `[[wikilinks]]`).
- After landing a fix or lesson: `lumina kb update <topic> "<note>"`.
