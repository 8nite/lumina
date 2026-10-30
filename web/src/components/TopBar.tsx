import { useState } from 'react';
import type { Session } from '../api/client';

export function TopBar(props: {
  session: Session | null;
  sessions: Session[];
  onSelect: (s: Session | null) => void;
  onNew: () => void;
  onFork: () => void;
  onExport: () => void;
  onShowServers: () => void;
  onToggleReview: () => void;
  reviewOpen: boolean;
  host: string;
}) {
  const [menu, setMenu] = useState(false);
  return (
    <div className="flex h-12 shrink-0 items-center gap-1 border-b border-white/10 px-2 text-sm">
      <button className="rounded px-2 py-1 text-zinc-400 hover:bg-white/5" title="Menu">
        ☰
      </button>
      <div className="relative">
        <button
          onClick={() => setMenu(!menu)}
          className="flex items-center gap-1 rounded px-2 py-1 text-zinc-200 hover:bg-white/5"
        >
          <span className="max-w-64 truncate">{props.session?.title || props.session?.id || 'New session'}</span>
          <span className="text-xs text-zinc-500">▾</span>
        </button>
        {menu && (
          <div className="absolute left-0 top-9 z-20 w-72 rounded-lg border border-white/10 bg-[#141416] p-1 shadow-xl">
            <button
              className="block w-full rounded px-2 py-1.5 text-left hover:bg-white/5"
              onClick={() => { setMenu(false); props.onNew(); }}
            >
              + New session
            </button>
            {props.session && (
              <>
                <button
                  className="block w-full rounded px-2 py-1.5 text-left hover:bg-white/5"
                  onClick={() => { setMenu(false); props.onFork(); }}
                >
                  Fork session
                </button>
                <button
                  className="block w-full rounded px-2 py-1.5 text-left hover:bg-white/5"
                  onClick={() => { setMenu(false); props.onExport(); }}
                >
                  Export transcript
                </button>
              </>
            )}
            {props.sessions.length > 0 && <div className="my-1 border-t border-white/10" />}
            {props.sessions.map((s) => (
              <button
                key={s.id}
                className="block w-full truncate rounded px-2 py-1.5 text-left text-zinc-400 hover:bg-white/5 hover:text-zinc-200"
                onClick={() => { setMenu(false); props.onSelect(s); }}
              >
                {s.title || s.id} <span className="text-zinc-600">· {s.agent}</span>
              </button>
            ))}
          </div>
        )}
      </div>
      <div className="flex-1" />
      <button
        className="flex items-center gap-1.5 rounded px-2 py-1 text-xs text-zinc-400 hover:bg-white/5"
        onClick={props.onShowServers}
        title="See servers"
      >
        <span className="inline-block h-1.5 w-1.5 rounded-full bg-green-500" />
        <span className="hidden sm:inline">🖥 {props.host}</span>
      </button>
      <button
        className="rounded px-2 py-1 text-zinc-400 hover:bg-white/5"
        onClick={props.onToggleReview}
        title="Toggle side panel"
      >
        {props.reviewOpen ? '◫' : '◧'}
      </button>
      {props.session && (
        <button
          className="rounded px-2 py-1 text-zinc-400 hover:bg-white/5"
          onClick={props.onExport}
          title="Share / export"
        >
          ⤴
        </button>
      )}
    </div>
  );
}
