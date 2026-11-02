---
date_modified: 2026-11-02
tags: [lumina-memory]
---

String::truncate on tool output panics mid-UTF8 (is_char_boundary) and kills the turn worker with no failure note; fixed with crate::tools::truncate_to (char-boundary floor) at 5 sites. No-progress gate releases now carry a NOT-done pointer instead of reading as done.
