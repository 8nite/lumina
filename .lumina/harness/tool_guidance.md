# tool_guidance.md
# Managed by `lumina evolve` (Harness² Level 1). Additive edits only.

# Memory lazy-load (after intent)

- Read the user request FIRST before touching `./memory/` or graphify.
- Do NOT preload `./memory/` / graphify bodies before the user request is known (no query = no seeds = full cost, near-zero hit).
- After intent is known: one `kb_query` with request words, budget 4000; follow `[[wikilinks]]` only if needed.
- Never inject whole `memory/*` bodies into the system prompt; carry IDs/stubs.

# Live-state first for close/exit questions (2026-11-05 deepcopy lesson)

- For `why did X close` / `why still open in master` / `mirror vs own exit`: run live checks via `bash` BEFORE theorizing — `curl .../api/relay/status`, `kubectl logs -n mt4 deploy/mt4-safe-algo --since 48h` (+ `--previous` after restarts), Mongo `relay_copies` / `relay_commands` for the ticket window.
- Static `DetailedStatement*.htm` older than the incident timestamp is stale history only, never the close proof.
- `bash` reservation: `kubectl` / `mongo` / `pymongo` / `curl` to live services are NOT native-tool equivalents; the read/grep/glob nudge never applies to them.
- Glob hygiene: exclude `venv/`, `.venv/`, `node_modules/`, `__pycache__/` on broad searches (prefer `glob **/copyEngine.ts` over tree-wide `grep copy`).
- Close proof required: ticket pair (`DC<master>` comment + local ticket) + `relay_copies.closeReason` + `closedAt` before answering `EQUITY STOP` vs `mirror master closed` vs own TP/SL/time.
