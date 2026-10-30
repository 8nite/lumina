import { useState } from 'react';
import { Composer } from './Composer';
import { LinkifiedText } from './LinkifiedText';
import type { PromptAttachment } from '../api/client';

export interface Turn {
  role: string;
  text: string;
}

// Active-session timeline: prompt cards, collapsible steps, responses.
export function SessionView(props: {
  turns: Turn[];
  onUndo: (i: number) => void;
  input: string;
  onInput: (v: string) => void;
  onSend: () => void;
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
  const [showSteps, setShowSteps] = useState(true);
  const toolTurns = (i: number) => {
    // Tool pills belonging to the assistant turn ending at/before i.
    const out: { idx: number; t: Turn }[] = [];
    for (let j = i - 1; j >= 0; j--) {
      const t = props.turns[j];
      if (t.role === 'assistant' || t.role === 'user') break;
      out.unshift({ idx: j, t });
    }
    return out;
  };
  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <div className="min-h-0 flex-1 space-y-6 overflow-y-auto px-4 py-6">
        <div className="mx-auto w-full max-w-2xl space-y-6">
          {props.turns.map((t, i) => {
            if (t.role === 'user') {
              return (
                <div key={i}>
                  <div className="rounded-lg border border-white/10 bg-white/[0.03] px-4 py-3 text-sm text-zinc-100">
                    <LinkifiedText text={t.text} />
                  </div>
                  <button
                    className="mt-1 text-xs text-zinc-500 hover:text-zinc-300"
                    onClick={() => props.onUndo(i)}
                  >
                    Undo
                  </button>
                </div>
              );
            }
            if (t.role !== 'assistant') return null;
            const tools = toolTurns(i);
            return (
              <div key={i}>
                {tools.length > 0 && (
                  <button
                    className="mb-2 text-xs text-zinc-500 hover:text-zinc-300"
                    onClick={() => setShowSteps(!showSteps)}
                  >
                    {showSteps ? 'Hide' : 'Show'} steps · {tools.length}
                  </button>
                )}
                {showSteps && tools.length > 0 && (
                  <div className="mb-3 flex flex-wrap gap-1.5">
                    {tools.map(({ idx, t: tt }) => (
                      <span
                        key={idx}
                        title={tt.text.slice(0, 300)}
                        className={`rounded border px-2 py-0.5 font-mono text-[11px] ${
                          tt.role === 'tool_error'
                            ? 'border-red-500/30 text-red-300'
                            : 'border-white/10 text-zinc-400'
                        }`}
                      >
                        {tt.text.split(':')[0]}
                      </span>
                    ))}
                  </div>
                )}
                <div className="mb-1 text-xs text-zinc-500">Response</div>
                <div className="whitespace-pre-wrap text-sm leading-relaxed text-zinc-100">
                  <LinkifiedText text={t.text} />
                </div>
                <button
                  className="mt-1 text-xs text-zinc-600 hover:text-zinc-400"
                  onClick={() => props.onUndo(i)}
                >
                  Undo
                </button>
              </div>
            );
          })}
        </div>
      </div>
      <div className="mx-auto w-full max-w-2xl px-4 pb-6">
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
          placeholder="Ask anything..."
        />
      </div>
    </div>
  );
}
