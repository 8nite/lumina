import { useEffect, useState } from 'react';
import { api } from '../api/client';

// v2-style side region: tabbed Review / Terminal / Graph / Memory /
// Harness / Connect inside the session panel frame.
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
    <div className="flex h-full min-h-0 flex-col">
      <div className="flex shrink-0 items-center border-b border-[var(--v2-border-base)] text-[12px]">
        <div className="flex flex-1 gap-0.5 overflow-x-auto px-2">
          {tabs.map((t) => (
            <button
              key={t}
              onClick={() => props.onTab(t)}
              className="rounded-t px-2.5 py-2 capitalize"
              style={
                props.tab === t
                  ? { background: 'var(--v2-overlay-hover)', color: 'var(--v2-text-base)' }
                  : { color: 'var(--v2-text-muted)' }
              }
            >
              {t === 'review' ? <ReviewTitle workspace={props.workspace} /> : t}
            </button>
          ))}
        </div>
      </div>
      <div className="min-h-0 flex-1 overflow-y-auto" data-scrollable>
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
      <span className="text-[var(--v2-text-faint)]">▤</span> Review
      {n > 0 && (
        <span
          className="rounded-full px-1.5 text-[10px]"
          style={{ background: 'var(--v2-bg-layer-02)', color: 'var(--v2-text-muted)' }}
        >
          {n}
        </span>
      )}
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
            <div key={i} className={l.startsWith('+') ? 'text-green-600 dark:text-green-300/90' : l.startsWith('-') ? 'text-red-600 dark:text-red-300/90' : 'text-[var(--v2-text-muted)]'}>
              {l || ' '}
            </div>
          ))}
        </pre>
      );
    }
    const left = hunk.filter((l) => !l.startsWith('+'));
    const right = hunk.filter((l) => !l.startsWith('-') || l.startsWith('+++'));
    return (
      <div key={si} className="grid grid-cols-2 gap-px bg-[var(--v2-border-base)] font-mono text-[11px] leading-relaxed">
        <pre className="overflow-x-auto whitespace-pre bg-[var(--v2-bg-base)] px-3 py-2">
          {left.map((l, i) => (
            <div key={i} className={l.startsWith('-') ? 'text-red-600 dark:text-red-300/90' : 'text-[var(--v2-text-muted)]'}>{l || ' '}</div>
          ))}
        </pre>
        <pre className="overflow-x-auto whitespace-pre bg-[var(--v2-bg-base)] px-3 py-2">
          {right.map((l, i) => (
            <div key={i} className={l.startsWith('+') && !l.startsWith('+++') ? 'text-green-600 dark:text-green-300/90' : 'text-[var(--v2-text-muted)]'}>{l || ' '}</div>
          ))}
        </pre>
      </div>
    );
  };

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
        <h2 className="text-[13px] font-medium text-[var(--v2-text-base)]">Session changes</h2>
        <div className="flex items-center gap-2 text-[12px]">
          <div className="flex overflow-hidden rounded-[6px] border border-[var(--v2-border-base)]">
            <button onClick={() => setSplit(false)} className="px-2.5 py-1" style={!split ? { background: 'var(--v2-overlay-hover)', color: 'var(--v2-text-base)' } : { color: 'var(--v2-text-muted)' }}>
              Unified
            </button>
            <button onClick={() => setSplit(true)} className="px-2.5 py-1" style={split ? { background: 'var(--v2-overlay-hover)', color: 'var(--v2-text-base)' } : { color: 'var(--v2-text-muted)' }}>
              Split
            </button>
          </div>
          <button onClick={() => toggleAll(false)} className="v2-row rounded-[6px] border border-[var(--v2-border-base)] px-2.5 py-1 text-[var(--v2-text-base)]">
            ⇅ Collapse all
          </button>
          <button onClick={load} className="v2-row rounded-[6px] border border-[var(--v2-border-base)] px-2.5 py-1 text-[var(--v2-text-base)]">
            Reload
          </button>
        </div>
      </div>
      {!loaded && <div className="text-[12px] text-[var(--v2-text-faint)]">Loading…</div>}
      {loaded && files.length === 0 && <div className="text-[12px] text-[var(--v2-text-faint)]">(no changes)</div>}
      <div className="space-y-2">
        {files.map((f, i) => (
          <div key={f.path} className="overflow-hidden rounded-[8px] border border-[var(--v2-border-base)]">
            <button
              className="flex w-full items-center gap-2 bg-[var(--v2-bg-layer-01)] px-3 py-2 text-left text-[12px]"
              onClick={() => setFiles((fs) => fs.map((x, j) => (j === i ? { ...x, collapsed: !x.collapsed } : x)))}
            >
              <span>🗎</span>
              <span className="flex-1 truncate font-mono text-[var(--v2-text-base)]">{f.path}</span>
              <span className="text-[var(--v2-text-faint)]">{f.collapsed ? '⌄' : '⌃'}</span>
            </button>
            {!f.collapsed && (
              <div className="border-t border-[var(--v2-border-base)]">
                {hunksOf(f).map((h, si) => <div key={si}>{renderHunk(h, si)}</div>)}
              </div>
            )}
          </div>
        ))}
      </div>
    </div>
  );
}
