import { useState } from 'react';
import type { Session } from '../api/client';

function fmtk(n: number): string {
  if (!n || n <= 0) return '0';
  if (n < 1000) return `${n}`;
  return `${(n / 1000).toFixed(1)}k`;
}

// v2-style session identity header: location crumb + title + context gauge.
export function TopBar(props: {
  session: Session | null;
  onToggleSidebar: () => void;
  onNew: () => void;
  onFork: () => void;
  onExport: () => void;
  onToggleReview: () => void;
  reviewOpen: boolean;
  ctx?: { used: number; max: number } | null;
}) {
  const ctxText =
    props.ctx && props.ctx.used
      ? props.ctx.max > 0
        ? `ctx ~${fmtk(props.ctx.used)}/${fmtk(props.ctx.max)}`
        : `ctx ~${fmtk(props.ctx.used)}`
      : '';
  const [menu, setMenu] = useState(false);
  const ws = props.session?.workspace_dir || '';
  const leaf = ws.split('/').filter(Boolean).pop() || ws || 'session';
  return (
    <div className="flex h-12 shrink-0 items-center gap-1 border-b border-[var(--v2-border-base)] px-2 text-[13px]">
      <button
        className="v2-row px-2 py-1 text-[var(--v2-text-muted)]"
        title="Toggle sidebar (mod+shift+b)"
        onClick={props.onToggleSidebar}
      >
        ☰
      </button>
      <div className="relative">
        <button
          onClick={() => setMenu(!menu)}
          className="v2-row flex max-w-72 items-center gap-1.5 px-2 py-1 text-[var(--v2-text-base)]"
        >
          <span className="max-w-56 truncate font-medium">
            {props.session ? props.session.title || props.session.id.slice(0, 8) : 'New session'}
          </span>
          {!props.session && (
            <span className="truncate text-[var(--v2-text-faint)]">· {leaf}</span>
          )}
          <span className="text-[11px] text-[var(--v2-text-faint)]">▾</span>
        </button>
        {menu && (
          <div
            className="absolute left-0 top-9 z-20 w-72 rounded-[12px] bg-[var(--v2-bg-base)] p-1 shadow-[var(--v2-elevation-floating)]"
            style={{ border: '1px solid var(--v2-border-base)' }}
          >
            <button
              className="v2-row block w-full rounded-[6px] px-2 py-1.5 text-left"
              onClick={() => { setMenu(false); props.onNew(); }}
            >
              + New session
            </button>
            {props.session && (
              <>
                <button
                  className="v2-row block w-full rounded-[6px] px-2 py-1.5 text-left"
                  onClick={() => { setMenu(false); props.onFork(); }}
                >
                  Fork session
                </button>
                <button
                  className="v2-row block w-full rounded-[6px] px-2 py-1.5 text-left"
                  onClick={() => { setMenu(false); props.onExport(); }}
                >
                  Export transcript
                </button>
              </>
            )}
          </div>
        )}
      </div>
      {props.session && (
        <span className="hidden truncate text-[12px] text-[var(--v2-text-faint)] md:inline" title={ws}>
          {leaf} · {props.session.agent}
        </span>
      )}
      <div className="flex-1" />
      {ctxText && (
        <span
          className="mr-1 rounded border border-[var(--v2-border-base)] px-1.5 py-0.5 font-mono text-[11px] text-[var(--v2-text-muted)]"
          title="estimated context usage"
        >
          {ctxText}
        </span>
      )}
      <button
        className="v2-row px-2 py-1 text-[var(--v2-text-muted)]"
        onClick={props.onToggleReview}
        title="Toggle side panel"
      >
        {props.reviewOpen ? '◫' : '◧'}
      </button>
      {props.session && (
        <button
          className="v2-row px-2 py-1 text-[var(--v2-text-muted)]"
          onClick={props.onExport}
          title="Share / export"
        >
          ⤴
        </button>
      )}
    </div>
  );
}
