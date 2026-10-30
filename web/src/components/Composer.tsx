import { useEffect, useRef, useState } from 'react';
import { EFFORTS, type PromptAttachment } from '../api/client';

const agents = ['plan', 'build'];
const perms = ['ask', 'auto-edit', 'auto-approve', 'bypass'];

// Centered composer card: attachments, textarea, agent / model / effort /
// permission controls.
export function Composer(props: {
  value: string;
  onChange: (v: string) => void;
  onSend: () => void;
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
  onAttach: (files: FileList | null) => void;
  onRemoveAttachment: (i: number) => void;
  placeholder?: string;
  working?: boolean;
}) {
  const [open, setOpen] = useState<null | 'agent' | 'model' | 'perm' | 'effort'>(null);
  const [custom, setCustom] = useState('');
  const boxRef = useRef<HTMLDivElement>(null);
  const fileRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    const close = (e: MouseEvent) => {
      if (boxRef.current && !boxRef.current.contains(e.target as Node)) setOpen(null);
    };
    window.addEventListener('mousedown', close);
    return () => window.removeEventListener('mousedown', close);
  }, []);

  const menu = 'absolute bottom-9 left-0 z-20 min-w-44 rounded-lg border border-white/10 bg-[#141416] p-1 shadow-xl';
  return (
    <div ref={boxRef} className="rounded-xl border border-white/10 bg-white/[0.03] p-4">
      {props.attachments.length > 0 && (
        <div className="mb-2 flex flex-wrap gap-1.5">
          {props.attachments.map((a, i) => (
            <span key={i} className="flex items-center gap-1 rounded border border-white/10 bg-white/[0.03] px-2 py-0.5 font-mono text-[11px] text-zinc-300" title={`${a.mime}, ${(a.data.length * 3 / 4 / 1024).toFixed(0)}KB`}>
              🖼 {a.name}
              <button className="text-zinc-500 hover:text-zinc-200" onClick={() => props.onRemoveAttachment(i)}>✕</button>
            </span>
          ))}
        </div>
      )}
      <textarea
        className="max-h-48 min-h-10 w-full resize-none bg-transparent text-sm text-zinc-100 outline-none placeholder:text-zinc-500"
        rows={2}
        value={props.value}
        onChange={(e) => props.onChange(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === 'Enter' && !e.shiftKey) {
            e.preventDefault();
            props.onSend();
          }
        }}
        placeholder={props.placeholder ?? 'Ask anything... "Help me debug this issue"'}
      />
      <div className="mt-2 flex items-center gap-1 text-sm">
        <div className="relative">
          <button onClick={() => setOpen(open === 'agent' ? null : 'agent')} className="flex items-center gap-1 rounded px-1.5 py-0.5 capitalize text-zinc-200 hover:bg-white/5">
            {props.agent} <span className="text-xs text-zinc-500">▾</span>
          </button>
          {open === 'agent' && (
            <div className={menu}>
              {agents.map((a) => (
                <button key={a} className="block w-full rounded px-2 py-1.5 text-left capitalize hover:bg-white/5"
                  onClick={() => { props.onAgent(a); setOpen(null); }}>
                  {a}
                </button>
              ))}
            </div>
          )}
        </div>
        <div className="relative">
          <button onClick={() => setOpen(open === 'model' ? null : 'model')} className="flex max-w-48 items-center gap-1 truncate rounded px-1.5 py-0.5 text-zinc-200 hover:bg-white/5" title={props.model}>
            <span className="truncate">{props.model.split('/').pop() || 'model'}</span> <span className="text-xs text-zinc-500">▾</span>
          </button>
          {open === 'model' && (
            <div className={menu}>
              {props.modelOptions.map((m) => (
                <button key={m} className="block w-full truncate rounded px-2 py-1.5 text-left text-zinc-300 hover:bg-white/5"
                  onClick={() => { props.onModel(m); setOpen(null); }} title={m}>
                  {m}
                </button>
              ))}
              <div className="flex items-center gap-1 p-1">
                <input
                  className="w-full rounded border border-white/10 bg-transparent px-2 py-1 text-xs outline-none placeholder:text-zinc-600"
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
          <button onClick={() => setOpen(open === 'effort' ? null : 'effort')} className="flex items-center gap-1 rounded px-1.5 py-0.5 text-zinc-200 hover:bg-white/5" title="Reasoning effort variant">
            <span className="capitalize">{props.effort || 'auto'}</span> <span className="text-xs text-zinc-500">▾</span>
          </button>
          {open === 'effort' && (
            <div className={menu}>
              {EFFORTS.map((e) => (
                <button key={e || 'auto'} className="block w-full rounded px-2 py-1.5 text-left capitalize hover:bg-white/5"
                  onClick={() => { props.onEffort(e); setOpen(null); }}>
                  {e || 'auto'}
                </button>
              ))}
            </div>
          )}
        </div>
        <div className="relative">
          <button onClick={() => setOpen(open === 'perm' ? null : 'perm')} className="flex items-center gap-1 rounded px-1.5 py-0.5 capitalize text-zinc-200 hover:bg-white/5">
            {props.perm === 'ask' ? 'Default' : props.perm}
          </button>
          {open === 'perm' && (
            <div className={menu}>
              {perms.map((p) => (
                <button key={p} className="block w-full rounded px-2 py-1.5 text-left uppercase hover:bg-white/5"
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
        <button onClick={() => fileRef.current?.click()} className="mr-1 rounded px-1.5 py-0.5 text-zinc-400 hover:bg-white/5 hover:text-zinc-200" title="Attach images">
          🖼
        </button>
        <span className="mr-1 text-xs text-zinc-600">{props.working ? 'working…' : ''}</span>
        <button
          onClick={props.onSend}
          className="flex h-7 w-7 items-center justify-center rounded-md bg-white/10 text-zinc-200 hover:bg-white/20"
          title="Send"
        >
          ↑
        </button>
      </div>
    </div>
  );
}
