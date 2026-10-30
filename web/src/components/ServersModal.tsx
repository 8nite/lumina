import { useState } from 'react';
import { loadServers, saveServers } from '../api/client';

// "See Servers" modal: switch which daemon this app connects to.
export function ServersModal(props: { active: string; onPick: (url: string) => void; onClose: () => void }) {
  const [list, setList] = useState<string[]>(() => loadServers());
  const [q, setQ] = useState('');
  const [draft, setDraft] = useState('http://localhost:7432');

  const add = () => {
    const u = draft.trim();
    if (!u) return;
    const next = list.includes(u) ? list : [...list, u];
    setList(next);
    saveServers(next);
    setDraft('');
  };

  const shown = list.filter((u) => u.includes(q));
  return (
    <div className="fixed inset-0 z-30 flex items-start justify-center bg-black/60 pt-32" onClick={props.onClose}>
      <div
        className="w-full max-w-lg rounded-xl border border-white/10 bg-[#141416] p-5 shadow-2xl"
        onClick={(e) => e.stopPropagation()}
      >
        <div className="mb-1 flex items-center justify-between">
          <h2 className="text-sm text-zinc-100">Servers</h2>
          <button className="text-zinc-500 hover:text-zinc-300" onClick={props.onClose}>
            ✕
          </button>
        </div>
        <p className="mb-3 text-xs text-zinc-400">Switch which Lumina server this app connects to.</p>
        <div className="mb-2 flex items-center gap-2 rounded-md border border-white/10 bg-white/[0.02] px-2">
          <span className="text-zinc-500">🔍</span>
          <input
            className="w-full bg-transparent py-2 text-sm outline-none placeholder:text-zinc-600"
            placeholder="Search servers"
            value={q}
            onChange={(e) => setQ(e.target.value)}
          />
        </div>
        <div className="mb-4 space-y-1">
          {shown.map((u) => (
            <button
              key={u}
              onClick={() => { props.onPick(u); props.onClose(); }}
              className="flex w-full items-center gap-2 rounded-md border border-white/10 bg-white/[0.03] px-3 py-2 text-left text-sm hover:bg-white/[0.06]"
            >
              <span className="inline-block h-1.5 w-1.5 rounded-full bg-green-500" />
              <span className="text-zinc-100">{u.replace(/^https?:\/\//, '')}</span>
              <span className="text-zinc-500">local</span>
              <span className="flex-1" />
              {props.active === u && <span className="text-zinc-300">✓</span>}
            </button>
          ))}
          {shown.length === 0 && <div className="text-xs text-zinc-600">No saved servers match.</div>}
        </div>
        <div className="text-xs text-zinc-400">Add a server</div>
        <div className="mt-1 flex gap-2">
          <input
            className="flex-1 rounded-md border border-white/10 bg-transparent px-3 py-2 text-sm outline-none placeholder:text-zinc-600"
            placeholder="http://localhost:7432"
            value={draft}
            onChange={(e) => setDraft(e.target.value)}
            onKeyDown={(e) => e.key === 'Enter' && add()}
          />
          <button onClick={add} className="rounded-md border border-white/10 px-3 text-sm text-zinc-200 hover:bg-white/5">
            + Add
          </button>
        </div>
      </div>
    </div>
  );
}
