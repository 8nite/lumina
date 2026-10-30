import { useEffect, useState } from 'react';
import { api } from '../api/client';

// Right pane: tabbed Review / Terminal / Graph / Memory / Harness / Connect.
export function ReviewPane(props: {
  tab: string;
  onTab: (t: string) => void;
  workspace: string;
  term: React.ReactNode;
  graph: React.ReactNode;
  memory: React.ReactNode;
  harness: React.ReactNode;
  connect: React.ReactNode;
}) {
  const tabs = ['review', 'term', 'graph', 'memory', 'harness', 'connect'];
  return (
    <div className="flex h-full min-h-0 flex-col border-l border-white/10">
      <div className="flex shrink-0 items-center border-b border-white/10 text-xs">
        <div className="flex flex-1 gap-0.5 overflow-x-auto px-2">
          {tabs.map((t) => (
            <button
              key={t}
              onClick={() => props.onTab(t)}
              className={`rounded-t px-2.5 py-2 capitalize ${
                props.tab === t ? 'bg-white/5 text-zinc-100' : 'text-zinc-500 hover:text-zinc-300'
              }`}
            >
              {t === 'review' ? <ReviewTitle workspace={props.workspace} /> : t}
            </button>
          ))}
        </div>
        <button className="px-2 text-zinc-600 hover:text-zinc-300" title="New tab">
          +
        </button>
      </div>
      <div className="min-h-0 flex-1 overflow-y-auto">
        {props.tab === 'review' && <ReviewChanges workspace={props.workspace} />}
        {props.tab === 'term' && props.term}
        {props.tab === 'graph' && props.graph}
        {props.tab === 'memory' && props.memory}
        {props.tab === 'harness' && props.harness}
        {props.tab === 'connect' && props.connect}
      </div>
    </div>
  );
}

function ReviewTitle(props: { workspace: string }) {
  const [n, setN] = useState(0);
  useEffect(() => {
    api.diff(props.workspace).then((d) => setN((d.files || []).length)).catch(() => {});
  }, [props.workspace]);
  return (
    <span className="flex items-center gap-1.5">
      <span className="text-zinc-600">▤</span> Review
      {n > 0 && <span className="rounded-full bg-white/10 px-1.5 text-[10px]">{n}</span>}
    </span>
  );
}

interface FileDiff {
  path: string;
  hunks: string[];
  collapsed: boolean;
}

function parseDiff(raw: string): FileDiff[] {
  const files: FileDiff[] = [];
  let cur: FileDiff | null = null;
  for (const line of raw.split('\n')) {
    const m = line.match(/^diff --git a\/(.*) b\/(.*)$/);
    if (m) {
      cur = { path: m[2], hunks: [], collapsed: true };
      files.push(cur);
      continue;
    }
    if (cur) cur.hunks.push(line);
  }
  return files;
}

function ReviewChanges(props: { workspace: string }) {
  const [files, setFiles] = useState<FileDiff[]>([]);
  const [split, setSplit] = useState(false);
  const [loaded, setLoaded] = useState(false);

  const load = () => {
    api
      .diff(props.workspace)
      .then((d) => {
        setFiles(parseDiff(d.diff || ''));
        setLoaded(true);
      })
      .catch(() => setLoaded(true));
  };

  useEffect(() => {
    load();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [props.workspace]);

  const toggleAll = (collapsed: boolean) =>
    setFiles((fs) => fs.map((f) => ({ ...f, collapsed })));

  const renderHunk = (hunk: string[], si: number) => {
    if (!split) {
      return (
        <pre key={si} className="overflow-x-auto whitespace-pre px-3 py-2 font-mono text-[11px] leading-relaxed">
          {hunk.map((l, i) => (
            <div key={i} className={l.startsWith('+') ? 'text-green-300/90' : l.startsWith('-') ? 'text-red-300/90' : 'text-zinc-500'}>
              {l || ' '}
            </div>
          ))}
        </pre>
      );
    }
    const left = hunk.filter((l) => !l.startsWith('+'));
    const right = hunk.filter((l) => !l.startsWith('-') || l.startsWith('+++'));
    return (
      <div key={si} className="grid grid-cols-2 gap-px bg-white/10 font-mono text-[11px] leading-relaxed">
        <pre className="overflow-x-auto whitespace-pre bg-[#0a0a0b] px-3 py-2">
          {left.map((l, i) => (
            <div key={i} className={l.startsWith('-') ? 'text-red-300/90' : 'text-zinc-500'}>{l || ' '}</div>
          ))}
        </pre>
        <pre className="overflow-x-auto whitespace-pre bg-[#0a0a0b] px-3 py-2">
          {right.map((l, i) => (
            <div key={i} className={l.startsWith('+') && !l.startsWith('+++') ? 'text-green-300/90' : 'text-zinc-500'}>{l || ' '}</div>
          ))}
        </pre>
      </div>
    );
  };

  // Group raw lines into hunks by @@ headers.
  const hunksOf = (f: FileDiff): string[][] => {
    const out: string[][] = [];
    let cur: string[] = [];
    for (const l of f.hunks) {
      if (l.startsWith('@@')) {
        if (cur.length) out.push(cur);
        cur = [l];
      } else {
        cur.push(l);
      }
    }
    if (cur.length) out.push(cur);
    return out;
  };

  return (
    <div className="p-3">
      <div className="mb-3 flex items-center justify-between">
        <h2 className="text-sm text-zinc-200">Session changes</h2>
        <div className="flex items-center gap-2 text-xs">
          <div className="flex overflow-hidden rounded-md border border-white/10">
            <button onClick={() => setSplit(false)} className={`px-2.5 py-1 ${!split ? 'bg-white/10 text-zinc-100' : 'text-zinc-500'}`}>
              Unified
            </button>
            <button onClick={() => setSplit(true)} className={`px-2.5 py-1 ${split ? 'bg-white/10 text-zinc-100' : 'text-zinc-500'}`}>
              Split
            </button>
          </div>
          <button onClick={() => toggleAll(false)} className="rounded-md border border-white/10 px-2.5 py-1 text-zinc-300 hover:bg-white/5">
            ⇅ Collapse all
          </button>
          <button onClick={load} className="rounded-md border border-white/10 px-2.5 py-1 text-zinc-300 hover:bg-white/5">
            Reload
          </button>
        </div>
      </div>
      {!loaded && <div className="text-xs text-zinc-600">Loading…</div>}
      {loaded && files.length === 0 && <div className="text-xs text-zinc-600">(no changes)</div>}
      <div className="space-y-2">
        {files.map((f, i) => (
          <div key={f.path} className="overflow-hidden rounded-lg border border-white/10">
            <button
              className="flex w-full items-center gap-2 bg-white/[0.02] px-3 py-2 text-left text-xs"
              onClick={() => setFiles((fs) => fs.map((x, j) => (j === i ? { ...x, collapsed: !x.collapsed } : x)))}
            >
              <span className="text-teal-300">🗎</span>
              <span className="flex-1 truncate font-mono text-zinc-300">{f.path}</span>
              <span className="text-zinc-600">{f.collapsed ? '⌄' : '⌃'}</span>
            </button>
            {!f.collapsed && (
              <div className="border-t border-white/10">
                {hunksOf(f).map((h, si) => <div key={si}>{renderHunk(h, si)}</div>)}
              </div>
            )}
          </div>
        ))}
      </div>
    </div>
  );
}
