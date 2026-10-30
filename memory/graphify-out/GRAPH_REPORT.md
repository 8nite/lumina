# Graph Report - memory  (2026-10-29)

## Corpus Check
- Corpus is ~509 words - fits in a single context window. You may not need a graph.

## Summary
- 15 nodes · 25 edges · 4 communities (2 shown, 2 thin omitted)
- Extraction: 96% EXTRACTED · 4% INFERRED · 0% AMBIGUOUS · INFERRED: 1 edges (avg confidence: 0.85)
- Token cost: 0 input · 0 output

## Community Hubs (Navigation)
- M0-M6 Build and Delivery
- Plan/Build and Permission Modes
- Memory Index and Live Wiring
- Graphify KB and Guardrails

## God Nodes (most connected - your core abstractions)
1. `Lumina Pure-Go Daemon` - 9 edges
2. `Lumina plan.md Implementation (M0-M6)` - 8 edges
3. `Markdown + Graphify RAG Engine` - 5 edges
4. `lumina Live Wiring` - 4 edges
5. `Plan & Build Primary Agent Modes (Tab)` - 3 edges
6. `Lumina Memory Index` - 3 edges
7. `Harness2 Propose-Probe-Compose Engine` - 3 edges
8. `Lumina-Compatible JS/TS Plugin Host` - 3 edges
9. `4-Mode Permission & Bypass Broker (Shift+Tab)` - 3 edges
10. `AGENTS.md & Markdown Extensions Loader` - 2 edges

## Surprising Connections (you probably didn't know these)
- `lumina Live Wiring` --references--> `Lumina Pure-Go Daemon`  [INFERRED]
  lumina-live.md → project.md
- `Lumina plan.md Implementation (M0-M6)` --references--> `Lumina Pure-Go Daemon`  [EXTRACTED]
  lumina-build.md → project.md
- `Lumina Memory Index` --references--> `Lumina Pure-Go Daemon`  [EXTRACTED]
  MEMORY.md → project.md
- `Lumina plan.md Implementation (M0-M6)` --references--> `Session Continue, Fork & Shadow-Git Undo`  [EXTRACTED]
  lumina-build.md → project.md
- `Lumina Memory Index` --references--> `Lumina plan.md Implementation (M0-M6)`  [EXTRACTED]
  MEMORY.md → lumina-build.md

## Hyperedges (group relationships)
- **Lumina Local-First Architecture Pillars** — project_lumina_daemon, project_graphify_kb, project_harness2_engine, project_no_google_secret_guard [EXTRACTED 1.00]
- **Lumina Runtime Parity Surface** — project_rules_hierarchy, project_lumina_plugins, project_plan_build_modes, project_permission_modes, project_session_lifecycle [EXTRACTED 1.00]

## Communities (4 total, 2 thin omitted)

### Community 0 - "M0-M6 Build and Delivery"
Cohesion: 0.50
Nodes (5): Lumina plan.md Implementation (M0-M6), Go plus Web Verification Gate, Harness2 Propose-Probe-Compose Engine, Lumina-Compatible JS/TS Plugin Host, Session Continue, Fork & Shadow-Git Undo

### Community 1 - "Plan/Build and Permission Modes"
Cohesion: 0.83
Nodes (4): Lumina Pure-Go Daemon, 4-Mode Permission & Bypass Broker (Shift+Tab), Plan & Build Primary Agent Modes (Tab), AGENTS.md & Markdown Extensions Loader

## Knowledge Gaps
- **2 isolated node(s):** `Singleflight Standing-Ops KB Sweeper`, `Go plus Web Verification Gate`
  These have ≤1 connection - possible missing edges or undocumented components. (Counts symbols only; 3 node(s) total have ≤1 connection when file, concept and rationale nodes are included.)
- **2 thin communities (<3 nodes) omitted from report** — run `graphify query` to explore isolated nodes.

## Suggested Questions
_Questions this graph is uniquely positioned to answer:_

- **Why does `Lumina Pure-Go Daemon` connect `Plan/Build and Permission Modes` to `M0-M6 Build and Delivery`, `Memory Index and Live Wiring`, `Graphify KB and Guardrails`?**
  _High betweenness centrality (0.453) - this node is a cross-community bridge._
- **What connects `Singleflight Standing-Ops KB Sweeper`, `Go plus Web Verification Gate` to the rest of the system?**
  _2 weakly-connected nodes found - possible documentation gaps or missing edges._
- **Why does `Lumina plan.md Implementation (M0-M6)` connect `M0-M6 Build and Delivery` to `Plan/Build and Permission Modes`, `Memory Index and Live Wiring`, `Graphify KB and Guardrails`?**
  _High betweenness centrality (0.308) - this node is a cross-community bridge._
- **Why does `Markdown + Graphify RAG Engine` connect `Graphify KB and Guardrails` to `M0-M6 Build and Delivery`, `Plan/Build and Permission Modes`?**
  _High betweenness centrality (0.239) - this node is a cross-community bridge._