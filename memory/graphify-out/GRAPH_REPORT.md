# Graph Report - memory  (2026-10-31, refreshed inline incl. loops-and-claims)

## Corpus Check
- 14 files · ~3600 words
- Verdict: corpus is large enough that graph structure adds value.

## Summary
- 56 nodes · 88 edges · 4 communities
- Extraction: 90% EXTRACTED · 10% INFERRED · 0% AMBIGUOUS · INFERRED: 7 edges (avg confidence: 0.84)
- Token cost: 0 input · 0 output

## Community Hubs (Navigation)
- M0-M6 Build and Delivery
- Memory Index and Live Wiring
- Subagent Background Web Tools
- Plan/Build and Permission Modes

## God Nodes (most connected - your core abstractions)
1. `Lumina Pure-Go Daemon` - 10 edges
2. `Lumina plan.md Implementation (M0-M6)` - 8 edges
3. `Subagent Background Web Tool Surface` - 8 edges
4. `Lumina Memory Index` - 7 edges
5. `Markdown + Graphify RAG Engine` - 5 edges
6. `lumina Live Wiring` - 6 edges
7. `Turn Delta Tool Completed Streaming Events` - 5 edges
8. `Reference system-prompt Parity via toolGuidance` - 4 edges
9. `Harness2 Propose-Probe-Compose Engine` - 3 edges
10. `Lumina-Compatible JS/TS Plugin Host` - 3 edges

## Surprising Connections (you probably didn't know these)
- `lumina Live Wiring` --references--> `Lumina Pure-Go Daemon`  [INFERRED]
  lumina-live.md → project.md
- `Lumina Pure-Go Daemon` --references--> `Subagent Background Web Tool Surface`  [INFERRED]
  project.md → subagents-websearch.md
- `Steer Queue Inbox While Working` --references--> `Subagent Background Web Tool Surface`  [EXTRACTED]
  verbose-streaming.md → subagents-websearch.md
- `Turn Delta Tool Completed Streaming Events` --references--> `Subagent Background Web Tool Surface`  [INFERRED]
  verbose-streaming.md → subagents-websearch.md
- `Lumina plan.md Implementation (M0-M6)` --references--> `Session Continue, Fork & Shadow-Git Undo`  [EXTRACTED]
  lumina-build.md → project.md

## Hyperedges (group relationships)
- **Lumina Local-First Architecture Pillars** — project_lumina_daemon, project_graphify_kb, project_harness2_engine, project_no_google_secret_guard [EXTRACTED 1.00]
- **Lumina Runtime Parity Surface** — project_rules_hierarchy, project_lumina_plugins, project_plan_build_modes, project_permission_modes, project_session_lifecycle [EXTRACTED 1.00]
- **Reference parity Tool Streaming Surface** — prompt_e2e_guidance, subagents_websearch_tool_surface, verbose_streaming_turn_events [EXTRACTED 1.00]
- **Reference parity Tool Streaming Surface** — prompt_e2e_guidance, subagents_websearch_tool_surface, verbose_streaming_turn_events [EXTRACTED 1.00]

## Communities (4 total, 0 thin omitted)

### Community 0 - "M0-M6 Build and Delivery"
Cohesion: 0.32
Nodes (7): Lumina plan.md Implementation (M0-M6), Go plus Web Verification Gate, Markdown + Graphify RAG Engine, Harness2 Propose-Probe-Compose Engine, Lumina-Compatible JS/TS Plugin Host, Session Continue, Fork & Shadow-Git Undo, Singleflight Standing-Ops KB Sweeper

### Community 1 - "Memory Index and Live Wiring"
Cohesion: 0.40
Nodes (18): lumina Live Wiring, Lumina Memory Index, Steer Queue Inbox While Working, TUI Thinking Toggle plus Web Tool Pills, Turn Delta Tool Completed Streaming Events, Long-Session Hardening Pass, Per-Session Prompt Single-Flight, Terminal SSE plus Auto-Reconnect, Web Permission Modal Answers permission.asked, Live Context Gauge (turn.context), TUI Space Key Types, Provider-Sourced Context Window, Router Context Cache with Caps, Dual Routing Session Headers, Session State Resync Endpoint, TUI Resync Timeline Rebuild, Missed-Started Recovery, Effort Variant Picker

### Community 2 - "Subagent Background Web Tools"
Cohesion: 0.40
Nodes (9): BTC E2E Web Subagent Background Verification, SSE Fragment Accumulation Fix, Reference system-prompt Parity via toolGuidance, Plan-Fence Deny plus Permission Gating, Depth-1 Subagent Runner with Explore Envelope, Subagent Background Web Tool Surface, Question Round-Trip UI Wiring, Patch Tool Envelope, Identical-Round Loop Guard

### Community 3 - "Plan/Build and Permission Modes"
Nodes (9): Lumina Pure-Go Daemon, 4-Mode Permission & Bypass Broker (Shift+Tab), Plan & Build Primary Agent Modes (Tab), AGENTS.md & Markdown Extensions Loader, Config Defaults Seed Flags, agent-browser Skill Port, Skill Advertisement Gap, Skill Advertisement Fix (skillsBlock), Global Skill Promotion plus Keyless Fallback

<!-- superseded node list follows -->
### Community 3 (prev)
Cohesion: 0.83
Nodes (5): Lumina Pure-Go Daemon, 4-Mode Permission & Bypass Broker (Shift+Tab), Plan & Build Primary Agent Modes (Tab), AGENTS.md & Markdown Extensions Loader, Config Defaults Seed Flags

## Knowledge Gaps
- **6 isolated node(s):** `Go plus Web Verification Gate`, `Singleflight Standing-Ops KB Sweeper`, `TUI Thinking Toggle plus Web Tool Pills`, `SSE Fragment Accumulation Fix`, `Plan-Fence Deny plus Permission Gating` (+1 more)
  These have ≤1 connection - possible missing edges or undocumented components. (Counts symbols only; 7 node(s) total have ≤1 connection when file, concept and rationale nodes are included.)

## Suggested Questions
_Questions this graph is uniquely positioned to answer:_

- **Why does `Lumina Pure-Go Daemon` connect `Plan/Build and Permission Modes` to `M0-M6 Build and Delivery`, `Memory Index and Live Wiring`, `Subagent Background Web Tools`?**
  _High betweenness centrality (0.460) - this node is a cross-community bridge._
- **Are the 2 inferred relationships involving `Lumina Pure-Go Daemon` (e.g. with `lumina Live Wiring` and `Subagent Background Web Tool Surface`) actually correct?**
  _`Lumina Pure-Go Daemon` has 2 INFERRED edges - model-reasoned connections that need verification._
- **What connects `Go plus Web Verification Gate`, `Singleflight Standing-Ops KB Sweeper`, `TUI Thinking Toggle plus Web Tool Pills` to the rest of the system?**
  _6 weakly-connected nodes found - possible documentation gaps or missing edges._
- **Why does `Subagent Background Web Tool Surface` connect `Subagent Background Web Tools` to `Memory Index and Live Wiring`, `Plan/Build and Permission Modes`?**
  _High betweenness centrality (0.342) - this node is a cross-community bridge._
- **Are the 2 inferred relationships involving `Subagent Background Web Tool Surface` (e.g. with `Lumina Pure-Go Daemon` and `Turn Delta Tool Completed Streaming Events`) actually correct?**
  _`Subagent Background Web Tool Surface` has 2 INFERRED edges - model-reasoned connections that need verification._
- **Why does `Lumina plan.md Implementation (M0-M6)` connect `M0-M6 Build and Delivery` to `Memory Index and Live Wiring`, `Plan/Build and Permission Modes`?**
  _High betweenness centrality (0.200) - this node is a cross-community bridge._