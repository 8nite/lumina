---
date_modified: 2026-10-29
tags: [lumina, implementation]
---
# Lumina plan.md implementation (M0–M6)
Implemented the full plan: Go daemon (REST+SSE+PTY WS, OpenAPI 3.1), plan/build fence + 4-mode broker with credential/Google hard-deny, BYO-LLM router (OpenAI/Anthropic/Ollama), sessions (-c/-s/fork/export --sanitize), shadow-Git checkpoints (/undo/redo, GIT_INDEX_FILE, stale-lock recovery), plugins (Node v22 both signatures), kb (Go BFS/DFS/shortest-path/god-nodes, vocab<=12, wikilinks, singleflight sweeper), Harness² (Propose/Probe/Compose, quality floor, library), Bubble Tea TUI (Tab/Shift+Tab), React SPA (ModeBar, SSE timeline, PTY, CSP), MCP stdio/SSE, pair.
Verified: go test ./... (14 pkgs), pnpm test+build, live daemon e2e (CRUD/SSE/PTY/evolve).
See also: [[project]]
