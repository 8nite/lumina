import { useState } from 'react';
import { api } from '../api/client';

// Graphify canvas: query vocab-constrained graph + community overview.
export function GraphifyPane() {
  const [q, setQ] = useState('');
  const [out, setOut] = useState('(ask about the codebase or memory)');
  return (
    <div className="border-t p-2 text-xs">
      <form
        className="mb-1 flex gap-1"
        onSubmit={(e) => {
          e.preventDefault();
          api.kbQuery(q).then((r) => setOut(JSON.stringify(r.results)));
        }}
      >
        <input
          className="flex-1 rounded border px-1"
          value={q}
          onChange={(e) => setQ(e.target.value)}
          placeholder="kb query…"
        />
        <button className="rounded bg-gray-100 px-2">Query</button>
      </form>
      <pre className="max-h-48 overflow-auto whitespace-pre-wrap rounded border border-white/10 bg-white/[0.02] p-2 text-zinc-300">{out}</pre>
    </div>
  );
}
