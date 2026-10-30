import { Composer } from './Composer';
import type { PromptAttachment } from '../api/client';

// Centered new-session view: workspace context rows + composer.
// Typing in the composer and sending creates the session.
export function HomeView(props: {
  workspace: string;
  onWorkspace: (w: string) => void;
  branch: string;
  modified: string;
  input: string;
  onInput: (v: string) => void;
  onSend: (delivery?: 'steer' | 'queue') => void;
  agent: string;
  onAgent: (a: string) => void;
  model: string;
  modelOptions: string[];
  onModel: (m: string) => void;
  onCustomModel: (m: string) => void;
  perm: string;
  onPerm: (p: string) => void;
  working: boolean;
  effort: string;
  onEffort: (e: string) => void;
  attachments: PromptAttachment[];
  onAttach: (f: FileList | null) => void;
  onRemoveAttachment: (i: number) => void;
}) {
  const last = props.workspace.split('/').filter(Boolean).pop() || props.workspace;
  const head = props.workspace.slice(0, props.workspace.length - last.length);
  return (
    <div className="mx-auto flex w-full max-w-2xl flex-1 flex-col justify-end px-4 pb-6">
      <h1 className="mb-6 text-xl text-zinc-300">New session</h1>
      <div className="mb-2 flex items-center gap-3 text-sm">
        <span className="w-5 text-center text-zinc-500">🗀</span>
        <span className="truncate text-zinc-500">
          {head}
          <span className="font-semibold text-zinc-100">{last}</span>
        </span>
      </div>
      <div className="mb-2 flex items-center gap-3 text-sm">
        <span className="w-5 text-center text-zinc-500">⑂</span>
        <span className="text-zinc-400">
          {props.branch ? <>Main branch <span className="text-zinc-100">({props.branch})</span></> : 'Not a git repo'}
        </span>
      </div>
      <div className="mb-8 flex items-center gap-3 text-sm">
        <span className="w-5 text-center text-zinc-500">✎</span>
        <span className="text-zinc-500">Last modified <span className="text-zinc-100">{props.modified || '—'}</span></span>
      </div>
      <div className="mb-3 flex items-center gap-2">
        <span className="text-xs text-zinc-500">Workspace:</span>
        <input
          className="flex-1 rounded border border-white/10 bg-transparent px-2 py-1 text-xs text-zinc-300 outline-none"
          value={props.workspace}
          onChange={(e) => props.onWorkspace(e.target.value)}
          spellCheck={false}
        />
      </div>
      <Composer
        value={props.input}
        onChange={props.onInput}
        onSend={props.onSend}
        agent={props.agent}
        onAgent={props.onAgent}
        model={props.model}
        modelOptions={props.modelOptions}
        onModel={props.onModel}
        onCustomModel={props.onCustomModel}
        perm={props.perm}
        onPerm={props.onPerm}
        effort={props.effort}
        onEffort={props.onEffort}
        attachments={props.attachments}
        onAttach={props.onAttach}
        onRemoveAttachment={props.onRemoveAttachment}
        working={props.working}
      />
    </div>
  );
}
