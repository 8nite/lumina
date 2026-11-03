# Graph Report - memory  (2026-11-03, refreshed inline incl. stuck-session fix + drive_one retry + prompt idempotency + follow-up inheritance/idle nudge + subagent budget/verify prod + harness-silent-turn-death EOF flush + kb-lazy-resolution + todo-pill-3of3 + harness-gap-analysis + smoke3/verify-gate/redirect-escalation + acp/mcp-serve serve plans + dedup rebuild + webui-tables GFM grammar + over-exploration addendum-2 + cache-hits telemetry + loop-harness/prompt-e2e cross-links)

## Corpus Check
- 38 files · ~11,735 words
- Verdict: corpus is large enough that graph structure adds value.

## Summary
- 189 nodes · 256 links · 5 communities
- Extraction: 83% EXTRACTED · 11% INFERRED · 6% other/unlabeled · INFERRED: 29 edges (avg confidence: 0.8)
- Token cost: 0 input · 0 output

## Community Hubs (Navigation)
- M0-M6 Build and Delivery
- Memory Index and Live Wiring
- Plan/Build and Permission Modes
- Subagent Background Web Tools
- Harness Strategy and Competitive Gaps

## God Nodes (most connected - your core abstractions)
1. `Single-Binary Rust Port` - 14 edges
2. `Subagent Background Web Tool Surface` - 12 edges
3. `Lumina Memory Index` - 11 edges
4. `Lumina Pure-Go Daemon` - 11 edges
5. `Turn Delta Tool Completed Streaming Events` - 10 edges
6. `Lumina plan.md Implementation (M0-M6)` - 9 edges
7. `lumina Live Wiring` - 8 edges
8. `Identical-Round Loop Guard` - 8 edges
9. `Eight Ranked Harness Gaps` - 8 edges
10. `Rust Round-2 Deferrals` - 7 edges

## Surprising Connections (you probably didn't know these)
- `4-Mode Permission & Bypass Broker (Shift+Tab)` --references--> `OS Sandbox Gap (critical)`  [INFERRED]
  project.md → harness-gap-analysis.md
- `Rust Steer/Queue Protocol` --implements--> `Steer Queue Inbox While Working`  [EXTRACTED]
  rust-port.md → verbose-streaming.md
- `Markdown + Graphify RAG Engine` --references--> `Harness2 plus Graphify RAG Moats`  [INFERRED]
  project.md → harness-gap-analysis.md
- `Harness2 Propose-Probe-Compose Engine` --references--> `Harness2 plus Graphify RAG Moats`  [INFERRED]
  project.md → harness-gap-analysis.md
- `lumina Live Wiring` --references--> `Lumina Pure-Go Daemon`  [INFERRED]
  lumina-live.md → project.md

## Hyperedges (group relationships)
- **Lumina Local-First Architecture Pillars** — project_lumina_daemon, project_graphify_kb, project_harness2_engine, project_no_google_secret_guard [EXTRACTED 1.00]
- **Lumina Runtime Parity Surface** — project_rules_hierarchy, project_lumina_plugins, project_plan_build_modes, project_permission_modes, project_session_lifecycle [EXTRACTED 1.00]
- **Reference parity Tool Streaming Surface** — prompt_e2e_guidance, subagents_websearch_tool_surface, verbose_streaming_turn_events [EXTRACTED 1.00]

## Communities (5 total, 0 thin omitted)

### Community 0 - "M0-M6 Build and Delivery"
Cohesion: 0.06
Nodes (41): Rust Agent Stage Progress, Rust Begin-Patch Engine, Rust Copilot Session Fix, Rust LLM Transport Parity Fixes, Rust Round-2 Deferrals, Rust Round-2 Verify, Rust Server TUI CLI Parity, Rust Tool Surface Parity (+33 more)

### Community 1 - "Memory Index and Live Wiring"
Cohesion: 0.03
Nodes (75): lumina Live Wiring, Lumina Memory Index, Router Context Cache with Caps, Provider-Sourced Context Window, Live Context Gauge (turn.context), Long-Session Hardening Pass, Web Permission Modal Answers permission.asked, Per-Session Prompt Single-Flight (+67 more)

### Community 3 - "Plan/Build and Permission Modes"
Cohesion: 0.14
Nodes (15): Lumina Pure-Go Daemon, 4-Mode Permission & Bypass Broker (Shift+Tab), Plan & Build Primary Agent Modes (Tab), AGENTS.md & Markdown Extensions Loader, agent-browser Skill Port, Config Defaults Seed Flags, Skill Advertisement Fix (skillsBlock), Global Skill Promotion plus Keyless Fallback (+7 more)

### Community 2 - "Subagent Background Web Tools"
Cohesion: 0.09
Nodes (23): Conditional Tool Guidance Rendering, Env Block plus Tool Schema Parity, Plan Fence plus Question Allowance, Patch Tool Envelope, Question Round-Trip UI Wiring, Empty-Final Turn Guard, Auto-Continue Tool Ceiling, Identical-Round Loop Guard (+15 more)

### Community 4 - "Harness Strategy and Competitive Gaps"
Cohesion: 0.32
Nodes (8): ACP Editor-Client Plan (src/acp.rs), Harness2 plus Graphify RAG Moats, LSP plus ACP Gaps (core), Act-as-MCP-Server Gap (core), MCP-Serve Delegation Plan (lumina mcp serve), OS Sandbox Gap (critical), Eight Ranked Harness Gaps, Rewind plus Distribution Gaps (notable)

## Knowledge Gaps
- **52 isolated node(s):** `Rust Background-Done Push`, `Rust PTY Parity Confirmed`, `Rust Round-3 Open Polish`, `Rust Round-3 Verify`, `Rust TUI Pickers and Scroll` (+47 more)
  These have ≤1 connection - possible missing edges or undocumented components. (Counts symbols only; 52 node(s) total have ≤1 connection when file, concept and rationale nodes are included.)

## Suggested Questions
_Questions this graph is uniquely positioned to answer:_

- **Why does `Subagent Background Web Tool Surface` connect `Subagent Background Web Tools` to `Memory Index and Live Wiring`, `Plan/Build and Permission Modes`?**
  _High betweenness centrality (0.223) - this node is a cross-community bridge._
- **Are the 3 inferred relationships involving `Subagent Background Web Tool Surface` (e.g. with `Lumina Pure-Go Daemon` and `Patch Tool Envelope`) actually correct?**
  _`Subagent Background Web Tool Surface` has 3 INFERRED edges - model-reasoned connections that need verification._
- **What connects `Rust Background-Done Push`, `Rust PTY Parity Confirmed`, `Rust Round-3 Open Polish` to the rest of the system?**
  _52 weakly-connected nodes found - possible documentation gaps or missing edges._
- **Should `M0-M6 Build and Delivery` be split into smaller, more focused modules?**
  _Cohesion score 0.05574912891986063 - nodes in this community are weakly interconnected._
- **Why does `Lumina Memory Index` connect `Memory Index and Live Wiring` to `M0-M6 Build and Delivery`, `Subagent Background Web Tools`, `Plan/Build and Permission Modes`, `Harness Strategy and Competitive Gaps`?**
  _High betweenness centrality (0.183) - this node is a cross-community bridge._
- **Are the 2 inferred relationships involving `Lumina Pure-Go Daemon` (e.g. with `lumina Live Wiring` and `Subagent Background Web Tool Surface`) actually correct?**
  _`Lumina Pure-Go Daemon` has 2 INFERRED edges - model-reasoned connections that need verification._
- **Should `Memory Index and Live Wiring` be split into smaller, more focused modules?**
  _Cohesion score 0.028421052631578948 - nodes in this community are weakly interconnected._
