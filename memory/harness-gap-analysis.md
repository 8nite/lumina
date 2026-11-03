---
date_modified: 2026-11-02
tags: [strategy, competition, gaps, roadmap]
related: [[conversation-efficiency]], [[project]]
---

# Harness gap analysis (Nov 2026)

Star counts API-verified 2026-11-03 (rest from same-week roundups):
opencode 224k (MIT) · claude-code 154k (proprietary) · codex 133k
(Apache-2.0, Rust) · gemini-cli 107k (archived → closed Antigravity) ·
openhands ~91k · cline ~71k · goose ~56k · aider ~49k · continue ~35k ·
crush ~28k. Lumina is unlisted (private repo).

## At parity
Plan/build modes, 4-level permissions + glob rules, shadow-git
checkpoints + undo/redo, sessions (continue/fork/export), compaction,
skills/commands/subagents/plugins/hooks, MCP client, background tasks,
headless/CI mode, AGENTS.md hierarchy, prompt caching (just added),
usage accounting (just added).

## Ahead
Harness² self-improvement (no competitor has mutable-harness evolution);
Markdown+graphify RAG instead of vector DBs (validates: Graphify-Labs
itself is ~141k stars); single static Rust binary like codex.

## Missing — ranked
1. **OS sandbox (critical).** PARTIALLY DONE 2026-11-03 (`src/sandbox.rs`,
   `exec_bash` chokepoint in `src/tools.rs`, `[sandbox]` config): bubblewrap
   confinement for `bash` (foreground + background) — userns/pid/uts/ipc/
   net isolation, RO system, RW workspace/TMPDIR/toolchain caches,
   fail CLOSED (missing binary or namespace failure errors loudly, never
   runs unsandboxed), default OFF. Verified: argv unit tests, fail-closed
   tests, live confinement probe. THIS container blocks userns, so live
   confinement can't demo here — bwrap fails loudly as designed.
   Remaining: seccomp filters, macOS Seatbelt, per-command network policy.
2. **Real LSP (core).** DONE 2026-11-03 (`src/lsp.rs`, `src/tools.rs`,
   `src/agent.rs`, `src/config.rs`, `src/server.rs`, `src/main.rs`): stdio
   JSON-RPC client, one server per (workspace root, lang), ops
   symbols/definition/references/diagnostics (pull-first, push-cache
   fallback), UTF-16 positions, `[lsp]` config + kill-switch + command
   overrides, `[lsp] disabled` hides the tool. Live-tested vs fake server,
   typescript-language-server, and rust-analyzer (symbols/def/diags);
   dogfooded headless on this repo (correct fn list, 57k input tokens).
   Toolchains live in /tmp/opencode/lsp-{ts,ra} (~68MB).
3. **ACP support (core, emerging).** No Agent Client Protocol surface in
   `src/`. See "ACP vs MCP-serve" section below (2026-11-03 research).
4. **Act as MCP server (core).** Client-only (`src/mcp.rs` serve mode is a
   test fake). See "ACP vs MCP-serve" section below (2026-11-03 research).
5. **Conversation rewind (notable).** File undo exists; no conversation
   rollback (Claude `/rewind`, T3's linked revert). `session fork` is the
   manual escape hatch.
6. **Distribution (notable).** No IDE extension, desktop app, or mobile —
   every 50k+ star project has at least one surface beyond the terminal.
7. **Provider breadth (minor).** 5 protocols vs 75+ via Models.dev catalogs;
   no subscription-OAuth paths (Claude Pro/Max, ChatGPT plan, Copilot).
8. **Proof (minor).** No published Terminal-Bench-style score; competitors
   quote them (Codex 83.1%, Cursor CLI 79.3%).

## ACP vs MCP-serve (researched 2026-11-03)

**ACP** = open standard for editor→agent (Zed-origin, vendor-neutral
`agentclientprotocol` org, Apache-2.0, JSON-RPC over stdio; "what LSP did
for language tooling"). Purpose-built coding primitives: session
lifecycle, permission requests, diffs. Reuses MCP JSON shapes where it
can. 40–50 agents: native `opencode acp`, `gemini --acp`, `goose acp`,
`cursor-agent acp`, `kimi acp`, Cline/Kilo/Qwen/Copilot/Junie/Devin/
OpenHands/Hermes; adapters for Claude Code and Codex. Clients: Zed,
JetBrains (beta), Neovim (CodeCompanion, avante), Emacs, VS Code exts.
Official Rust SDK exists. For Lumina: new `src/acp.rs` reusing
Runner/Store/Registry + permission broker → ACP permission requests;
stdio child-process shape matches existing `run` model. This is the
cheapest fix for the IDE-distribution gap (no per-editor plugin).

**MCP-serve** = expose own tools so *other agents* can drive you
(`claude mcp serve`: ~28 tools over stdio, `isNonInteractiveSession`, no
passthrough of its own connected servers). For Lumina: new
`lumina mcp serve` reusing `src/mcp.rs` JSON-RPC framing in reverse;
expose raw tools (read/edit/bash/…) PLUS one `lumina_run` delegation
tool (full turn loop, summary back — keeps parent context clean, mirrors
our subagent pattern). Smaller build than ACP; serves multi-agent
pipelines (OpenHands, Claude Desktop-style orchestrators).

**Security asymmetry (both):** in serve modes our permission prompts do
not apply — the connecting client owns confirmation. Serve mode must
ship with an explicit policy ceiling (default deny-destructive) + full
trace logging, never inherit `--bypass`.

**Order:** ACP first (live client ecosystem today), MCP-serve second
(smaller, reuses `mcp.rs`).
