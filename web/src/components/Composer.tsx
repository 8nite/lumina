import { useEffect, useRef, useState } from 'react';
import { EFFORTS, type PromptAttachment } from '../api/client';

const agents = ['plan', 'build'];
const perms = ['ask', 'auto-edit', 'auto-approve', 'bypass'];

// v2-style composer: layered card, activity chips, footer pills.
export function Composer(props: {
  value: string;
  onChange: (v: string) => void;
  onSend: (delivery?: 'steer' | 'queue') => void;
  agent: string;
  onAgent: (a: string) => void;
  model: string;
  modelOptions: string[];
  onModel: (m: string) => void;
  onCustomModel: (m: string) => void;
  perm: string;
  onPerm: (p: string) => void;
  effort: string;
  onEffort: (e: string) => void;
  attachments: PromptAttachment[];
  onAttach: (files: FileList | File[] | null) => void;
  onRemoveAttachment: (i: number) => void;
  placeholder?: string;
  working?: boolean;
}) {
  const [open, setOpen] = useState<null | 'agent' | 'model' | 'perm' | 'effort'>(null);
  const [custom, setCustom] = useState('');
  const [queueArmed, setQueueArmed] = useState(false);
  const boxRef = useRef<HTMLDivElement>(null);
  const fileRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    const close = (e: MouseEvent) => {
      if (boxRef.current && !boxRef.current.contains(e.target as Node)) setOpen(null);
    };
    window.addEventListener('mousedown', close);
    return () => window.removeEventListener('mousedown', close);
  }, []);

  const menu =
    'absolute bottom-9 left-0 z-20 min-w-44 rounded-[12px] bg-[var(--v2-bg-base)] p-1 shadow-[var(--v2-elevation-floating)]';
  return (
    <div
      ref={boxRef}
      className="rounded-[12px] bg-[var(--v2-bg-base)] p-4 shadow-[var(--v2-elevation-raised)]"
      style={{ border: '1px solid var(--v2-border-base)', scrollbarGutter: 'stable' }}
    >
      {(props.attachments.length > 0 || props.working || queueArmed) && (
        <div className="mb-2 flex flex-wrap items-center gap-1.5">
          {props.attachments.map((a, i) => (
            <span key={i} className="flex items-center gap-1 rounded-[6px] border border-[var(--v2-border-base)] bg-[var(--v2-bg-layer-02)] px-2 py-0.5 font-mono text-[11px] text-[var(--v2-text-base)]" title={`${a.mime}, ${(a.data.length * 3 / 4 / 1024).toFixed(0)}KB`}>
              🖼 {a.name}
              <button className="text-[var(--v2-text-muted)] hover:text-[var(--v2-text-base)]" onClick={() => props.onRemoveAttachment(i)}>✕</button>
            </span>
          ))}
          {props.working && (
            <span className="rounded-[6px] border border-[var(--v2-border-base)] px-2 py-0.5 text-[11px] text-[var(--v2-text-muted)]">
              working… (enter queues)
            </span>
          )}
          {queueArmed && (
            <span className="rounded-[6px] border border-[var(--v2-border-base)] px-2 py-0.5 text-[11px] text-[var(--v2-text-muted)]">
              will queue — Enter to queue, Esc to cancel
            </span>
          )}
        </div>
      )}
      <textarea
        className="max-h-48 min-h-10 w-full resize-none bg-transparent text-[13px] leading-5 text-[var(--v2-text-base)] outline-none placeholder:text-[var(--v2-text-faint)]"
        rows={2}
        value={props.value}
        onChange={(e) => props.onChange(e.target.value)}
        onPaste={(e) => {
          const items = e.clipboardData?.items;
          const files: File[] = [];
          if (items) {
            for (const item of Array.from(items)) {
              if (item.kind === 'file' && item.type.startsWith('image/')) {
                const f = item.getAsFile();
                if (f) files.push(f);
              }
            }
          }
          if (files.length === 0 && e.clipboardData?.files?.length) {
            for (const f of Array.from(e.clipboardData.files)) {
              if (f.type.startsWith('image/')) files.push(f);
            }
          }
          if (files.length > 0) {
            // Don't preventDefault: let any accompanying text paste through.
            props.onAttach(files);
          }
        }}
        onKeyDown={(e) => {
          if (e.key === 'x' && e.ctrlKey) {
            e.preventDefault();
            setQueueArmed(true);
            return;
          }
          if (e.key === 'Escape') {
            setQueueArmed(false);
            return;
          }
          if (e.key === 'Enter' && !e.shiftKey) {
            e.preventDefault();
            const q = queueArmed || props.working;
            setQueueArmed(false);
            props.onSend(q ? 'queue' : 'steer');
          }
        }}
        placeholder={props.placeholder ?? 'Ask anything... "Help me debug this issue"'}
      />
      <div className="mt-2 flex items-center gap-1 text-[13px]">
        <div className="relative">
          <button onClick={() => setOpen(open === 'agent' ? null : 'agent')} className="v2-row flex items-center gap-1 px-1.5 py-0.5 capitalize text-[var(--v2-text-base)]" title="Primary agent (Tab)">
            {props.agent} <span className="text-[11px] text-[var(--v2-text-faint)]">▾</span>
          </button>
          {open === 'agent' && (
            <div className={menu} style={{ border: '1px solid var(--v2-border-base)' }}>
              {agents.map((a) => (
                <button key={a} className="v2-row block w-full rounded-[6px] px-2 py-1.5 text-left capitalize"
                  onClick={() => { props.onAgent(a); setOpen(null); }}>
                  {a}
                </button>
              ))}
            </div>
          )}
        </div>
        <div className="relative">
          <button onClick={() => setOpen(open === 'model' ? null : 'model')} className="v2-row flex max-w-48 items-center gap-1 truncate px-1.5 py-0.5 text-[var(--v2-text-base)]" title={props.model}>
            <span className="truncate">{props.model.split('/').pop() || 'model'}</span> <span className="text-[11px] text-[var(--v2-text-faint)]">▾</span>
          </button>
          {open === 'model' && (
            <div className={menu} style={{ border: '1px solid var(--v2-border-base)' }}>
              {props.modelOptions.map((m) => (
                <button key={m} className="v2-row block w-full truncate rounded-[6px] px-2 py-1.5 text-left text-[var(--v2-text-base)]"
                  onClick={() => { props.onModel(m); setOpen(null); }} title={m}>
                  {m}
                </button>
              ))}
              <div className="flex items-center gap-1 p-1">
                <input
                  className="w-full rounded-[6px] border border-[var(--v2-border-base)] bg-transparent px-2 py-1 text-xs outline-none placeholder:text-[var(--v2-text-faint)]"
                  placeholder="provider/model…"
                  value={custom}
                  onChange={(e) => setCustom(e.target.value)}
                  onKeyDown={(e) => {
                    if (e.key === 'Enter' && custom.trim()) {
                      props.onCustomModel(custom.trim());
                      setCustom('');
                      setOpen(null);
                    }
                  }}
                />
              </div>
            </div>
          )}
        </div>
        <div className="relative">
          <button onClick={() => setOpen(open === 'effort' ? null : 'effort')} className="v2-row flex items-center gap-1 px-1.5 py-0.5 text-[var(--v2-text-base)]" title="Reasoning effort variant">
            <span className="capitalize">{props.effort || 'auto'}</span> <span className="text-[11px] text-[var(--v2-text-faint)]">▾</span>
          </button>
          {open === 'effort' && (
            <div className={menu} style={{ border: '1px solid var(--v2-border-base)' }}>
              {EFFORTS.map((e) => (
                <button key={e || 'auto'} className="v2-row block w-full rounded-[6px] px-2 py-1.5 text-left capitalize"
                  onClick={() => { props.onEffort(e); setOpen(null); }}>
                  {e || 'auto'}
                </button>
              ))}
            </div>
          )}
        </div>
        <div className="relative">
          <button onClick={() => setOpen(open === 'perm' ? null : 'perm')} className="v2-row flex items-center gap-1 px-1.5 py-0.5 capitalize text-[var(--v2-text-base)]" title="Permission mode (Shift+Tab)">
            {props.perm === 'bypass' ? 'Default' : props.perm}
          </button>
          {open === 'perm' && (
            <div className={menu} style={{ border: '1px solid var(--v2-border-base)' }}>
              {perms.map((p) => (
                <button key={p} className="v2-row block w-full rounded-[6px] px-2 py-1.5 text-left uppercase"
                  onClick={() => { props.onPerm(p); setOpen(null); }}>
                  {p}
                </button>
              ))}
            </div>
          )}
        </div>
        <div className="flex-1" />
        <input
          ref={fileRef}
          type="file"
          accept="image/*"
          multiple
          className="hidden"
          onChange={(e) => {
            props.onAttach(e.target.files);
            if (fileRef.current) fileRef.current.value = '';
          }}
        />
        <button onClick={() => fileRef.current?.click()} className="v2-row mr-1 px-1.5 py-0.5 text-[var(--v2-text-muted)] hover:text-[var(--v2-text-base)]" title="Attach images">
          🖼
        </button>
        <button
          onClick={() => { const q = queueArmed || props.working; setQueueArmed(false); props.onSend(q ? 'queue' : 'steer'); }}
          className="flex h-7 w-7 items-center justify-center rounded-[6px] text-[var(--v2-text-base)] hover:bg-[var(--v2-overlay-hover)]"
          style={{ background: 'var(--v2-bg-layer-02)' }}
          title="Send"
        >
          ↑
        </button>
      </div>
    </div>
  );
}
