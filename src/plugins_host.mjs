// Lumina plugin worker: loads Lumina-compatible JS/TS plugins and serves
// hook dispatches over a Unix socket (line-delimited JSON).
//
// Supported plugin signatures (both, like Lumina):
//   1. Factory / return-object: export default async ({ $, directory }) => ({ "tool.execute.before": fn, ... })
//   2. v2 object with setup(): export default { id, async setup(ctx) { ctx.tool.hook("execute.before", fn) } }
//
// Hook context passed to plugins:
//   { $, directory, sessionID, serverUrl }
// Hook map keys accepted in both styles:
//   config, chat.message, chat.messages.transform,
//   tool.execute.before, tool.execute.after,
//   shell.env, permission.ask, event
import { createServer } from "node:net";
import { readFile, unlink } from "node:fs/promises";
import path from "node:path";
import { pathToFileURL } from "node:url";

const args = process.argv.slice(2);
const sockIdx = args.indexOf("--socket");
const plugIdx = args.indexOf("--plugins");
const sock = sockIdx >= 0 ? args[sockIdx + 1] : null;
const pluginDefs = plugIdx >= 0 ? JSON.parse(args[plugIdx + 1]) : [];

if (!sock) {
  console.error("host.mjs: --socket required");
  process.exit(1);
}

// Minimal $ helper (exec + file helpers) mirroring Lumina's plugin $ surface.
const $ = {
  async exec(cmd, opts = {}) {
    const { execFile } = await import("node:child_process");
    const { promisify } = await import("node:util");
    const run = promisify(execFile);
    try {
      const { stdout } = await run("sh", ["-c", cmd], { timeout: (opts.timeout ?? 30) * 1000 });
      return stdout;
    } catch (e) {
      return e.stdout ?? "";
    }
  },
};

function normalizeHooks(raw) {
  // v2 style registers via ctx.tool.hook / ctx.chat hooks inside setup();
  // factory style returns the hook map directly.
  const out = {};
  for (const [k, v] of Object.entries(raw || {})) {
    if (typeof v === "function") out[k] = v;
  }
  return out;
}

function makeSetupCtx(collected) {
  const hook = (name, fn) => {
    collected[name] = collected[name] || [];
    collected[name].push(fn);
  };
  return {
    $,
    tool: { hook: (n, fn) => hook(`tool.${n}`, fn) },
    chat: { hook: (n, fn) => hook(`chat.${n}`, fn) },
    shell: { hook: (n, fn) => hook(`shell.${n}`, fn) },
    permission: { hook: (n, fn) => hook(`permission.${n}`, fn) },
    event: { hook: (n, fn) => hook(`event.${n}`, fn) },
  };
}

const hooks = {}; // name -> [fn]
function addHooks(map) {
  for (const [k, v] of Object.entries(normalizeHooks(map))) {
    hooks[k] = hooks[k] || [];
    hooks[k].push(v);
  }
}

async function loadPlugin(file) {
  const mod = await import(pathToFileURL(path.resolve(file)).href + `?t=${Date.now()}`);
  const exp = mod.default ?? mod;
  const ctx = { $, directory: process.cwd(), sessionID: "", serverUrl: "" };
  if (exp && typeof exp.setup === "function") {
    const collected = {};
    await exp.setup(makeSetupCtx(collected));
    for (const [k, fns] of Object.entries(collected)) for (const fn of fns) {
      hooks[k] = hooks[k] || [];
      hooks[k].push(fn);
    }
  } else if (typeof exp === "function") {
    const returned = await exp(ctx);
    addHooks(returned);
  } else if (typeof exp === "object") {
    addHooks(exp);
  }
}

function shortHook(h) {
  // Accept both "tool.execute.before" and "execute.before".
  if (hooks[h]) return hooks[h];
  const short = h.split(".").slice(1).join(".");
  return hooks[short] || [];
}

async function dispatch(hook, payload) {
  const fns = [...(hooks[hook] || []), ...shortHook(hook).filter((f) => !(hooks[hook] || []).includes(f))];
  let output = { ...(payload || {}) };
  for (const fn of fns) {
    const res = await fn({ ...output, $ });
    if (typeof res === "string" && hook === "tool.execute.before" && res !== "") {
      // Convention: returning guidance string rewrites nothing; throw to block.
      continue;
    }
    if (res && typeof res === "object") {
      if (res.block) return { block: res.block };
      if (res.patch) output = { ...output, ...res.patch };
      else if (res.output) output = { ...output, ...res.output };
      else output = { ...output, ...res };
    }
  }
  return { patch: output };
}

await unlink(sock).catch(() => {});
for (const p of pluginDefs) {
  try {
    await loadPlugin(p.path);
  } catch (e) {
    console.error(`plugin load failed ${p.path}: ${e.message}`);
  }
}

const server = createServer((conn) => {
  let buf = "";
  conn.on("data", async (chunk) => {
    buf += chunk.toString();
    let idx;
    while ((idx = buf.indexOf("\n")) >= 0) {
      const line = buf.slice(0, idx);
      buf = buf.slice(idx + 1);
      if (!line.trim()) continue;
      try {
        const { hook, payload } = JSON.parse(line);
        const out = await dispatch(hook, payload || {});
        conn.write(JSON.stringify(out) + "\n");
      } catch (e) {
        conn.write(JSON.stringify({ error: e.message }) + "\n");
      }
    }
  });
});

server.listen(sock, () => {
  // ready
});
