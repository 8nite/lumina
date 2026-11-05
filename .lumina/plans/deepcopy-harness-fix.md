# DeepCopy harness-only fix — PLAN (no `src/` changes)

## Problem
Same prompt, same model (`muse-spark-1.3-contributor`), same workspace `/home/herbert/trading-dashboard`:
- lumina `5bc43142` (11:43–11:50Z, ended `Stopped by user`): 1x `bash ls`, then `read`/`kb_query`/`grep`/`glob` loop over static files. Never queried live state. Final guess correct in shape (own-exit vs mirror) but with no `closeReason` proof.
- opencode `ses_e5476de2` (11:47–11:54Z): same initial reads, then `kubectl get pods/svc`, `curl relay/status`, `kubectl logs mt4-safe-algo --since 48/72h + --previous`, `pymongo relay_copies + relay_commands + relay_configs + signals`. Answer: `USDCHF #250202305 -> #250202314`, `EQUITY STOP` basket of 7 at `2026-11-04T01:18:14.988Z`, `equityStopPct=5`, `mirrorMasterClose=true`, master still open.

Root cause (harness, not model): no harness SOP tells the agent that `why did X close at T` is a live-state question. It stayed in Markdown + stale `DetailedStatement.htm` (last data Oct-28, useless for Nov-04). `venv/node_modules` globs polluted context.

## Scope
Harness only: `.lumina/harness/*`. No `src/*.rs`, no config, no K8s/Mongo changes, no daemon restart.

## Changes (4 files, additive)

### 1. `.lumina/harness/tool_guidance.md` (append)
```md
- Live-state first for close/exit questions (`why did X close`, `why still open in master`): before theorizing, run live checks via `bash`: `curl .../api/relay/status`, `kubectl logs -n mt4 deploy/mt4-safe-algo --since 48h`, Mongo `relay_copies`/`relay_commands` for the ticket window. Static `DetailedStatement*.htm` older than the incident timestamp is stale — cite it as history only, never as the close proof.
- `bash` reservation: `kubectl` / `mongo` / `pymongo` / `curl` to live services are NOT native-tool equivalents. The read/grep/glob nudge never applies to them; use `bash` for them freely.
- Glob hygiene: exclude `venv/`, `.venv/`, `node_modules/`, `__pycache__/` on broad `**/*copy*`-style searches (e.g. `glob **/copyEngine.ts` beats `grep copy` over the tree).
- Close proof required: ticket pair (`DC<master>` comment, local ticket) + `relay_copies.closeReason` + `closedAt` before answering `EQUITY STOP` vs `mirror master closed` vs `own TP/SL/time`.
```

### 2. `.lumina/harness/system_prompt.md` (append)
```md
- Finish close/exit questions: fetch the ticket + `closeReason` yourself (`relay_copies`, `relay_commands`, logs). Do not end with "need ticket to confirm" when you have `bash` + Mongo/K8s access — go get it, then answer.
```

### 3. `.lumina/harness/skills/deepcopy-relay/SKILL.md` (new)
```md
---
name: deepcopy-relay
description: Diagnose DeepCopy relay open/close divergence (follower 11646258 vs master 11632704). Use when user asks why a copy closed, why master still holds it, or whether to re-copy. Prefers Mongo journal + K8s logs over statements.
---
# DeepCopy relay forensics
1. Identify pair: `grep DC<masterTicket> DetailedStatement.htm` for history; live truth is Mongo.
2. Live config: `curl -s -m 15 "https://mt4.nexvision.cc/api/relay/status?all=1"`.
3. Logs: `kubectl logs -n mt4 deploy/mt4-safe-algo --since=48h | grep "relay sync follower=11646258"` (+ `--previous` for restarts).
4. Journal (via `bash` + `pymongo`, secrets from `kubectl get secret mt4-secrets -n mt4 -o json` in-process, never print): `relay_copies.find({followerAccountId:11646258, closedAt:{$gte:incident-5m,$lt:incident+5m}})`, `relay_commands` same window, `relay_configs.find({followerAccountId:11646258})`.
5. Decide: `upstream/mt4-safe-algo/src/lib/copyEngine.ts:314` equity-stop closes all on follower floating; `:326` mirror-close only when master gone; `:349` journal authoritative, closed stays closed. Cite `closeReason` + `closedAt` + sibling tickets.
```

### 4. `.lumina/harness/guardrails.yaml` (append, additive-only)
```yaml
- id: deepcopy-close-evidence
  when: prompt matches "why.*clos|still.*hold|mirror.*master"
  require: [relay_copies.closeReason, relay_copies.closedAt, sibling-basket-check]
  forbid: answering EQUITY-STOP vs mirror from DetailedStatement.htm alone when incident > statement max date
- id: deepcopy-secret-hygiene
  when: querying Mongo/K8s secrets
  require: secrets via env/subprocess in bash, never print URI/key, never commit transcript values
```

## Verification (harness gates, no src tests needed)
1. `cat .lumina/harness/tool_guidance.md .lumina/harness/system_prompt.md` shows appended blocks; `ls .lumina/harness/skills/deepcopy-relay/SKILL.md` exists.
2. `lumina evolve --dry-run` or worktree probe: candidate = these 4 files; `verify` = files exist + `guardrails.yaml` still contains old lines (additive check in `src/harness.rs:86`).
3. Manual replay checklist (next deepcopy question): first `bash` after intent is `relay/status` or `kubectl logs`, not 3rd `read`; final answer cites `closeReason` + `closedAt` + ≥1 `copyEngine.ts` line.
4. Quality floor: `compose` lands only if score >= baseline; rollback = `git rev-parse refs/lumina/harness` before + `git update-ref` back (never touches user HEAD).

## Non-goals
- No edits to `src/agent.rs` (prompt/loop guards), `src/tools.rs` (bash_native_blocked), `src/harness.rs`, `src/rules.rs`.
- No change to `equityStopPct`, relay policy, or dashboards. This plan explains divergence; it does not change trading behavior.
