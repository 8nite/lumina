---
date_modified: 2026-11-05
tags: [opencode-parity, system-prompt, permissions, subagents, steps]
---

# OpenCode parity copied into Rust harness (2026-11-05)

Copied 7 OpenCode v2 behaviors into `src/` (272-line diff, `cargo test` 207 green, `cargo clippy --tests` zero warnings):

- **Env/date block** (`src/agent.rs:env_block`): `Today's date` + `<env>` (workspace, git yes/no, `OS-ARCH`, tmp) prepended to every system prompt. Rust had dropped the Go `envBlock`.
- **Per-agent tool specs** (`src/agent.rs:tool_specs_filtered`): plan hides `edit/write/patch/todowrite/subagent/mcp` at the spec level (fence still denies); `patch` advertised only to GPT-family models (`supports_patch`).
- **Durable `allow_always`** (`src/approve.rs:record_allow_always/load_allow_always`, `src/server.rs:answer_permission`, seeded in `src/agent.rs:run_prompt`): project-scoped `.lumina/permissions.json`; never overrides deny (decide runs pre-gate).
- **Parallel web/polls** (`src/agent.rs:is_parallel_tool`): `webfetch`, `websearch`, `task_output` join the concurrent read batch.
- **Subagent `sessionID` continue** (`src/tools.rs:child_sessions/do_subagent`): first call returns `(subagent session: <id>)`; follow-up folds prior result into the prompt. General children also lose `question` (was already denied by policy intent).
- **Per-agent `steps:`** (`src/rules.rs:AgentDef.steps`, honored in `run_prompt` + `run_subtask`): final step strips tools and asks for a text summary.
- **Verify-gate artifact fix** (2026-11-05, `src/agent.rs:runs_written_artifact`): a successful bash that directly executes a file the turn wrote now counts as verification. Retest session `c5d6f1e6`: 8 turns, ~50k in / 1.4k out, exit 0, no spin (was 34 turns / ~181k in, spinning).
- **Small-app E2E evidence** (session `3a8927e4`, since cleaned): `write hello.py` + `bash python3 hello.py` completed in 2 tool rounds, but the verify gate never counts a plain script run as verification → `UNVERIFIED_POINTER` + 2 auto-continues, 34 turns, ~181k input tokens for a done task. OpenCode has no such gate. If the gate stays, `is_verify_command` should accept a successful run of the just-written artifact.
- **CLI papercut found by the same run**: `lumina run ... "prompt" --format text` swallowed `--format` into the prompt (`src/main.rs:284-286` `prompt.extend(rest)`); flags must precede the prompt.

See also: [[prompts-parity]], [[prompt-e2e]]
