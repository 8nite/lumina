---
date_modified: 2026-11-01
tags: [rules, config, conventions, security]
---

# `rules/` dir (was `instructions/`)

- Global/repo rule dir renamed `instructions/` → `rules/` (opencode v2 plural-dir convention: `agents/`, `commands/`, `skills/`).
- Sites: `src/rules.rs` discovery (global `~/.config/lumina/rules/`, repo `.lumina/rules/`), `src/security.rs` `is_tooling_doc` fence, `plan.md` + `README.md` docs and config comments.
- Tests: `rules_dir_loads_as_layer`, `tooling_docs_cover_rules_dir`.

See also: [[model-switch-ui]], [[project]]
