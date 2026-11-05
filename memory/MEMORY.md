---
date_modified: 2026-11-03
tags: [index, memory, lumina]
---

# Lumina Memory Index

- [[project]] — Core stack, client-server topology, Markdown + `graphify` RAG (`internal/kb`), Harness² self-improvement (`internal/harness`), and security/no-Google invariants.
- [[lumina-build]] — plan.md M0–M6 implementation record and Go + web verification gate.
- [[lumina-live]] — lumina Responses-only transport wiring and live-verified tool loop.
- [[subagents-websearch]] — Subagent/background/websearch tools and plan-fence + permission rules.
- [[verbose-streaming]] — Verbose run streaming (SSE `turn.delta`/`turn.tool`, TUI thinking + tool steps, web pills) and steer/queue inbox.
- [[prompt-e2e]] — Backend system prompt (`toolGuidance`), SSE fragment-accumulation fix, and BTC E2E (websearch→webfetch fallback, subagent cross-check, background monitor).
- [[long-session-fixes]] — Long-session hardening (prompt single-flight, terminal SSE + reconnect, permission modal, context gauge, TUI space key).
- [[context-from-providers]] — Provider-sourced context windows (`ModelMeta.context_window`, `Router.ContextWindow` cache) feeding the gauge.
- [[prompts-parity]] — Opencode-v2 prompt/tool parity fixes (conditional tool guidance, env/date block, schemas, fence).
- [[config-defaults-fix]] — `[defaults]` agent/permission_mode seeding TUI + headless runs (CLI flags still win).
- [[browser-skills]] — agent-browser skill port (`bundle.Skills` ad gap, explicit load).
- [[vault-placeholders]] — session vault + `{placeholder_xxx}` remap, detector families, layer-0 guidance, widened credential hard-deny.
- [[resync-variants]] — session state resync endpoint + TUI rebuild, effort variant picker, identical-round loop guard.
- [[loops-and-claims]] — per-call repeat ledger (3rd identical call) + cross-daemon turn claim (`inflight_owner`, 12 min stale takeover, 409).
- [[rust-port]] — single-binary Rust port (M0/M1/M2): axum server + broker/hard-deny, BYO-LLM router, tool registry, native `graph.json` KB (`kb/query|update|reflect|status`), compat flags.
- [[rust-migration]] — Go tree deleted from `rust-revamp` (2026-11-01): port-if-seen rule, pre-delete baselines, post-delete verification before merge.
- [[web-ui-sticky-scroll]] — SessionView sticky-bottom autoscroll (80px threshold, transcript + working card pin, respects manual scroll-up).
- [[opencode-zen-models]] — Opencode zen `/models` schema (`limit.context` window, `capabilities.reasoning.effort` ladder), parsed in `src/llm.rs`, live-verified on 28 models.
- [[model-picker-filter]] — Composer model menu filter (substring match), App default model label fallback, and applyModel `setSession` refresh.
- [[model-switch-ui]] — WebUI `applyModel` `setSession(patched)` fix (header button went stale); live-verified `iceman` → `muse-spark-1.3`.
- [[opencode-zen-plugin]] — `models.list` hook + bundled `plugins/opencode-zen.mjs` (Zen catalog merge via `canonical_id` twins); core keeps generic parse only.
- [[zen-twin-suppression]] — Zen/Go twin suppression (endpoint-bound ids): list enriched Go id, suppress unusable Zen twin; live 55 servable models.
- [[rules-dir]] — Global/repo rule dir renamed `instructions/` → `rules/` (opencode v2 plural-dir convention); `src/rules.rs` discovery, `security.rs` tooling-doc fence, tests.
- [[effort-stepdown]] — `chat_responses`/`chat_openai` retry down the effort ladder on effort-rejection 400s (`effort_step_down` + `is_effort_error`); live-verified `iceman` max → OK.
- [[effort-ladder]] — Canonical effort ladder de-hardcoded (`none`→`max` union; verbatim advertised passthrough, per-model pickers, TUI 8-step `EFFORTS`).
- [[session-resume-workspace]] — Web/CLI session resume mismatch (`.` vs absolute `workspace_dir`); `normalize_workspace()` in `src/store.rs` + legacy `.` fallback, regression test.
- [[stuck-session-fix]] — Stuck-session fix (56d41a74): `read_repeat_nudge` (4th same-path read), SSE channel 256→1024, `:4089` owned by ai-manager — do not kill; + no-progress gate, TurnRelease guard, drive_one retry, request-id prompt idempotency (6ad0f21); + follow-up task inheritance + 12-round idle nudge (7614829e continue spin); + subagent budget 8 / proxy-read ledger / mid-loop verify prod (cc686408, uncommitted).
- [[loop-harness-audit]] — Turn-loop vs reference-agent audit (12 fixes: atomic compaction, turn-reuse retry, loop guards, SSE seq ids, idempotency + steer inbox).
- [[harness-silent-turn-death]] — `String::truncate` mid-UTF8 panic killing the turn worker silently; fixed with `crate::tools::truncate_to` (char-boundary floor) at 5 sites, no-progress releases now carry a NOT-done pointer.
- [[kb-lazy-resolution]] — Lazy rule-based KB resolution (request-first, one `kb_query`, per-workspace dir resolver, post-completion writes).
- [[todo-pill-3of3]] — TodoPill 3/3 fix (match user-role `[tool result todowrite]` turns, not just role tool; `SessionView.tsx` `isToolText` mirror).
- [[conversation-efficiency]] — Conversation-log efficiency (opencode comparison): Anthropic/OpenAI prompt caching + `cache_read/write` usage, `sessions` token columns, nudge tax cut, thinking write-only, 60% compaction trigger, `tool_full` spill, parallel pure-read batches.
- [[harness-gap-analysis]] — Harness gap analysis (Nov 2026): at-parity/ahead map (Harness², graphify RAG, static Rust binary), 8 ranked gaps (OS sandbox critical → benchmark proof minor).
- [[over-exploration-fix]] — Over-exploration loop guards (de530754 audit: 105 turns for ~8 reads): identical-output ledger, todowrite 4th-advisory/6th-block budget, identical-text streak forced answer, reset on edit/write/patch.
- [[webui-tables]] — WebUI pipe tables rendered as literal text: `LinkifiedText.tsx` GFM table grammar (`splitRow` code-span-aware split, `delimAlign`, strict header/delimiter match, ragged-row pad/truncate); `tsc` + `vite build` green.
- [[webui-background-visibility]] — Background-turn visibility (3bbace61 gap): daemon live-turn set overlays `running` on session views, sidebar pulsing dot, `App.tsx` completion toasts + tab title flash for unviewed sessions.
- [[cache-hits]] — Prefix-cache hit rates per session (60-71% steady-state, compact costs one full-price turn, subagents start ice-cold, model swap resets namespace).
- [[zero-google-removal-2026-11-05]] — Zero-Google guardrail layer removed (dead weight, operator uses no Google services): `is_google_url/target` + endpoint/env/cmd checks + server 403s + MCP/fetch blocks + 6 tests gone; credential denies kept. `plan.md`/`test-plan.md` left historical.
