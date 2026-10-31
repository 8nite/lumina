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
        className="w-full max-w-lg rounded-[12px] bg-[var(--v2-bg-base)] p-5 shadow-[var(--v2-elevation-floating)]"
        style={{ border: '1px solid var(--v2-border-base)' }}
        onClick={(e) => e.stopPropagation()}
      >
        <div className="mb-1 flex items-center justify-between">
          <h2 className="v2-title text-[13px]">Servers</h2>
          <button className="text-[var(--v2-text-muted)] hover:text-[var(--v2-text-base)]" onClick={props.onClose}>
            ✕
          </button>
        </div>
        <p className="mb-3 text-[12px] text-[var(--v2-text-muted)]">Switch which Lumina server this app connects to.</p>
        <div className="v2-search mb-2 flex items-center gap-2 px-2">
          <span className="text-[var(--v2-text-faint)]">🔍</span>
          <input
            className="w-full bg-transparent py-2 text-[13px] outline-none placeholder:text-[var(--v2-text-faint)]"
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
              className="v2-row flex w-full items-center gap-2 border border-[var(--v2-border-base)] px-3 py-2 text-left text-[13px]"
            >
              <span className="inline-block h-1.5 w-1.5 rounded-full bg-green-500" />
              <span className="text-[var(--v2-text-base)]">{u.replace(/^https?:\/\//, '')}</span>
              <span className="text-[var(--v2-text-muted)]">local</span>
              <span className="flex-1" />
              {props.active === u && <span>✓</span>}
            </button>
          ))}
          {shown.length === 0 && <div className="text-[12px] text-[var(--v2-text-faint)]">No saved servers match.</div>}
        </div>
        <div className="text-[12px] text-[var(--v2-text-muted)]">Add a server</div>
        <div className="mt-1 flex gap-2">
          <input
            className="flex-1 rounded-[6px] border border-[var(--v2-border-base)] bg-transparent px-3 py-2 text-[13px] outline-none placeholder:text-[var(--v2-text-faint)]"
            placeholder="http://localhost:7432"
            value={draft}
            onChange={(e) => setDraft(e.target.value)}
            onKeyDown={(e) => e.key === 'Enter' && add()}
          />
          <button onClick={add} className="v2-row rounded-[6px] border border-[var(--v2-border-base)] px-3 text-[13px]">
            + Add
          </button>
        </div>
      </div>
    </div>
  );
}
