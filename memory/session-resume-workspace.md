---
date_modified: 2026-11-01
tags: [lumina, sessions, resume, bugfix, store]
author: opencode
---

# Web/CLI session resume mismatch (`.` vs absolute `workspace_dir`)

Web UI creates sessions with `workspace_dir="."` (App state default, `send()` in
`web/src/App.tsx`), while CLI/TUI/headless use the absolute cwd (`cwd()` in
`src/main.rs`). `latest_for_dir` / `list_for_dir` in `src/store.rs` compared
with exact SQL equality, so the two spellings never matched: `-c`/`run -c`
could not find web sessions and vice versa. Live evidence 2026-11-01: session
`570ea66e` stored as `"."` invisible to `?latest=1&directory=/home/herbert/lumina`.

Fix: `normalize_workspace()` in `src/store.rs` (canonicalize if the path
exists, else join relative onto process cwd + lexical `.`/`..`/slash cleanup;
never fails). Applied in `create_session`, `latest_for_dir`, `list_for_dir`,
plus a legacy fallback that matches pre-normalization rows (literal `"."`) by
normalized comparison. Regression test:
`store::tests::workspace_dot_and_absolute_share_sessions`.

Related: [[stuck-session-fix]] (same date — the session investigating
this stalled on loop-guard + filler final text, then its executor model
`opencode_go/iceman-contributor-free` returned 422 unavailable; session moved
to `opencode_go/beam-free` to continue).
- Full `cargo test`: 100/101 pass; sole failure
  `plugins::tests::discovers_layouts` is environmental/pre-existing (global
  `~/.config/lumina/plugins/opencode-zen.mjs` leaks into the test's count;
  passes with the global dir moved aside). Untouched by this fix.
- Side finding: daemon injects per-session `TMPDIR=/tmp/lumina-ses-<id>` into
  tool shells but the dir may not exist (cleaned on restart) → `cargo test`
  run by an agent fails in `tempfile::tempdir()` (seen in
  `checkpoints::tests::snapshot_restore_roundtrip`); passes with clean TMPDIR.
  Daemon should `create_dir_all` the injected TMPDIR.
- TMPDIR side finding resolved: `src/tools.rs` now `create_dir_all`s the injected
  per-session TMPDIR before running bash, so agent-run test suites (which rely on
  `TMPDIR`) work. Verified live via probe session (TMPDIR-OK).
