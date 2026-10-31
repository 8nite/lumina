---
date_modified: 2026-10-31
tags: [vault, secrets, placeholders, security, sessions, lumina]
---

# Session vault + `{placeholder_xxx}` remap

- Secret vault (`internal/agent/vault.go`, wired in `store.go`, `agent.go` loop): user-side transparency, LLM-side opacity. Outbound text (history, tool feedback, steered input, subagent tasks) is substituted before the model sees it; inbound `{placeholder_xxx}` refs rehydrate only on the user-facing path. Turns encrypted at rest.
- Ref shape: `{placeholder_xxx}` — lowercase letters only, stable per-session — so a ref can never be mistaken for (part of) a numeric code. Session-scoped memory vault; `vault check` CLI + debug log for inspection.
- Detector families (shapes, not values): labeled `api_key`/`secret`/`private_key`/`password`/`token`/`id` (+ `otp`/`pin`/`passcode`/SIN/SSN labels, CA 3-3-3 / US 3-2-4 kept as one ref); bare shapes `sk-…`, `AKIA…`, PEM private-key blocks (one ref per block), bare PIN/OTP/ID digit runs, segmented `1234-12345` codes, `bearer …`. Labeled SIN/SSN keeps label + one ref per number; bare formatted numbers fall to generic digit patterns.
- Layer-0 placeholder guidance: system prompt teaches the model to emit `{placeholder_xxx}` refs instead of raw secrets; wider secret detectors back it.
- Credential hard-deny widened (`internal/security/security.go`): whole-tree `.kube`/`.ssh`/`.aws`/`.gnupg` + `/etc/ssh` + `/etc/pki`, plus `shadow`/`gshadow`/`master.passwd`; still wins over every whitelist including layer-0 external paths. `PermDefault` is an alias of bypass (not ask). `IsPlanWritable` expands `~` before containment check.
- Layer-0 external whitelist: reads cover home (`~/.*`), temp (`os.TempDir`, `/tmp`, `/var/tmp`, `/temp`, `/private/tmp`), read-only system prefixes; writes stay workspace + temp only.
- Dual routing headers (see also [[lumina-live]]): `x-lumina-session` mirrored as `x-opencode-session` with the same UUID — opencode zen gateway 400s (`MissingSessionID`) without it. Covered by `TestRoutingSessionHeaders`.
See also: [[project]], [[lumina-live]], [[subagents-websearch]]
