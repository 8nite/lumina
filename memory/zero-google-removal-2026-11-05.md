---
date_modified: 2026-11-05
tags: [security, guardrails, config, cleanup]
---

# Zero-Google guardrail removal

Operator does not use Google services: the whole zero-Google enforcement layer was dead weight, removed `de7ce53` (+ `--from-literal` deny `b0d6f24`).

- Removed: `is_google_url()` (`src/config.rs`), `is_google_target()` (`src/security.rs`), endpoint/token_url/env/cmd checks, 2× `403 Google endpoints blocked` in `src/server.rs`, MCP dial check, webfetch fetch/redirect checks, `IMMUTABLE_SECURITY_CORE` Google line, 6 tests. `+26/−169` over 9 files.
- Kept (not Google-specific): credential-file hard-denies, secret redaction, `--from-literal` deny at first (then removed per operator direction `b0d6f24` with its test + unused `eval` helper).
- Docs: `AGENTS.md` + `README.md` lines updated; `plan.md` / `test-plan.md` left historical (still describe the old rule).
- Verified: repo-wide token scan clean; `cargo test` 198 pass; `cargo clippy --tests` clean.

See also: [[project]], [[vault-placeholders]], [[subagents-websearch]]
