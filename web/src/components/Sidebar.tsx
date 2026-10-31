import { useMemo, useState } from 'react';
import type { Session } from '../api/client';

// v2-style vertical tabs sidebar: search + grouped session rows + footer.
// Sessions group by recency when timestamps exist, else by workspace.
export function Sidebar(props: {
  sessions: Session[];
  activeId: string | null;
  onSelect: (s: Session | null) => void;
  onNew: () => void;
  open: boolean;
  width: number;
  host: string;
  onShowServers: () => void;
  scheme: 'light' | 'dark';
  onScheme: (s: 'light' | 'dark') => void;
}) {
  const [q, setQ] = useState('');
  const filtered = useMemo(() => {
    const needle = q.trim().toLowerCase();
    if (!needle) return props.sessions;
    return props.sessions.filter((s) =>
      `${s.title || ''} ${s.id} ${s.workspace_dir} ${s.agent}`.toLowerCase().includes(needle),
    );
  }, [props.sessions, q]);

  const groups = useMemo(() => {
    const byWs = new Map<string, Session[]>();
    for (const s of filtered) {
      const key = s.workspace_dir || '.';
      if (!byWs.has(key)) byWs.set(key, []);
      byWs.get(key)!.push(s);
    }
    return [...byWs.entries()];
  }, [filtered]);

  if (!props.open) return null;
  return (
    <aside
      id="vertical-tabs-sidebar"
      className="relative z-10 h-full min-h-0 shrink-0"
      style={{ width: props.width }}
    >
      <div className="absolute inset-0 flex min-h-0 flex-col bg-[var(--v2-bg-deep)] pb-2 pe-2.5 ps-4 pt-2">
        <div className="mb-2 flex items-center gap-1">
          <span className="v2-title px-1.5 text-[13px]">Lumina</span>
          <div className="flex-1" />
          <button
            className="v2-row px-2 py-1 text-[12px] text-[var(--v2-text-muted)]"
            title="New session"
            onClick={props.onNew}
          >
            ✎
          </button>
        </div>
        <label className="v2-search mb-2 flex h-9 w-full items-center gap-2 px-3 py-1 text-[var(--v2-icon-muted)]">
          <span>⌕</span>
          <input
            className="min-w-0 flex-1 border-0 bg-transparent text-[13px] text-[var(--v2-text-base)] outline-0 placeholder:text-[var(--v2-text-faint)]"
            placeholder="Search sessions"
            value={q}
            onChange={(e) => setQ(e.target.value)}
          />
          {q && (
            <button className="shrink-0" onClick={() => setQ('')} title="Clear">
              ✕
            </button>
          )}
        </label>
        <div className="min-h-0 flex-1 overflow-y-auto" data-scrollable>
          {groups.length === 0 && (
            <div className="px-1.5 py-6 text-center text-[13px] text-[var(--v2-text-muted)]">
              No sessions yet.
              <br />
              <button className="mt-2 underline" onClick={props.onNew}>
                Start a new session
              </button>
            </div>
          )}
          {groups.map(([ws, list]) => (
            <div key={ws} className="mb-4">
              <div className="v2-section-label sticky top-0 bg-[var(--v2-bg-deep)] px-1.5 py-1">
                {ws.split('/').filter(Boolean).pop() || ws}
              </div>
              <div className="flex min-w-0 flex-col gap-px">
                {list.map((s) => {
                  const active = s.id === props.activeId;
                  return (
                    <button
                      key={s.id}
                      onClick={() => props.onSelect(s)}
                      className="v2-row group/session relative flex h-10 min-w-0 w-full items-center gap-1.5 px-1.5 py-3 text-left md:ps-3"
                      style={active ? { background: 'var(--v2-overlay-hover)' } : undefined}
                    >
                      {active && (
                        <span
                          aria-hidden="true"
                          className="pointer-events-none absolute top-1/2 h-3 w-0.5 -translate-y-1/2 rounded-[2px] bg-[var(--v2-text-faint)]"
                          style={{ right: 'calc(100% + 4px)' }}
                        />
                      )}
                      <span
                        className="flex h-4 w-4 shrink-0 items-center justify-center rounded-[4px] text-[10px] text-[var(--v2-text-muted)]"
                        style={{ background: 'var(--v2-bg-layer-02)' }}
                        title={s.workspace_dir}
                      >
                        {(s.workspace_dir.split('/').filter(Boolean).pop() || '·').slice(0, 1).toUpperCase()}
                      </span>
                      <span className="flex min-w-0 flex-1 items-center gap-1.5">
                        <span className="v2-title min-w-0 flex-1 overflow-hidden text-ellipsis whitespace-nowrap text-[13px]">
                          {s.title || s.id.slice(0, 8)}
                        </span>
                        <span className="shrink-0 text-[11px] capitalize text-[var(--v2-text-faint)]">{s.agent}</span>
                      </span>
                    </button>
                  );
                })}
              </div>
            </div>
          ))}
        </div>
        <div className="flex shrink-0 items-center gap-1 pt-2">
          <button
            className="v2-row flex items-center gap-1.5 px-2 py-1 text-[12px] text-[var(--v2-text-muted)]"
            onClick={props.onShowServers}
            title="See servers"
          >
            <span className="inline-block h-1.5 w-1.5 rounded-full bg-green-500" />
            <span className="max-w-32 truncate">{props.host}</span>
          </button>
          <div className="flex-1" />
          <button
            className="v2-row px-2 py-1 text-[12px] text-[var(--v2-text-muted)]"
            onClick={() => props.onScheme(props.scheme === 'dark' ? 'light' : 'dark')}
            title="Toggle color scheme"
          >
            {props.scheme === 'dark' ? '☾' : '☀'}
          </button>
        </div>
      </div>
    </aside>
  );
}
