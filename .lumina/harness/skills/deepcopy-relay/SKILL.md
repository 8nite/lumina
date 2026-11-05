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
