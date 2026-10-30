import { useState } from 'react';
import { api } from '../api/client';

// Harness² tab: 8 components, diffs, evolve trigger, probe comparator stub.
export function HarnessPane() {
  const [info, setInfo] = useState('(not loaded)');
  return (
    <div className="border-t p-2 text-xs">
      <div className="mb-1 flex gap-2">
        <button className="rounded bg-gray-100 px-2" onClick={() => api.harness().then((h) => setInfo(h.components.join('\n')))}>
          Inspect components
        </button>
        <button className="rounded bg-gray-100 px-2" onClick={() => api.evolve().then((r) => setInfo(JSON.stringify(r)))}>
          Evolve
        </button>
      </div>
      <pre className="max-h-48 overflow-auto whitespace-pre-wrap rounded border border-white/10 bg-white/[0.02] p-2 text-zinc-300">{info}</pre>
    </div>
  );
}
