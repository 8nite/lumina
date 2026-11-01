---
date_modified: 2026-11-01
tags: [plugin, models, opencode, zen, router]
---

# Zen twin suppression (endpoint-bound ids)

- Zen/Go catalog twins (same `canonical_id`) are endpoint-bound: each id 400s on the other endpoint.
- Plugin lists the enriched Go id, suppresses the unusable Zen twin; Zen-only ids translate and stay listed. Live: 55 models, spark filter shows only servable ids.

See also: [[opencode-zen-plugin]], [[model-picker-filter]], [[opencode-zen-models]]
