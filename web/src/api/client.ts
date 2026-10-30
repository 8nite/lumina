// Typed daemon client (mirrors /openapi.json; regenerate via `pnpm openapi`).
export interface Session {
  id: string;
  workspace_dir: string;
  agent: string;
  permission_mode: string;
  model?: string;
  title?: string;
}

export interface DaemonEvent {
  type: string;
  session_id?: string;
  data?: unknown;
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

async function req<T>(method: string, path: string, body?: unknown): Promise<T> {
  const res = await fetch(apiBase + path, {
    method,
    headers: { 'Content-Type': 'application/json' },
    body: body ? JSON.stringify(body) : undefined,
  });
  if (!res.ok) throw new Error(`${method} ${path}: ${res.status}`);
  return res.json() as Promise<T>;
}

export interface PromptAttachment {
  name: string;
  mime: string;
  data: string; // raw base64
}

// Reasoning-effort variants offered by the composer.
export const EFFORTS = ['', 'low', 'medium', 'high', 'xhigh', 'max'];

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
  createSession: (workspace_dir: string, agent = 'build', permission_mode = 'ask') =>
    req<Session>('POST', '/api/v1/sessions', { workspace_dir, agent, permission_mode }),
  patchSession: (id: string, patch: Partial<Session>) =>
    req<Session>('PATCH', `/api/v1/sessions/${id}`, patch),
  fork: (id: string) => req<Session>('POST', `/api/v1/sessions/${id}/fork`, {}),
  prompt: (id: string, text: string, images?: PromptAttachment[], effort?: string, delivery?: string) =>
    req<{ status: string; position?: number }>('POST', `/api/v1/sessions/${id}/prompt`, {
      text, images: images || [], effort: effort || '', delivery: delivery || '',
    }),
  revert: (id: string) => req<{ status: string }>('POST', `/api/v1/sessions/${id}/revert`, {}),
  unrevert: (id: string) => req<{ status: string }>('POST', `/api/v1/sessions/${id}/unrevert`, {}),
  answerPermission: (reqId: string, decision: string) =>
    req<{ status: string }>('POST', `/api/v1/permissions/${reqId}`, { decision }),
  answerQuestion: (reqId: string, answers: string[][]) =>
    req<{ status: string }>('POST', `/api/v1/questions/${reqId}`, { answers }),
  dismissQuestion: (reqId: string) =>
    req<{ status: string }>('POST', `/api/v1/questions/${reqId}`, { dismissed: true }),
  export: (id: string) =>
    req<{ transcript: { id: string; role: string; text: string }[] }>(`GET`, `/api/v1/sessions/${id}/export`),
  routerState: () => req<{ router: Record<string, string> }>('GET', '/api/v1/router'),
  setRouter: (role: string, model: string) =>
    req<{ status: string }>('PUT', '/api/v1/router', { [role]: model }),
  providers: () => req<{ providers: (string | { name: string; protocol?: string })[] }>('GET', '/api/v1/providers'),
  models: (provider: string) =>
    req<{ models: { id: string; name: string }[] }>(
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
export function subscribe(onEvent: (e: DaemonEvent) => void): () => void {
  let dead = false;
  let delayMs = 1000;
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
