# Changelog

All notable changes to `lumina` are tracked here (new file, started 2026-11-05).
Format follows Keep a Changelog; versions are `Cargo.toml`/`web/package.json`.

## [Unreleased]
### Added
- Harness deepcopy-relay forensics: live-state-first guidance (`tool_guidance.md`), close/exit finish rule (`system_prompt.md`), `deepcopy-relay` skill, `guardrails.yaml` close-evidence gate. Plan: `.lumina/plans/deepcopy-harness-fix.md`.

## [0.1.0] - 2026-11-05
### Noted
- Baseline: Rust single binary (`src/`) + React SPA (`web/`), axum daemon on `127.0.0.1:4089`, SQLite sessions, 4-mode permission broker, Harness² overlay (`.lumina/harness/`), Markdown + graphify KB.
- Prior history: see `git log --oneline` (auto-continue, work-gate, webui visibility, copilot models).
