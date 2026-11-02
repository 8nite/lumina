// serves the Zen model catalog (plus Zen schema
// metadata) to any connected provider.
//
// Why a plugin: the Zen catalog endpoint and its schema
// (`limit.context`, `capabilities.reasoning.effort`) are provider-specific.
// Core only parses generic OpenAI/OpenRouter metadata and fetches
// `<base_url>/models`; this plugin merges the Zen catalog on top via the
// generic `models.list` hook, so core stays provider-neutral.
//
// Per upstream docs: the same API key works for Go and Zen.
// Go catalog:  <base_url>/zen/go/v1/models (or /inference/go/...)
// Zen catalog: <base_url>/zen/v1/models
// A Go id like `muse-spark-1.3-contributor` and its Zen twin `muse-spark-1.3`
// share `canonical_id` (`meta/muse-spark-1.3`). Twins are endpoint-bound:
// each id 400s on the other endpoint ("Model is unavailable"), so the plugin
// lists the Go id (enriched with the twin's metadata) and suppresses the
// Zen twin. Zen-only ids translate across endpoints and are listed.
//
// Install:
//   cp plugins/<plugin>.mjs ~/.config/lumina/plugins/
// Requires the provider to use api_key_env or api_key_cmd (never a pasted
// secret — the plugin runs your own configured command locally).
export default async ({ $ }) => ({
  "models.list": async (input) => {
    const provider = input.provider || "";
    const baseUrl = String(input.base_url || "");
    if (!/opencode/i.test(provider) && !baseUrl.includes("opencode.ai")) {
      return {}; // not ours — keep the core list untouched
    }
    try {
      let key = "";
      if (input.api_key_env && process.env[input.api_key_env]) {
        key = process.env[input.api_key_env];
      } else if (input.api_key_cmd) {
        key = String(await $.exec(input.api_key_cmd)).trim();
      }
      if (!key) return {};
      const sid = Math.random().toString(36).slice(2);
      const headers = {
        Authorization: `Bearer ${key}`,
        "User-Agent": "lumina/0.1.0",
        "x-opencode-session": sid,
        "x-lumina-session": sid,
      };
      const get = async (url) => {
        const res = await fetch(url, { headers, signal: AbortSignal.timeout(15000) });
        if (!res.ok) return [];
        const body = await res.json();
        return (body && body.data) || [];
      };
      // ponytail: one documented endpoint, overridable for tests.
      const zenUrl =
        process.env.OPENCODE_ZEN_MODELS_URL || "https://opencode.ai/zen/v1/models";
      const [zen, go] = await Promise.all([
        get(zenUrl),
        get(baseUrl.replace(/\/+$/, "") + "/models"),
      ]);
      if (zen.length === 0 && go.length === 0) return {};
      const metaOf = (m) => ({
        context_window: (m.limit && m.limit.context) || 0,
        supported_efforts:
          (m.capabilities && m.capabilities.reasoning && m.capabilities.reasoning.effort) || [],
      });
      // Canonical index from the raw Go catalog (core drops canonical_id).
      const canonOf = new Map();
      for (const m of go) {
        if (m && m.id && m.canonical_id) canonOf.set(m.id, m.canonical_id);
      }
      const zenById = new Map();
      const zenByCanon = new Map();
      for (const m of zen) {
        if (!m || !m.id) continue;
        const meta = { id: m.id, name: m.name || m.id, ...metaOf(m) };
        zenById.set(m.id, meta);
        if (m.canonical_id) zenByCanon.set(m.canonical_id, meta);
      }
      // Go twins sharing a canonical id are served by the Go endpoint under
      // the Go id only (the Zen twin id 400s there) — suppress the twin.
      const goCanons = new Set(canonOf.values());
      const twinIds = new Set(
        [...zenByCanon].filter(([c]) => goCanons.has(c)).map(([, m]) => m.id),
      );
      // Merge: core ids kept; Zen wins per id, or enriches via canonical_id.
      const merged = new Map();
      for (const m of input.models || []) {
        if (!m || !m.id) continue;
        const canon = canonOf.get(m.id);
        const twin = zenByCanon.get(canon);
        merged.set(m.id, {
          id: m.id,
          name: m.name || m.id,
          context_window: m.context_window || (twin && twin.context_window) || 0,
          supported_efforts:
            (m.supported_efforts && m.supported_efforts.length && m.supported_efforts) ||
            (twin && twin.supported_efforts) ||
            [],
        });
      }
      for (const [id, z] of zenById) {
        if (merged.has(id) || twinIds.has(id)) continue;
        merged.set(id, z);
      }
      const models = [...merged.values()].sort((a, b) =>
        String(a.id < b.id ? -1 : a.id > b.id ? 1 : 0)
      );
      return { models };
    } catch {
      return {}; // catalog failure must never break the core list
    }
  },
});
