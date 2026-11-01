---
date_modified: 2026-11-01
tags: [lumina, rust-port, migration, go-removal]
---

# Rust migration: Go tree deleted from `rust-revamp`

Decision (2026-11-01): Rust (`src/`) is the maintained stack. The Go tree
(`cmd/`, `internal/`, `pkg/`, `go.mod`/`go.sum`, `web/embed.go`,
`tmp-tui-test/`) was deleted from branch `rust-revamp` only. `main` is
still Go until this branch is fully tested and deliberately merged — no
pushes, merges, or branch deletes without explicit approval.

## Rule for all future work

- New behaviour goes into `src/*.rs` with a Rust test. Never recreate Go
  paths or Go Makefile targets on this branch.
- If Go logic is spotted (on `main`, in a patch, in a stale checkout)
  with no Rust equivalent: port it into `src/` **first**, add a Rust
  test, record the gap here. Do not fix it in Go.
- Parity reference: diff read-only against `main`'s Go tree when in doubt.

## Deletion baselines (pre-delete, 2026-11-01)

- `cargo test`: 101 passed, 0 failed.
- `CGO_ENABLED=0 go test ./...`: all packages ok (cmd, agent, config,
  harness, kb, llm, mcp, plugins, rules, security, server, tools, tui, sdk).
- `cargo clippy --tests`: 17 warnings, all pre-existing surface (no new).
- `pnpm --dir web test` (`tsc --noEmit`): exit 0.

## Post-delete verification (to run before merge)

- `cargo test`, `cargo clippy --tests` (must stay at 17, no new),
  `pnpm --dir web test`, `make build`, plus `tests/e2e-live.sh` against a
  live backend. Deletion itself cannot break the Rust build (no `*.rs`
  references Go paths; `rust-embed` covers the SPA, `web/embed.go` was
  Go-only), but re-run to prove it.

## Small behaviour deltas shipped with the migration docs

- `src/security.rs` readonly-shell allowlist + `src/tools.rs` test now use
  `cargo test`/`cargo check`/`cargo clippy` instead of `go test`
  (README permission table + `[permissions.bash]` example updated to match).
- `AGENTS.md` port corrected to `127.0.0.1:7432` (was stale `4089`;
  code default in `src/config.rs` / `src/main.rs`). Superseded
  2026-11-01: canonical port is `127.0.0.1:4089` (ai-manager owns it;
  other apps bind `:7432`). See [[stuck-session-fix]].
- `plan.md` / `test-plan.md` marked historical (written for Go); behaviour
  guarantees still hold, gates map to `cargo test` / `cargo clippy`.

See also: [[rust-port]], [[project]]
