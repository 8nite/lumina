---
date_modified: 2026-10-30
tags: [opencode-parity, system-prompt, subagent, background-tasks, websearch, e2e, interop]
---

# opencode System Prompt + BTC E2E (websearch + subagent + background)

- **opencode v2 system prompt** (MIT, `anomalyco/opencode`, `packages/core/src/session/runner/prompt/system.txt` + `system-prompt.ts`): short base (`You are an AI agent running in OpenCode...`, `# Harness` with Markdown/`<system-reminder>`/parallel-calls + `${OPENCODE_TOOL_GUIDANCE}`, `# Communication`, `# Working in codebases`) plus generated per-tool lines (shell: prefer dedicated tools, no chained `echo "===="` separators; write-vs-edit; edit `old_string` uniqueness). Adapted into `internal/agent/agent.go:toolGuidance` (with `[lumina]` additions: websearch-key fallback to webfetch, subagent/background+`task_output` polling, kb/memory/skill pointers, gather-all-results rule). Wired into `RunPrompt` (replaces ad-hoc bullets) and subagent child systems.
- **E2E test** (`/tmp/btctest`, `lumina run --bypass -m copilot/gpt-4o`, prompt: web price → explore-subagent cross-check → background $1-move monitor → polled reply). Result: reply gathered all three (Coinbase/Kraken web price, subagent CoinGecko cross-check + investing.com 403, background monitor status). No websearch provider keys exist in this env, so `websearch` correctly reports "no provider" and the model falls back to `webfetch` per the guidance.
- **Interop bug found by the test:** `internal/llm/openai_compat.go` emitted one broken tool call per SSE fragment (empty name, nil args → `unknown tool ""`, `websearch: missing query`). Copilot streams id+name first, argument substrings after. Fixed: accumulate fragments by `index`, flush complete calls on `finish_reason` (Anthropic/Responses adapters already did this). Regression test `internal/llm/fragment_test.go`.
- **Robustness fixes from the test:**
  - Subagent delegation prompts truncated (6000/2000 chars) so a pasted dump can't blow the child context window (child system is intentionally lean: no global bundle).
  - opencode `steps` parity: when the 8-round cap is hit with pending calls, one final no-tools round asks the model to summarize, so the turn ends with a substantive gathered reply instead of `""`.
- **Caveats observed:** `copilot/gpt-4o` + `gpt-4.1` work on this account; newer `claude-sonnet-5`/`gpt-5.6-*` IDs return `model_not_supported`; Copilot 429s under burst load; `api.coindesk.com` does not resolve from this network (use coinbase/kraken/coingecko); `investing.com` returns 403 to bots (subagent reported it honestly). Google cross-check skipped per repo zero-Google hard-deny posture.
