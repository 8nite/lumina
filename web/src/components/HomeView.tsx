import { Composer } from './Composer';
import type { PromptAttachment } from '../api/client';
import type { Session } from '../api/client';

// v2-style draft home: workspace context rows + composer + recent sessions.
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
  onAttach: (files: FileList | File[] | null) => void;
  onRemoveAttachment: (i: number) => void;
  sessions: Session[];
  onOpenSession: (s: Session) => void;
}) {
  const last = props.workspace.split('/').filter(Boolean).pop() || props.workspace;
  const head = props.workspace.slice(0, props.workspace.length - last.length);
  const recent = props.sessions.slice(0, 8);
  return (
    <div className="mx-auto flex w-full max-w-[720px] flex-1 flex-col justify-center px-4 py-8">
      <h1 className="v2-title mb-6 text-[15px]">New session</h1>
      <div className="mb-2 flex items-center gap-3 text-[13px]">
        <span className="w-5 text-center text-[var(--v2-text-faint)]">🗀</span>
        <span className="truncate text-[var(--v2-text-muted)]">
          {head}
          <span className="font-medium text-[var(--v2-text-base)]">{last}</span>
        </span>
      </div>
      <div className="mb-2 flex items-center gap-3 text-[13px]">
        <span className="w-5 text-center text-[var(--v2-text-faint)]">⑂</span>
        <span className="text-[var(--v2-text-muted)]">
          {props.branch ? <>Main branch <span className="text-[var(--v2-text-base)]">({props.branch})</span></> : 'Not a git repo'}
        </span>
      </div>
      <div className="mb-8 flex items-center gap-3 text-[13px]">
        <span className="w-5 text-center text-[var(--v2-text-faint)]">✎</span>
        <span className="text-[var(--v2-text-muted)]">Last modified <span className="text-[var(--v2-text-base)]">{props.modified || '—'}</span></span>
      </div>
      <div className="mb-3 flex items-center gap-2">
        <span className="text-[12px] text-[var(--v2-text-muted)]">Workspace:</span>
        <input
          className="flex-1 rounded-[6px] border border-[var(--v2-border-base)] bg-transparent px-2 py-1 text-[12px] text-[var(--v2-text-base)] outline-none"
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
      {recent.length > 0 && (
        <div className="mt-8">
          <div className="v2-section-label mb-2 px-1.5">Recent sessions</div>
          <div className="flex flex-col gap-px">
            {recent.map((s) => (
              <button
                key={s.id}
                onClick={() => props.onOpenSession(s)}
                className="v2-row flex h-10 min-w-0 items-center gap-1.5 px-1.5 py-3 text-left md:ps-3"
              >
                <span
                  className="flex h-4 w-4 shrink-0 items-center justify-center rounded-[4px] text-[10px] text-[var(--v2-text-muted)]"
                  style={{ background: 'var(--v2-bg-layer-02)' }}
                >
                  {(s.workspace_dir.split('/').filter(Boolean).pop() || '·').slice(0, 1).toUpperCase()}
                </span>
                <span className="min-w-0 flex-1 truncate text-[13px] font-medium text-[var(--v2-text-base)]">
                  {s.title || s.id.slice(0, 8)}
                </span>
                <span className="shrink-0 text-[11px] capitalize text-[var(--v2-text-faint)]">{s.agent}</span>
              </button>
            ))}
          </div>
        </div>
      )}
    </div>
  );
}
