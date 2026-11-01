---
date_modified: 2026-11-01
tags: [web-ui, sessionview, scroll, stick-to-bottom]
---

# WebUI sticky-bottom scroll

Done: sticky-bottom autoscroll in `web/src/components/SessionView.tsx` — conversation sticks to newest while streaming, respects manual scroll-up. Transcript div tracks near-bottom (<80px) via stickRef; auto-sticks on new turns/liveText/liveThinking/liveSteps/working/error/session-switch. Working card (`max-h-38vh`) has its own ref and also pins. `tsc` + `vite build` pass.

See also: [[long-session-fixes]], [[verbose-streaming]]
