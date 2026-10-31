import { useState } from 'react';
import { Composer } from './Composer';
import { LinkifiedText } from './LinkifiedText';
import type { PromptAttachment } from '../api/client';

export interface Turn {
  role: string;
  text: string;
  agent?: string;
  model?: string;
  created_at?: string;
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

export interface QueuedPrompt {
  text: string;
  files: string[];
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

// --- v2-style conversation rows -------------------------------------------
// Row model mirrors opencode v2 session-ui/timeline rows: user message,
// grouped tool calls (read group collapsed, shell monospace, errors open),
// thinking, error card, turn diff. Data comes from Lumina's transcript
// (tool turns are `name: output`) + live SSE steps.

const READ_TOOLS = new Set(['read', 'glob', 'grep', 'lsp', 'kb_query', 'skill']);

function parseTool(text: string): { name: string; body: string } {
  const i = text.indexOf(':');
  if (i < 0) return { name: text.trim() || 'tool', body: '' };
  return { name: text.slice(0, i).trim() || 'tool', body: text.slice(i + 1).trimStart() };
}

function oneLine(s: string, n = 120): string {
  const line = s.split('\n')[0].trim();
  return line.length > n ? line.slice(0, n) + '…' : line;
}

function shortModel(m?: string): string {
  if (!m) return '';
  return m.includes('/') ? m.split('/').pop()! : m;
}

// ponytail: categorical dot colors from the --c-* tokens in styles.css.
function toolColor(name: string): string {
  switch (name) {
    case 'read':
    case 'glob':
    case 'grep':
    case 'lsp':
    case 'skill':
    case 'kb_query':
      return 'var(--c-read)';
    case 'bash':
    case 'task_output':
      return 'var(--c-shell)';
    case 'edit':
    case 'write':
    case 'patch':
      return 'var(--c-edit)';
    case 'subagent':
    case 'mcp':
      return 'var(--c-agent)';
    case 'question':
      return 'var(--c-warn)';
    case 'websearch':
    case 'webfetch':
      return 'var(--c-search)';
    case 'todowrite':
    case 'memory_write':
      return 'var(--c-mem)';
    default:
      return 'var(--c-misc)';
  }
}

function fmtTime(iso?: string): string {
  if (!iso) return '';
  const d = new Date(iso);
  if (isNaN(d.getTime())) return '';
  return d.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
}

function ToolRow(props: { name: string; body: string; error: boolean; live?: boolean; duration_ms?: number }) {
  const shell = props.name === 'bash' || props.name === 'task_output';
  const [open, setOpen] = useState(props.error || shell);
  const summary = oneLine(props.body) || (props.live ? 'running…' : '(no output)');
  return (
    <div className="rounded-[6px] border border-[var(--v2-border-base)]">
      <button
        className="flex w-full items-center gap-1.5 px-2 py-1 text-left"
        onClick={() => setOpen(!open)}
        title={props.body.slice(0, 300) || props.name}
      >
        <span
          className={`inline-block h-1.5 w-1.5 shrink-0 rounded-full ${props.error ? 'bg-red-500' : props.live ? 'animate-pulse bg-amber-400' : ''}`}
          style={!props.error && !props.live ? { background: toolColor(props.name) } : undefined}
        />
        <span className="shrink-0 font-mono text-[11px] font-medium" style={{ color: props.error ? '#ef4444' : toolColor(props.name) }}>{props.name}</span>
        <span
          dir="auto"
          className={`min-w-0 flex-1 truncate text-[11px] ${shell ? 'font-mono text-[var(--v2-text-base)]' : 'text-[var(--v2-text-muted)]'}`}
          style={shell ? { fontWeight: 440, letterSpacing: '-0.04px' } : undefined}
        >
          {summary}
        </span>
        {props.duration_ms != null && (
          <span className="shrink-0 font-mono text-[11px] tabular-nums text-[var(--v2-text-faint)]">{props.duration_ms}ms</span>
        )}
        <span className="shrink-0 text-[11px] text-[var(--v2-text-faint)]">{open ? '⌃' : '⌄'}</span>
      </button>
      {open && props.body && (
        <pre className="max-h-48 overflow-y-auto whitespace-pre-wrap border-t border-[var(--v2-border-base)] px-2 py-1.5 font-mono text-[11px] leading-relaxed text-[var(--v2-text-muted)]">
          {props.body.slice(0, 2000)}
        </pre>
      )}
    </div>
  );
}

function ToolGroup(props: { tools: { idx: number; t: Turn }[] }) {
  const [show, setShow] = useState(false);
  const parsed = props.tools.map(({ idx, t }) => ({ idx, error: t.role === 'tool_error', ...parseTool(t.text) }));
  const allRead = parsed.length > 1 && parsed.every((p) => READ_TOOLS.has(p.name));
  if (allRead && !show) {
    return (
      <button
        className="v2-row flex items-center gap-1.5 rounded-[6px] border border-[var(--v2-border-base)] px-2 py-1 text-[11px] text-[var(--v2-text-muted)]"
        onClick={() => setShow(true)}
      >
        <span className="font-mono">{parsed.length} reads</span>
        <span className="truncate">{parsed.map((p) => oneLine(p.body, 40)).join(' · ').slice(0, 100)}</span>
      </button>
    );
  }
  return (
    <div className="space-y-1">
      {allRead && (
        <button className="text-[11px] text-[var(--v2-text-faint)] hover:text-[var(--v2-text-muted)]" onClick={() => setShow(false)}>
          Collapse reads
        </button>
      )}
      {parsed.map((p) => (
        <ToolRow key={p.idx} name={p.name} body={p.body} error={p.error} />
      ))}
    </div>
  );
}

// Active turn's plan as one capsule in the composer lane (v2 todo-pill):
// derived from the latest todowrite tool turn
// ("Todos updated: 1/3 completed… In progress: …").
function TodoPill(props: { turns: Turn[] }) {
  const [open, setOpen] = useState(false);
  let raw: string | null = null;
  for (let i = props.turns.length - 1; i >= 0; i--) {
    const t = props.turns[i];
    if ((t.role === 'tool' || t.role === 'tool_error') && parseTool(t.text).name === 'todowrite') {
      raw = parseTool(t.text).body;
      break;
    }
  }
  if (!raw) return null;
  const done = raw.match(/(\d+)\s*\/\s*(\d+)\s*completed/);
  const active = raw.match(/In progress:\s*(.+)/)?.[1]?.split(',')[0]?.trim();
  if (!done && !active) return null;
  return (
    <div className="flex min-w-0 shrink items-center">
      <button
        className="v2-row flex min-w-0 items-center gap-1.5 rounded-full border border-[var(--v2-border-base)] px-2.5 py-1 text-[12px]"
        style={{ background: 'var(--v2-bg-base)' }}
        onClick={() => setOpen(!open)}
        title={raw.slice(0, 300)}
      >
        <svg viewBox="0 0 16 16" width="14" height="14" fill="none" aria-hidden="true" className="shrink-0 text-[var(--v2-text-muted)]">
          <circle cx="8" cy="8" r="6.5" stroke="currentColor" />
          {active ? (
            <path d="M8 4a4 4 0 0 0 0 8V4Z" fill="currentColor" />
          ) : (
            <path d="M5 8 7.5 11 11 5.5" stroke="currentColor" strokeLinecap="round" strokeLinejoin="round" />
          )}
        </svg>
        {done && <span className="shrink-0 tabular-nums text-[var(--v2-text-muted)]">{done[1]}/{done[2]}</span>}
        {active && <span dir="auto" className="min-w-0 max-w-[260px] truncate text-[var(--v2-text-base)]">{active}</span>}
      </button>
      {open && (
        <div className="absolute bottom-full mb-1 max-w-80 rounded-[8px] bg-[var(--v2-bg-base)] p-2 text-[12px] shadow-[var(--v2-elevation-floating)]" style={{ border: '1px solid var(--v2-border-base)' }}>
          {raw.slice(0, 500)}
        </div>
      )}
    </div>
  );
}

// Active-session conversation: v2 timeline rows, working lane, queue panel,
// composer lane (activity + todo capsules), composer.
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
  onAttach: (f: FileList | File[] | null) => void;
  onRemoveAttachment: (i: number) => void;
  liveText: string;
  liveThinking: string;
  liveSteps: LiveStep[];
  stage: string;
  summary: TurnSummary | null;
  ctx: { used: number; max: number } | null;
  elapsedMs: number;
  // v2 working-event formats: terminal error, per-turn file changes, queued prompts.
  error: string | null;
  onRetry: () => void;
  changed: { files: string[]; stat: string } | null;
  queue: QueuedPrompt[];
}) {
  const [showAllChanged, setShowAllChanged] = useState(false);
  const [copied, setCopied] = useState(false);
  const summaryText = formatSummary(props.summary);
  let lastAssistant = -1;
  props.turns.forEach((t, i) => {
    if (t.role === 'assistant') lastAssistant = i;
  });
  // Forward grouping: consecutive tool/tool_error turns form one group
  // rendered in place, so the timeline follows transcript insertion order
  // (assistant reasoning between tool rounds stays where it happened
  // instead of collapsing into one block above the final text).
  const isToolTurn = (t: Turn) => t.role === 'tool' || t.role === 'tool_error';
  // Capsule counts for the composer lane, from live (still-running) steps.
  const running = props.liveSteps.filter((s) => s.phase === 'start');
  const runningShells = running.filter((s) => s.tool === 'bash' || s.tool === 'task_output').length;
  const runningAgents = running.filter((s) => s.tool === 'subagent').length;

  const copyLast = async () => {
    const text = lastAssistant >= 0 ? props.turns[lastAssistant].text : '';
    if (!text) return;
    try {
      await navigator.clipboard.writeText(text);
    } catch {
      const ta = document.createElement('textarea');
      ta.value = text;
      document.body.appendChild(ta);
      ta.select();
      document.execCommand('copy');
      ta.remove();
    }
    setCopied(true);
    setTimeout(() => setCopied(false), 1500);
  };

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <div className="min-h-0 flex-1 overflow-y-auto px-4 py-6" data-scrollable>
        <div className="mx-auto w-full max-w-[720px] space-y-4">
          {props.turns.map((t, i) => {
            if (t.role === 'user') {
              const run = [t.agent, shortModel(t.model), fmtTime(t.created_at)].filter(Boolean).join(' · ');
              return (
                <div key={i}>
                  {run && <div className="mb-1 text-[12px] text-[var(--v2-text-faint)]">{run}</div>}
                  <div
                    className="rounded-[8px] px-4 py-3 text-[13px] leading-5 text-[var(--v2-text-base)] select-text"
                    style={{ background: 'var(--user-bubble)', border: '1px solid var(--user-border)' }}
                  >
                    <LinkifiedText text={t.text} />
                  </div>
                  <button
                    className="mt-1 text-[12px] text-[var(--v2-text-faint)] hover:text-[var(--v2-text-muted)]"
                    onClick={() => props.onUndo(i)}
                  >
                    Undo
                  </button>
                </div>
              );
            }
            if (isToolTurn(t)) {
              // Group head renders the whole consecutive run; the rest
              // of the run returns null to avoid double-rendering.
              if (i > 0 && isToolTurn(props.turns[i - 1])) return null;
              const group: { idx: number; t: Turn }[] = [];
              for (let j = i; j < props.turns.length && isToolTurn(props.turns[j]); j++) {
                group.push({ idx: j, t: props.turns[j] });
              }
              return (
                <div key={i} className="space-y-2">
                  <ToolGroup tools={group} />
                </div>
              );
            }
            if (t.role !== 'assistant') return null;
            return (
              <div key={i} className="space-y-2">
                <div className="whitespace-pre-wrap text-[13px] leading-5 text-[var(--v2-text-base)] select-text">
                  <LinkifiedText text={t.text} />
                </div>
                {i === lastAssistant && !props.working && (
                  <div className="flex items-center gap-2 text-[12px] text-[var(--v2-text-faint)]">
                    {summaryText && <span>{summaryText}</span>}
                    {fmtTime(t.created_at) && <span>{fmtTime(t.created_at)}</span>}
                    <button className="v2-row rounded px-1.5 py-0.5 hover:text-[var(--v2-text-base)]" onClick={copyLast} title="Copy response">
                      {copied ? 'Copied ✓' : '⧉ Copy'}
                    </button>
                    <button
                      className="hover:text-[var(--v2-text-base)]"
                      onClick={() => props.onUndo(i)}
                    >
                      Undo
                    </button>
                  </div>
                )}
              </div>
            );
          })}
          {props.working && (
            <div
              className="rounded-[8px] bg-[var(--v2-bg-base)] px-4 py-3"
              style={{ border: '1px solid var(--v2-border-base)' }}
            >
              {/* v2 activity lane: fixed height, shimmer status, no layout shift. */}
              <div className="mb-1 flex h-9 items-center gap-2 text-[13px]">
                <span className="v2-shimmer font-medium" role="status">Working</span>
                {props.elapsedMs > 0 && (
                  <span className="text-[12px] tabular-nums text-[var(--v2-text-muted)]">{Math.floor(props.elapsedMs / 1000)}s</span>
                )}
                {props.stage && <span className="truncate text-[12px] text-[var(--v2-text-muted)]">· {props.stage}</span>}
                <span className="flex-1" />
                {formatCtx(props.ctx) && (
                  <span className="rounded border border-[var(--v2-border-base)] px-1.5 py-0.5 font-mono text-[11px] text-[var(--v2-text-muted)]">
                    {formatCtx(props.ctx)}
                  </span>
                )}
              </div>
              {props.liveSteps.length > 0 && (
                <div className="mb-2 space-y-1">
                  {props.liveSteps.map((s, i) => (
                    <ToolRow
                      key={i}
                      name={s.tool}
                      body={(s.target + (s.preview ? '\n' + s.preview : '')).trim()}
                      error={!!s.error}
                      live={s.phase === 'start'}
                      duration_ms={s.phase === 'done' ? s.duration_ms : undefined}
                    />
                  ))}
                </div>
              )}
              {props.liveThinking && (
                <details className="mb-2 text-[12px] text-[var(--v2-text-muted)]">
                  <summary className="cursor-pointer hover:text-[var(--v2-text-base)]">thinking</summary>
                  <div className="mt-1 whitespace-pre-wrap select-text">{props.liveThinking.slice(-2000)}</div>
                </details>
              )}
              {props.liveText && (
                <div className="whitespace-pre-wrap text-[13px] leading-5 text-[var(--v2-text-base)] select-text">
                  {props.liveText.slice(-4000)}
                  <span className="animate-pulse">▊</span>
                </div>
              )}
            </div>
          )}
          {!props.working && props.error && (
            <div
              role="alert"
              className="rounded-[8px] px-4 py-3 text-[13px] leading-5"
              style={{ border: '1px solid rgba(239,68,68,0.4)', background: 'rgba(239,68,68,0.06)' }}
            >
              <div className="mb-1 font-medium text-red-500">Turn failed</div>
              <div className="whitespace-pre-wrap text-[var(--v2-text-base)] select-text">{props.error.slice(0, 1000)}</div>
              <button className="v2-row mt-2 rounded-[6px] px-2 py-1 text-[12px]" onClick={props.onRetry}>
                ↻ Retry
              </button>
            </div>
          )}
          {!props.working && !props.error && props.changed && props.changed.files.length > 0 && (
            <div className="rounded-[8px] border border-[var(--v2-border-base)] px-4 py-3">
              <div className="flex h-6 min-w-0 items-center gap-2">
                <span className="text-[13px] font-medium tabular-nums text-[var(--v2-text-base)]">
                  {props.changed.files.length} file{props.changed.files.length === 1 ? '' : 's'} changed
                </span>
                {props.changed.stat && (
                  <span className="truncate font-mono text-[11px] text-[var(--v2-text-muted)]">{props.changed.stat.slice(0, 120)}</span>
                )}
                {props.changed.files.length > 5 && (
                  <button
                    type="button"
                    className="ml-auto shrink-0 text-[12px] text-[var(--v2-text-muted)] hover:text-[var(--v2-text-base)]"
                    onClick={() => setShowAllChanged(!showAllChanged)}
                  >
                    {showAllChanged ? 'Show less' : 'Show all'}
                  </button>
                )}
              </div>
              <div className="mt-1 flex flex-wrap gap-1.5">
                {(showAllChanged ? props.changed.files : props.changed.files.slice(0, 5)).map((f) => (
                  <span key={f} className="rounded-[6px] border border-[var(--v2-border-base)] px-2 py-0.5 font-mono text-[11px] text-[var(--v2-text-muted)]">
                    {f}
                  </span>
                ))}
              </div>
            </div>
          )}
        </div>
      </div>
      <div className="relative z-[70] mx-auto w-full max-w-[720px] px-4 pb-4">
        {/* v2 composer lane: activity + todo capsules sit beside the composer. */}
        {props.working && (runningShells + runningAgents > 0 || props.queue.length > 0) && (
          <div className="relative mb-1 flex min-w-0 items-center gap-2 px-1">
            {(runningShells + runningAgents > 0) && (
              <span
                className="flex min-w-0 items-center gap-1.5 rounded-full border border-[var(--v2-border-base)] px-2.5 py-1 text-[12px] tabular-nums"
                style={{ background: 'var(--v2-bg-base)' }}
                title={running.map((s) => `${s.tool} ${s.target}`).join('\n').slice(0, 300)}
              >
                <span className="inline-block h-1.5 w-1.5 animate-pulse rounded-full bg-amber-400" />
                <span className="truncate text-[var(--v2-text-muted)]">
                  {[runningShells ? `${runningShells} shell${runningShells === 1 ? '' : 's'}` : '', runningAgents ? `${runningAgents} subagent${runningAgents === 1 ? '' : 's'}` : ''].filter(Boolean).join(' · ')}
                  {props.elapsedMs > 0 ? ` · ${Math.floor(props.elapsedMs / 1000)}s` : ''}
                </span>
              </span>
            )}
            {props.queue.length > 0 && (
              <span className="rounded-full border border-[var(--v2-border-base)] px-2.5 py-1 text-[12px] tabular-nums text-[var(--v2-text-muted)]" style={{ background: 'var(--v2-bg-base)' }}>
                {props.queue.length} queued
              </span>
            )}
            <span className="min-w-0 flex-1" />
            <TodoPill turns={props.turns} />
          </div>
        )}
        {/* v2 queue panel: attached surface above the composer listing prompts
            queued behind the active turn. Display-only: the daemon has no
            queue reorder/cancel endpoint, so rows offer no actions. */}
        {props.queue.length > 0 && (
          <div
            className="relative z-0 -mb-3 rounded-t-[12px] bg-[var(--v2-bg-base)] px-1.5 pt-1.5 shadow-[var(--v2-elevation-raised)]"
            style={{ border: '1px solid var(--v2-border-base)', borderBottom: 'none', paddingBottom: props.queue.length > 4 ? 12 : 18 }}
          >
            {props.queue.length > 3 && (
              <div className="px-1.5 pb-px pt-1 text-[11px] font-medium uppercase tracking-[0.05px] text-[var(--v2-text-muted)] tabular-nums">
                {props.queue.length} queued
              </div>
            )}
            <div className={`flex flex-col ${props.queue.length > 4 ? 'max-h-[148px] overflow-y-auto' : ''}`}>
              {props.queue.map((q, i) => (
                <div key={i} className="mb-px flex h-8 shrink-0 items-center justify-between gap-2 rounded-md py-1 pe-1 ps-0.5">
                  <div className="flex min-w-0 flex-1 items-center gap-2">
                    <span className="grid shrink-0 grid-cols-2 gap-x-[2px] gap-y-[2.25px] p-1" aria-hidden="true">
                      {Array.from({ length: 6 }).map((_, k) => (
                        <span key={k} className="size-[2px] bg-[var(--v2-text-faint)]" />
                      ))}
                    </span>
                    <span dir="auto" className="min-w-0 flex-1 truncate text-start text-[13px] text-[var(--v2-text-base)]" title={q.text}>
                      {q.text || `(${q.files.length} attachment${q.files.length === 1 ? '' : 's'})`}
                    </span>
                    {q.files.length > 0 && q.text && (
                      <span className="shrink-0 whitespace-nowrap text-[12px] text-[var(--v2-text-muted)]">
                        {q.files.length} file{q.files.length === 1 ? '' : 's'}
                      </span>
                    )}
                  </div>
                </div>
              ))}
            </div>
          </div>
        )}
        <div className="relative">
          {!props.working && (
            <div className="relative mb-1 flex min-w-0 items-center gap-2 px-1 empty:hidden">
              <span className="min-w-0 flex-1" />
              <TodoPill turns={props.turns} />
            </div>
          )}
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
    </div>
  );
}
