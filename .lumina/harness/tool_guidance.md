# tool_guidance.md
# Managed by `lumina evolve` (Harness² Level 1). Additive edits only.

# Memory lazy-load (after intent)

- Read the user request FIRST before touching `./memory/` or graphify.
- Do NOT preload `./memory/` / graphify bodies before the user request is known (no query = no seeds = full cost, near-zero hit).
- After intent is known: one `kb_query` with request words, budget 4000; follow `[[wikilinks]]` only if needed.
- Never inject whole `memory/*` bodies into the system prompt; carry IDs/stubs.
- Workspace KB first: `<workspace>/memory` + workspace graphs outrank fallback corpora; fallbacks share at most ~30% of the budget and every hit cites `source_file`.
- Files vs live: file questions → native read/grep/glob/lsp; runtime/behavior questions → primary sources first (status APIs, logs, journals/DBs/stores found in code) via bash/webfetch; cite record IDs + timestamps; never present static inference as fact; never ask the user for tool-retrievable data.
