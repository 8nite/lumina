---
date_modified: 2026-10-30
tags: [config, defaults, permissions, agent-modes, lumina]
---

# Config defaults seed flags

- `[defaults]` `agent` / `permission_mode` in `config.toml` (`LUMINA_CONFIG` or `~/.config/lumina/config.toml`) now seed TUI and headless `run` flags via `configBaseFlags()` (`cmd/lumina/main.go`).
- Precedence: config file seeds base; explicit CLI flags (`--agent`, `--auto-edit`, etc.) still win; missing/unreadable config falls back to `build`/`ask`.
- Bypass-in-config starts sessions in bypass (still never bypasses credential-file blocks or No-Google rules).
- Covered by `TestConfigDefaultsSeedFlags`.
See also: [[project]], [[prompts-parity]]
