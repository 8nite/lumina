# Harness generality fix plan (evidence: Lumina 5bc43142 vs OpenCode ses_e5476de2)

> Evidence only — fixes are domain-agnostic. Same prompt, same workspace (`/home/herbert/trading-dashboard`).
> Lumina `5bc43142` (41 turns): static reads only, no primary-source query, speculative answer, stopped by user.
> OpenCode `ses_e5476de2`: 3 doc reads then primary sources (status API, logs, journal DB), answered with record citations.
> The DeepCopy/relay/Mongo details below are examples of the general classes, not fix content.

## 1. Generic root causes (harness code)

### R1. `grep`/`glob` ignore gaps — `src/tools.rs:769-987`
- `do_grep walk:815-830,837-838` + `do_glob walk2:939-952` skip only `/.git/` + `/node_modules/`.
- Missing: `venv/ .venv/ __pycache__/ target/ dist/ .next/ .turbo/` + `.gitignore` respect.
- General defect, not prompt-specific: any broad pattern in a monorepo with vendored envs returns noise and buries signal.

### R2. KB fallback outranks workspace — `src/kb.rs`, `src/agent.rs:337-340,208`
- Query returned a non-workspace corpus while workspace `memory/` held the relevant notes.
- General defect: no workspace-first ordering and no cap on fallback corpora, so context pollutes before any tool runs.

### R3. No primary-source-first doctrine — `src/agent.rs:199-213`
- `system_prompt` covers native-tool choice + lazy KB, but never states: `questions about runtime behavior → query primary sources first (live API / logs / journal DB / store / status endpoints discovered from workspace code), cite record IDs + timestamps; never answer from docs/memory/static alone`.
- Per-workspace `AGENTS.md` cannot be relied on for this; it must be a code-level fence.

### R4. Bash guidance over-deterrence — `src/agent.rs:208`, tool specs `:150-197`
- `use native tools, not bash equivalents — read (not cat), grep (not rg), glob (not find/ls)` generalizes to avoiding `bash` entirely (1× `ls` in 41 turns).
- General defect: no files-vs-live distinction. File inspection → native tools; runtime/live inspection (processes, network, cluster, DB read-only CLIs, status HTTP) → `bash` is required.

### R5. Info-seeking spin not gated — `src/agent.rs:399-526`
- Progress/idle/re-read/output/text ledgers arm on `work_requested` (edit intent). Pure `help chk why…` questions have `work_requested=false` → exempt.
- General defect: any diagnostic turn can loop on static reads + filler + re-reads and end speculative without ever touching a primary source.

## 2. Generic harness fixes (no workspace/domain edits)

### F1. Ignore-aware grep/glob (`src/tools.rs`)
- Extend `walk`/`walk2` skip set: `.git, node_modules, venv, .venv, __pycache__, target, dist, .next, .turbo` (keep binary + credential skips).
- Unit test with fixture tree containing env-noise files; assert zero hits from skipped dirs, `limit` behavior unchanged.
- New dep (e.g. `ignore` crate) only if the hand-rolled test proves insufficient. Update the `ponytail:` comment with the ceiling.

### F2. Workspace-first KB (`src/kb.rs`, wiring `src/agent.rs:337-340`)
- Order: (1) `<workspace>/memory/*.md` + workspace graph outs, then (2) fallback corpora only with remaining budget, capped (e.g. ≤30%).
- Every hit carries `source_file` (+ `source_location` when known).
- Unit test: workspace note outranks fallback corpus for the same keywords.

### F3. Primary-source-first fence (`src/agent.rs:199-213` + `.lumina/harness/tool_guidance.md`)
- Add to `Runner::system_prompt` and harness `tool_guidance.md` default:
  `Runtime/behavior questions: identify primary sources from workspace code/routes first (status APIs, logs, journals/DBs, stores), query them before concluding; cite record IDs + timestamps; never present docs/memory/static inference as fact; never ask the user for data a tool can retrieve.`
- No domain names, ticket fields, collection names, or endpoint paths in the fence. Secret-safety stays via existing `VAULT_GUIDANCE`.

### F4. Files-vs-live tool guidance (`src/agent.rs:150-208`)
- Rewrite to: `files → native read/grep/glob (never bash cat/rg/find/ls); live/runtime → bash + webfetch required (log tails, status HTTP, read-only DB/journal queries discovered from code)`.
- `bash` ToolSpec gets one generic live example per class (log grep, status curl, read-only DB query via secret-safe env indirection) without naming any product, namespace, or collection.

### F5. Info-seeking progress gate (`src/agent.rs:440-528`)
- New `looks_like_diagnostic` (separate from `looks_like_work`): question words about behavior/state at a time (`why/did…close/hold/show/mismatch`, timestamps) with no edit intent.
- After N=2 consecutive static-only rounds (only read/grep/glob/kb_query, zero bash-live/webfetch-live/primary-store calls), inject one-shot nudge: `name the exact primary-source query next and run it now; no concluding answer without primary-record citations`.
- Block closing answers that request from the user data retrievable via tools, or that state findings without record citations, when no primary query was attempted. No domain keywords in the gate.

## 3. Verification (generic)
- `cargo test` green + `cargo clippy --tests` zero new warnings.
- New tests: (a) ignore fixture; (b) KB workspace-priority fixture; (c) prompt-contains-fence test (`plan_system_prompt_fences`-style at `src/agent.rs:2927`); (d) gate test: 2 static rounds on a diagnostic turn → nudge fires; speculative close without primary call is refused.
- Manual replay on an unrelated workspace/incident: first primary-source query within ≤4 tool calls; final answer cites primary records (IDs + timestamps), not docs.

## 4. Non-goals
- No product/domain logic, no endpoint/collection/namespace names in code, no workspace edits, no journal/DB writes, no policy/threshold changes, no credential reads, no new deps unless the ignore test forces it.
- DeepCopy session pair stays as evidence only.
