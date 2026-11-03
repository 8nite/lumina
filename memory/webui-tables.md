---
date_modified: 2026-11-03
tags: [webui, markdown, tables, bug]
related: [[project]]
---

# WebUI pipe tables rendered as literal text

`LinkifiedText.tsx` (the only chat-markdown renderer, used by SessionView)
supported headers/lists/code/bold/links but had no table grammar, so model
output like `1. Rules` + `| opencode | lumina | same? |` + delimiter row
showed raw pipes (seen in session de530754).

Fix (`web/src/components/LinkifiedText.tsx`): GFM table blocks —
`splitRow` (code-span-aware pipe split, `\|` escapes, optional outer
pipes), `delimAlign` (`:---`/`:---:`/`---:`), strict header/delimiter
column match so `a | b` prose stays a paragraph, ragged body rows padded /
truncated, cells go through `renderInline`. Verified: `tsc --noEmit` +
`vite build` green; compiled-module unit asserts on the real code (header,
delim, alignment, code-span pipes, non-delim rejection); `renderToStatic-
Markup` of the exact failing shape yields `<table>` + thead + 2 rows.
No new deps (repo rule).

See [[web-ui-sticky-scroll]], [[todo-pill-3of3]].
