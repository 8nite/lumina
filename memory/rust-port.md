---
date_modified: 2026-10-31
tags: [lumina, rust-port, kb, server, agent-loop]
---

# Rust port (M0/M1/M2)

Single-binary Rust port alongside the Go daemon (`src/main.rs` `mod agent|config|kb|llm|rules|security|server|store|tools`). `make build` unchanged; verify with `CGO_ENABLED=0 go test ./...` plus `cargo test` for the Rust side.

- M0 scaffold: config (`src/config.rs`), plan/build fence + 4-mode broker + credential/Google hard-deny (`src/security.rs`: `is_credential_path`, `is_google_target`, `plan_allows_write`, layer-0 deny first in `decide`), rules discovery (`src/rules.rs`: `workspace_root`, `discover`, frontmatter parse, `builtin_agents`, `Bundle`), axum server skeleton (`src/server.rs`).
- M1 agent loop: BYO-LLM router (`src/llm.rs`: `Router`, `split_model`, `normalize_effort`/`valid_effort`/`effort_budget`, `ModelMeta`), session store (`src/store.rs`), tool registry (`src/tools.rs`: `read|edit|write|bash|grep|glob|kb_query|memory_write|skill`; `..` traversal denied, args aliases `path|file|target` and `pattern|query`), agent runner (`src/agent.rs` `Runner::run_prompt` with compat tuple).
- M2 KB + APIs: `src/kb.rs` mirrors `internal/kb` with zero vector DBs — native `graph.json` traversal (BFS/DFS/path/explain/god-nodes, <2ms), constrained-vocab expansion, recursive `[[wikilink]]` resolution (`wikilinks`, `resolve_wikilinks`), dedup-on-write `memory_write`, `manifest_stamp`, cold-start `reflect`, singleflight `Sweeper`. CLI: `lumina kb [query|update|reflect|status]` (`src/main.rs` `cmd_kb`, dual graph paths `memory/graphify-out/graph.json` + `graphify-out/graph.json`). Server: session CRUD/fork/export/revert, `files`, `git/diff|branch`, `providers|router|models`, `kb/query|browse`, compat flags `(import_global_rules, import_global_skills)` threaded through `AppState` and `run_prompt`.
- Ops note: hyperedge `nodes[]` coerced on load, confidence defaulted, degenerate <2-node edges shrink-guarded (`Graph::load`).

See also: [[project]], [[lumina-build]], [[lumina-live]], [[vault-placeholders]], [[loops-and-claims]]
