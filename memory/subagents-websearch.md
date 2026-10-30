---
date_modified: 2026-10-30
tags: [agents, subagents, background-tasks, websearch, webfetch, opencode-parity]
---

# Subagents, Background Tasks & Web Search (opencode parity)

- **Source of truth:** `https://opencode.ai/v2/docs/agents`, `/websearch`, `/tools`, `/permissions` (opencode V2 docs; semantics reimplemented, no sources copied). Provider REST shapes verified against Tavily/Exa/Firecrawl/Parallel/TinyFish API references.
- **Tools (`internal/tools`, `toolSpecs` in `internal/agent/agent.go`):**
  - `subagent {agent, description, prompt, background}` — child session with a configured subagent; `background:true` detaches (poll with `task_output`, completion fires `OnBackgroundDone` → `turn.progress` SSE).
  - `bash {command, background, timeout}` — `background:true` detaches (no timeout unless supplied); foreground keeps the 2-min cap.
  - `task_output {task_id}` — polls a background `bash`/`subagent` task (full output retained, 30KB poll cap).
  - `webfetch {url, format}` — http(s) only, 100KB cap, binary rejected.
  - `websearch {query}` — providers `tavily | exa | firecrawl | parallel | tinyfish` via `*_API_KEY` env; `[websearch] provider = "random"` fans out over available keys; 429 → per-provider 60s/`Retry-After` cooldown, all-cooling fails fast; `disabled = true` removes the tool from model requests.
- **Runner (`internal/agent`):** `runSubagentText` resolves custom `agents/*.md` + builtins (`general`, `explore` in `internal/rules`), enforces `mode: subagent|all` and **depth 1** (children never get the `subagent` tool + `SecCtx.SubagentDepth` guard); `explore` gets the read-only envelope (reads + `bash` + web, no edits); child inherits workspace/model/permission, never touches the parent turn tree.
- **Fence (`internal/security`):** `webfetch`/`websearch`/`task_output` are read tools (allowed in `plan` too, Google hard-deny still wins); `subagent` is **denied in plan** (a build child editing under a plan parent would bypass the read-only fence) and `ask`-gated in build (allowed under bypass); permission prompts use agent ID / URL / query as the resource (see `[[project]]` permission broker).
- **Config (`internal/config`, wired in `cmd/lumina/main.go:loadStack`):** `[websearch] provider = ""|random|exa|firecrawl|parallel|tavily|tinyfish`, `disabled = false`; validated + hot-reloaded.
- **Tests:** `security/web_tools_test.go`, `tools/web_subagent_test.go` (no network: bad-scheme fetch, disabled/missing-key search, bash + subagent background lifecycles, depth deny), `agent/subagent_test.go` (specs, explore envelope, unknown-agent error), `config/websearch_test.go`. Full `CGO_ENABLED=0 go test ./...` green.
- **Limits (do not regress):** child inherits the *parent's* permission mode (per-agent `permissions` frontmatter not yet enforced); `shell` alias only exists in the security broker, the tool is still `bash`.
