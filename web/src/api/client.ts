// Typed daemon client (mirrors /openapi.json; regenerate via `pnpm openapi`).
export interface Session {
  id: string;
  workspace_dir: string;
  agent: string;
  permission_mode: string;
  model?: string;
  title?: string;
  running?: boolean;
}

export interface DaemonEvent {
  type: string;
  session_id?: string;
  data?: unknown;
}

export interface SessionState {
  session: Session;
  running: boolean;
  permission?: { request_id: string; tool: string; target: string } | null;
  question?: {
    request_id: string;
    questions: { question: string; header?: string; options?: { label: string; description?: string }[]; multiple?: boolean }[];
  } | null;
}

// Base URL switching ("See Servers"): relative = same origin.
let apiBase = '';
try {
  apiBase = localStorage.getItem('lumina.activeServer') || '';
} catch {
  /* private mode */
}

export function setApiBase(b: string) {
  apiBase = b;
  try {
    localStorage.setItem('lumina.activeServer', b);
  } catch {
    /* ignore */
  }
}

export function getApiBase() {
  return apiBase;
}

export function loadServers(): string[] {
  try {
    return JSON.parse(localStorage.getItem('lumina.servers') || '[]') as string[];
  } catch {
    return [];
  }
}

export function saveServers(list: string[]) {
  try {
    localStorage.setItem('lumina.servers', JSON.stringify(list));
  } catch {
    /* ignore */
  }
}

// Retryable failures cool down and retry instead of surfacing immediately:
// 429 (rate limit), 502/503/504 (gateway/proxy timeouts on long turns),
// plus network-level fetch rejections (TypeError). 4xx (except 429) is a
// real client error and throws at once.
function retryableStatus(status: number): boolean {
  return status === 429 || status === 502 || status === 503 || status === 504;
}

function cooldownMs(attempt: number, retryAfter: string | null): number {
  if (retryAfter) {
    const secs = Number(retryAfter.trim());
    // ponytail: numeric Retry-After only; date form falls back to backoff.
    if (Number.isFinite(secs) && secs >= 0) return Math.min(secs * 1000, 30_000);
  }
  // 1s, 2s, 4s exponential cooldown.
  return Math.min(1000 * 2 ** attempt, 8000);
}

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

async function req<T>(method: string, path: string, body?: unknown, retries = 3): Promise<T> {
  let lastErr: unknown = null;
  for (let attempt = 0; ; attempt++) {
    let res: Response;
    try {
      res = await fetch(apiBase + path, {
        method,
        headers: { 'Content-Type': 'application/json' },
        body: body ? JSON.stringify(body) : undefined,
      });
    } catch (e) {
      // Network-level failure (daemon restarting, proxy blip): cool down + retry.
      lastErr = e;
      if (attempt >= retries) throw e;
      await sleep(cooldownMs(attempt, null));
      continue;
    }
    if (res.ok) return res.json() as Promise<T>;
    if (!retryableStatus(res.status) || attempt >= retries) {
      // Keep the daemon's reason (e.g. 409 "turn in flight on another
      // daemon") so alerts name the cause instead of a bare status code.
      let detail = '';
      try {
        const text = await res.text();
        const err = JSON.parse(text) as { error?: string };
        detail = (err.error || text).slice(0, 300);
      } catch {
        /* non-JSON error body */
      }
      throw new Error(`${method} ${path}: ${res.status}${detail ? ' ' + detail : ''}`);
    }
    lastErr = new Error(`${method} ${path}: ${res.status}`);
    await sleep(cooldownMs(attempt, res.headers.get('retry-after')));
  }
  throw lastErr;
}

export interface PromptAttachment {
  name: string;
  mime: string;
  data: string; // raw base64
}

// Reasoning-effort variants offered by the composer.
export const EFFORTS = ['', 'none', 'minimal', 'low', 'medium', 'high', 'xhigh', 'max'];

export function fileToAttachment(file: File): Promise<PromptAttachment> {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onload = () => {
      const url = String(reader.result || '');
      const data = url.includes(',') ? url.split(',')[1] : url;
      resolve({ name: file.name, mime: file.type || 'image/png', data });
    };
    reader.onerror = () => reject(new Error('read failed: ' + file.name));
    reader.readAsDataURL(file);
  });
}

export const api = {
  sessions: () => req<Session[]>('GET', '/api/v1/sessions'),
  session: (id: string) => req<Session>('GET', `/api/v1/sessions/${id}`),
  sessionState: (id: string) => req<SessionState>('GET', `/api/v1/sessions/${id}/state`),
  createSession: (workspace_dir: string, agent = 'build', permission_mode = 'bypass') =>
    req<Session>('POST', '/api/v1/sessions', { workspace_dir, agent, permission_mode }),
  patchSession: (id: string, patch: Partial<Session>) =>
    req<Session>('PATCH', `/api/v1/sessions/${id}`, patch),
  fork: (id: string) => req<Session>('POST', `/api/v1/sessions/${id}/fork`, {}),
  prompt: async (id: string, text: string, images?: PromptAttachment[], effort?: string, delivery?: string) => {
    // Retry-safe submit: a 504/proxy timeout usually means the turn STARTED
    // server-side but the response was lost (the daemon drains detached, so
    // the turn continues). Every attempt reuses one client-generated
    // request_id, which the daemon dedups — even an inconclusive /state
    // probe can't steer duplicate text into the running turn (7614829e).
    // On a retryable failure check /state first: running → return accepted
    // and wait on SSE; idle → cool down + resubmit with the same key.
    const requestId =
      typeof crypto !== 'undefined' && 'randomUUID' in crypto
        ? crypto.randomUUID()
        : `${Date.now()}-${Math.random().toString(36).slice(2)}`;
    const body = { text, images: images || [], effort: effort || '', delivery: delivery || '', request_id: requestId };
    const path = `/api/v1/sessions/${id}/prompt`;
    for (let attempt = 0; ; attempt++) {
      try {
        return await req<{ status: string; position?: number; deduped?: boolean }>('POST', path, body, 0);
      } catch (e) {
        const retryable = /: (429|502|503|504)\b/.test(String(e)) || e instanceof TypeError;
        if (!retryable || attempt >= 3) throw e;
        try {
          const st = await req<SessionState>('GET', `/api/v1/sessions/${id}/state`, undefined, 2);
          if (st.running) return { status: 'accepted' };
        } catch {
          /* state check failed; resubmit with the same request_id — the
             daemon dedups it if the original POST landed */
        }
        await sleep(cooldownMs(attempt, null));
      }
    }
  },
  revert: (id: string) => req<{ status: string }>('POST', `/api/v1/sessions/${id}/revert`, {}),
  unrevert: (id: string) => req<{ status: string }>('POST', `/api/v1/sessions/${id}/unrevert`, {}),
  stop: (id: string) => req<{ status: string }>('POST', `/api/v1/sessions/${id}/stop`, {}),
  cancelQueued: (id: string, index: number) =>
    req<{ status: string; text: string; images: { name: string; media_type: string; data: string }[] }>(
      'POST',
      `/api/v1/sessions/${id}/queue/cancel`,
      { index },
    ),
  steerQueued: (id: string, index: number) =>
    req<{ status: string; text: string }>('POST', `/api/v1/sessions/${id}/queue/steer`, { index }),
  answerPermission: (reqId: string, decision: string) =>
    req<{ status: string }>('POST', `/api/v1/permissions/${reqId}`, { decision }),
  answerQuestion: (reqId: string, answers: string[][]) =>
    req<{ status: string }>('POST', `/api/v1/questions/${reqId}`, { answers }),
  dismissQuestion: (reqId: string) =>
    req<{ status: string }>('POST', `/api/v1/questions/${reqId}`, { dismissed: true }),
  export: (id: string) =>
    // `?format=json`: Rust serves the Go-shaped transcript JSON; the Go
    // daemon ignores the unknown param and returns JSON either way.
    req<{ transcript: { id: string; role: string; text: string; created_at?: string }[] }>(`GET`, `/api/v1/sessions/${id}/export?format=json`),
  routerState: async () => {
    const v = await req<Record<string, string> & { router?: Record<string, string> }>('GET', '/api/v1/router');
    // Rust returns flat keys + nested `router`; Go returns nested only.
    // Normalize to `{router: {...}}` for callers.
    const nested = v.router && typeof v.router === 'object' ? v.router : {};
    const flat: Record<string, string> = {};
    for (const k of ['fast', 'executor', 'planner', 'improver']) {
      const val = (v as Record<string, string>)[k];
      if (typeof val === 'string' && val) flat[k] = val;
    }
    return { router: { ...flat, ...nested } };
  },
  setRouter: (role: string, model: string) =>
    req<{ status: string }>('PUT', '/api/v1/router', { role, model, [role]: model }),
  providers: () => req<{ providers: (string | { name: string; protocol?: string })[] }>('GET', '/api/v1/providers'),
  models: (provider: string) =>
    req<{ models: { id: string; name: string; context_window?: number; supported_efforts?: string[] }[] }>(
      'GET',
      '/api/v1/models?provider=' + encodeURIComponent(provider),
    ),
  kbQuery: (query: string, directory?: string) =>
    req<{ results: unknown[] }>('POST', '/api/v1/kb/query', { query, directory }),
  branch: (directory?: string) =>
    req<{ branch: string; commit: string; modified: string }>(
      'GET',
      '/api/v1/git/branch' + (directory ? '?directory=' + encodeURIComponent(directory) : ''),
    ),
  harness: () => req<{ components: string[] }>('GET', '/api/v1/harness'),
  evolve: () => req<{ status: string }>('POST', '/api/v1/harness/evolve', {}),
  diff: (directory?: string) =>
    req<{ diff: string; stat: string; files: string[]; truncated: boolean }>(
      'GET',
      '/api/v1/git/diff' + (directory ? '?directory=' + encodeURIComponent(directory) : ''),
    ),
};

// SSE subscription over fetch (EventSource cannot send auth headers).
// Auto-reconnects with backoff so a dropped connection resumes the live
// stream instead of leaving the UI stuck at working… forever.
// onConnect fires after every (re)connect so the caller can resync missed
// state (transcript + running flag + pending prompts) via sessionState.
export function subscribe(onEvent: (e: DaemonEvent) => void, onConnect?: () => void): () => void {
  let dead = false;
  let delayMs = 1000;
  let connects = 0;
  const ctrls = new Set<AbortController>();
  const run = async () => {
    while (!dead) {
      const ctrl = new AbortController();
      ctrls.add(ctrl);
      try {
        const res = await fetch(apiBase + '/api/v1/events', {
          headers: { Accept: 'text/event-stream' },
          signal: ctrl.signal,
        });
        if (!res.body) throw new Error('no body');
        connects++;
        try {
          onConnect?.();
        } catch {
          /* resync failures retry on next event */
        }
        const reader = res.body.getReader();
        const dec = new TextDecoder();
        let buf = '';
        delayMs = 1000;
        for (;;) {
          const { done, value } = await reader.read();
          if (done) break;
          buf += dec.decode(value, { stream: true });
          let idx: number;
          while ((idx = buf.indexOf('\n\n')) >= 0) {
            const chunk = buf.slice(0, idx);
            buf = buf.slice(idx + 2);
            const dataLine = chunk.split('\n').find((l) => l.startsWith('data:'));
            if (!dataLine) continue;
            try {
              onEvent(JSON.parse(dataLine.slice(5).trim()) as DaemonEvent);
            } catch {
              /* keepalive */
            }
          }
        }
      } catch {
        /* reconnect below unless cleaned up */
      } finally {
        ctrls.delete(ctrl);
      }
      if (dead) return;
      await new Promise((r) => setTimeout(r, delayMs));
      delayMs = Math.min(delayMs * 2, 15000);
    }
  };
  void run();
  return () => {
    dead = true;
    for (const c of ctrls) c.abort();
    ctrls.clear();
  };
}
