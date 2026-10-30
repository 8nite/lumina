---
date_modified: 2026-10-30
tags: [system-prompt, opencode-parity, tool-guidance, permissions]
---

# Opencode-v2 prompt/tool parity fixes

Closed the audit gaps vs `anomalyco/opencode@dev` (`packages/core/src/session/*`, `tool/plugin/*`):

- **Conditional tool guidance** (`internal/agent/agent.go:renderToolGuidance`): Harness shell/write/edit lines and Basic/Lumina sections render only for tools in the model-visible spec list — mirrors `session/system-prompt.ts:render()`. Explore-like children no longer see edit/delegation guidance.
- **Env/date block** (`internal/agent/agent.go:envBlock`): `Today's date` + `<env>` (workspace, git yes/no, `runtime.GOOS`, tmp preference) injected in `RunPrompt` — mirrors `instructions/builtins.ts`.
- **Schemas** (`internal/agent/agent.go:toolSpecs`): `bash.workdir`, `grep` include/literal/caseSensitive/limit, `glob` path/limit, `subagent.model`, year-aware `websearch`, new `question` + `todowrite` specs.
- **Tools** (`internal/tools/tools.go`): `read` returns `line: content` (limit 2000, dir listing, binary guard); `grep` regex literal/insensitive/include; `glob` `**` via `globToRegExp`; `bash` workdir confined to workspace; `webfetch` per-hop redirect checks + HTML→text; `subagent` model override + `sessionID` loud reject + `SubagentMaxDepth` (`LUMINA_SUBAGENT_DEPTH`); `question` (unwired-safe) + `todowrite` (per-session store).
- **Fence** (`internal/security/security.go`): plan allows `question`, denies `todowrite`; `question` allowed in all build modes; `todowrite` treated as edit-class. `plan.md` fence row updated (`ask_question` → `question`).

Kept deliberately divergent from opencode: plan `bash` stays read-only even under bypass; background notification stays poll-via-`task_output` (no push runtime yet); no child-session follow-ups.

## 2026-10-30 — question UI wiring + patch tool

- **Question round-trip is live end-to-end:** model `question` → daemon `question.asked` SSE → TUI card (number/free-text answers, `esc` dismiss) or web modal (per-question option buttons + custom text) → `POST /api/v1/questions/{id}` → formatted `"q"="a"` reply. Headless `run` (no UI) still degrades to best-guess guidance, never hangs. Runner hook `AskQuestion`, `tools.ErrQuestionUnwired` sentinel, `RequestQuestion`/`handleQuestions`, SDK `AnswerQuestion`/`DismissQuestion`, `question.asked/resolved` events + OpenAPI entry.
- **Patch tool added** (`internal/tools/patch.go`): opencode `*** Begin Patch ***` envelope (Add/Update/Delete + `*** Move to:`, `@@` hunks, `*** End of File`, heredoc strip, 4-level seek fallback). Edit-class permissions, plan-scoped per-file enforcement in `doPatch`, partial-progress errors. Spec + `# Harness` guidance line + explore exclusion.
- Tests: `patch_test.go` (add/update/delete + 8 reject paths incl. plan fence + partial progress), server question round-trip + dismiss, TUI QModal keys/parse/event/view, specs advertise question/todowrite/patch.

See also: [[prompt-e2e]], [[subagents-websearch]]
