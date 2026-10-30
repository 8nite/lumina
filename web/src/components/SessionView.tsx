import { useState } from 'react';
import { Composer } from './Composer';
import { LinkifiedText } from './LinkifiedText';
import type { PromptAttachment } from '../api/client';

export interface Turn {
  role: string;
  text: string;
}

export interface LiveStep {
  tool: string;
  target: string;
  phase: 'start' | 'done';
  preview?: string;
  error?: boolean;
  duration_ms?: number;
}

export interface TurnSummary {
  agent?: string;
  model?: string;
  duration_ms?: number;
  output_tokens?: number;
  input_tokens?: number;
  context_tokens?: number;
  max_tokens?: number;
}

function fmtk(n: number): string {
  if (!n || n <= 0) return '0';
  if (n < 1000) return `${n}`;
  return `${(n / 1000).toFixed(1)}k`;
}

export function formatCtx(ctx: { used: number; max: number } | null | undefined): string {
  if (!ctx || !ctx.used) return '';
  if (ctx.max > 0) return `ctx ~${fmtk(ctx.used)}/${fmtk(ctx.max)}`;
  return `ctx ~${fmtk(ctx.used)}`;
}

export function formatSummary(s: TurnSummary | null): string {
  if (!s) return '';
  const parts: string[] = [];
  if (s.model) parts.push(s.model);
  if (s.duration_ms && s.duration_ms > 0) {
    const ms = s.duration_ms;
    parts.push(ms < 1000 ? `${ms}ms` : ms < 60000 ? `${(ms / 1000).toFixed(1)}s` : `${Math.floor(ms / 60000)}m${String(Math.floor((ms % 60000) / 1000)).padStart(2, '0')}s`);
  }
  if (s.output_tokens && s.output_tokens > 0 && s.duration_ms && s.duration_ms > 0) {
    parts.push(`${Math.round(s.output_tokens / (s.duration_ms / 1000))} tok/s`);
  }
  if (s.context_tokens && s.context_tokens > 0) {
    parts.push(formatCtx({ used: s.context_tokens, max: s.max_tokens || 0 }));
  }
  return parts.join(' · ');
}

// Active-session timeline: prompt cards, collapsible steps, responses.
export function SessionView(props: {
  turns: Turn[];
  onUndo: (i: number) => void;
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
  // Live run state: streamed text/thinking, tool steps, status stage,
  // and the just-finished turn summary.
  liveText: string;
  liveThinking: string;
  liveSteps: LiveStep[];
  stage: string;
  summary: TurnSummary | null;
  // Live context gauge + elapsed wall-clock for the in-flight turn.
  ctx: { used: number; max: number } | null;
  elapsedMs: number;
}) {
  const [showSteps, setShowSteps] = useState(true);
  const summaryText = formatSummary(props.summary);
  let lastAssistant = -1;
  props.turns.forEach((t, i) => {
    if (t.role === 'assistant') lastAssistant = i;
  });
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
                <div className="mb-1 text-xs text-zinc-500">Response{i === lastAssistant && summaryText ? ` · ${summaryText}` : ''}</div>
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
          {props.working && (
            <div className="rounded-lg border border-white/10 bg-white/[0.02] px-4 py-3">
              <div className="mb-1 flex items-center gap-2 text-xs text-zinc-500">
                <span className="inline-block h-1.5 w-1.5 animate-pulse rounded-full bg-amber-400" />
                <span>
                  {props.stage || 'working…'}
                  {props.elapsedMs > 0 ? ` · ${Math.floor(props.elapsedMs / 1000)}s` : ''}
                </span>
                {formatCtx(props.ctx) && (
                  <span className="rounded border border-white/10 px-1.5 py-0.5 font-mono text-[11px] text-zinc-400">
                    {formatCtx(props.ctx)}
                  </span>
                )}
              </div>
              {props.liveSteps.length > 0 && (
                <div className="mb-2 flex flex-wrap gap-1.5">
                  {props.liveSteps.map((s, i) => (
                    <span
                      key={i}
                      title={(s.target + (s.preview ? '\n' + s.preview : '')).slice(0, 300)}
                      className={`rounded border px-2 py-0.5 font-mono text-[11px] ${
                        s.error
                          ? 'border-red-500/30 text-red-300'
                          : s.phase === 'done'
                            ? 'border-white/10 text-zinc-400'
                            : 'border-amber-500/30 text-amber-300'
                      }`}
                    >
                      {s.tool} {s.target.slice(0, 60)}
                      {s.phase === 'done' && s.duration_ms != null ? ` · ${s.duration_ms}ms` : ''}
                      {s.phase === 'start' ? ' …' : ''}
                    </span>
                  ))}
                </div>
              )}
              {props.liveThinking && (
                <details className="mb-2 text-xs text-zinc-500">
                  <summary className="cursor-pointer hover:text-zinc-300">thinking</summary>
                  <div className="mt-1 whitespace-pre-wrap">{props.liveThinking.slice(-2000)}</div>
                </details>
              )}
              {props.liveText && (
                <div className="whitespace-pre-wrap text-sm leading-relaxed text-zinc-100">
                  {props.liveText.slice(-4000)}
                  <span className="animate-pulse">▊</span>
                </div>
              )}
            </div>
          )}
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
