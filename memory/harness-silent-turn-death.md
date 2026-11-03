---
date_modified: 2026-11-02
tags: [lumina-memory, llm-streaming, silent-stop]
---

String::truncate on tool output panics mid-UTF8 (is_char_boundary) and kills the turn worker with no failure note; fixed with crate::tools::truncate_to (char-boundary floor) at 5 sites. No-progress gate releases now carry a NOT-done pointer instead of reading as done.

## Real cause of the "stops after 'now I'll …'" sessions (df1834c6, 2026-11-02)

The turn worker does not die: the LLM stream adapters dropped pending
tool calls and ended the turn on the model's promise text.

- `src/llm.rs` `chat_responses` (opencode_go protocol `openai-responses`):
  function calls accumulate in `pending` and were only flushed on
  `response.completed`. A gateway that closes after
  `response.output_item.done` (or after `[DONE]`) hit the EOF tail,
  which sent `done:true` and discarded `pending` → harness saw text
  only ("Switching to native file tools…") and accepted it as final.
  Fix: `flush_openai_calls(&mut pending, &tx)` on the terminal branches
  and at EOF.
- `src/llm.rs` `chat_openai`: `[DONE]` set `fr_done` and skipped the
  final flush; a chunk carrying both `tool_calls` and `finish_reason`
  hit `continue` before the finish check. Both lost the call.
  Fix: flush on `[DONE]` and on same-frame `finish_reason`.
- Tests: `responses_eof_flushes_pending_calls`,
  `openai_done_flushes_pending_calls`,
  `openai_same_frame_finish_flushes_pending_calls` (src/llm.rs).
- Symptom signature: transcript ends with an interim assistant line that
  promises the next tool ("Launching…", "Switching to…", "Testing it
  live") and `running:false`, no failure note, no stop row. Check the
  adapter before blaming the model.
- Note: the deployed `:4089` daemon must be rebuilt/restarted to pick
  this up (running binary predates the fix).

See [[harness-gap-analysis]], [[stuck-session-fix]], [[long-session-fixes]].
