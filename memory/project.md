---
date_modified: 2026-10-30
tags: [architecture, lumina, harness2, graphify, plugins, permissions, sessions]
---

# Lumina Project Architecture

- **Repo:** `8nite/lumina` (blueprint in [[plan]]).
- **Stack:** Pure-Go (`CGO_ENABLED=0`) Go 1.22+ single-binary daemon (`lumina serve`) + Bubble Tea TUI + embedded React 19 / TypeScript / Vite / Tailwind Web UI (`//go:embed`) + pure-Go SQLite (`modernc.org/sqlite`) for session/turn-tree/trace state.
- **Lumina Parity (Rules, Plugins, Modes, Sessions):**
  - **Rules & Extensions (`internal/rules`):** Merges global `~/.config/lumina/AGENTS.md` + `instructions/*.md` (with optional read-only `~/.claude/CLAUDE.md` import) + repo/subdir `AGENTS.md`, plus lazy `skills/<name>/SKILL.md`, `agents/*.md`, and `commands/*.md`.
  - **Plugins (`internal/plugins`):** On-demand Node v22 worker (`host.mjs`) supporting both factory/return-object and v2 `setup(ctx)` JS/TS plugins (`chat.message`, `chat.messages.transform`, `tool.execute.before/after`, `shell.env`, `permission.ask`, `event`).
  - **Primary Agent Modes (`Tab`):** Built-in `plan` (hard Go-level read-only codebase fence allowing only `plan.md` / `.lumina/plans/*.md` writes, even under `--bypass`, with optional `router.planner` model and 1-click `Plan -> Build` handoff) and `build` (full execution), plus custom `agents/*.md` (`mode: primary`).
  - **Approval Modes (`Shift+Tab`, `internal/security`):** Orthogonal live-switchable `ask`, `auto-edit`, `auto-approve` (`-y`), and `bypass` (`--bypass`) + granular command/path globs + `allow_session`, while **never** bypassing credential-file blocks or No-Google rules.
  - **Sessions (`internal/agent`):** `-c`/`--continue` (latest workspace session), `-s <id>`, `/fork`, headless `lumina run`, `session export --sanitize`, shadow-Git `/undo` & `/redo`, auto-compaction, per-session `TMPDIR`, and built-in `model-persist`.
- **RAG & Memory (`internal/kb`):** Plain Markdown (`./memory/*.md` + `MEMORY.md` + `[[wikilinks]]`) + **`graphify`** knowledge graphs (`graph.json`, `GRAPH_REPORT.md`, `LESSONS.md`, `graph.html`) across `memory`, `workspace` (0-token AST + docs), and `harness2` `library`, with `<2ms` Go BFS/DFS traversal and singleflight `standing-ops` sweeper.
- **Self-Improvement (`internal/harness`):** `google-research/harness2` adapted locally — 8 mutable `.lumina/harness/` components on `refs/lumina/harness`, `Propose -> Probe -> Compose` in `git worktree` sandboxes, and `graphify`-indexed Improvement Library.
- **Hard Invariants:** Zero Google services/SDKs/fonts/backends (`gemini` & `--google-workspace` blocked); secrets strictly via `api_key_env` / `api_key_cmd` indirection with 2-layer secret redaction.
