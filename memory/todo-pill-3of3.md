---
date_modified: 2026-11-02
tags: [lumina-memory]
---

TodoPill never reached 3/3: it scanned only role tool/tool_error, but the agent persists tool outputs as role user with [tool result todowrite] prefix, so every turn.completed reconcile (export replaces live rows) wiped the pills source. Fixed web/src/components/SessionView.tsx to also match user turns with isToolText, mirroring the timelines isToolTurn. Tests: tools::todos_progress_reaches_3_of_3 (1/3->2/3->3/3 output shape, no stale In progress tail) and store::transcript_json_preserves_todowrite_progress_to_3_of_3 (transcript keeps all three prefixed user turns in order). See also: [[stuck-session-fix]]
