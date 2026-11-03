---
date_modified: 2026-11-03
tags: [lumina-memory]
---

KB is lazy and rule-based: read user request FIRST, use question tool when ambiguous, then at most one kb_query with request keywords budget 4000 and only when a KB is configured. KB dirs resolve per-workspace from AGENTS.md/CLAUDE.md/.lumina/rules/*.md mentions ([kb] memory_dir or LUMINA_MEMORY_DIR override wins), else legacy memory/ with .md, else skip with guidance (never invent). Writes go to the resolved dir post-completion only (memory_write_in + manifest stamp + best-effort graphify reflect). Harness RAG (harness::memory_dir) delegates to the same resolver; Library::promote stamps manifest + reflect; offline_pass also distills a lesson md. Skip note now tells the model to NOT call kb_query again that turn (re-query returns the same note). See also: [[rust-port]] [[memory-lazy]]
