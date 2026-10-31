---
date_modified: 2026-10-31
tags: [skills, browser, agent-browser, plugins, tui]
---

# Browser Skills (agent-browser)

Ported `agent-browser` from `~/.claude/skills/agent-browser` (opencode skills) to `.lumina/skills/agent-browser/SKILL.md` with `references/` and `templates/`.
Verified with `lumina run 'Load skill named agent-browser...'` reporting Thornhill weather via `bash agent-browser open wttr.in`.
Fixed 2026-10-31: `bundle.Skills` now advertised in system prompt per plan.md §6.1 — `skillsBlock` (name + description) in `RunPrompt` + subagent systems, `withSkillList` annotates the `skill` tool description. Live-verified: `lumina run 'list your skill names'` returns `agent-browser` (was: only ambient harness skills). `websearch` no-provider error now suggests the `webfetch` fallback inline.
See also: [[project]], [[prompts-parity]]
Promoted agent-browser to global ~/.config/lumina/skills/agent-browser/ (SKILL.md+references+templates) so all workspaces use it via import_global_skills. Verified global load. websearch has no provider (no TAVILY/EXA/FIRECRAWL/PARALLEL/TINYFISH keys) – agent auto-falls back to webfetch Open-Meteo which works keyless for weather.
