import React from 'react';

// Minimal chat-markdown renderer (no new deps): fenced code blocks,
// # headers, -/*/1. lists, **bold**, *italic*, `code`, [t](u) links,
// plus the original bare-URL linkify. Nodes are React elements, so text
// is never injected as HTML. Keeps the LinkifiedText name/props so all
// call sites keep working.

export interface FileTarget {
  path: string;
  line?: number;
}

const urlRe = /(https?:\/\/[^\s<>"')\]]+|www\.[^\s<>"')\]]+)/;

const linkCls =
  'text-sky-300 underline decoration-sky-300/40 underline-offset-2 hover:text-sky-200';

// Matches repo-relative file mentions like `src/foo.ts`, `web/a/b.tsx:12`,
// ./relative/paths, and /absolute/paths — with an optional :line suffix.
// ponytail: extension allowlist keeps prose ("word: thing") unlinked.
const fileRe =
  /(^|[\s(\[{'"`>])(~\/[^\s<>"')\]]+|\.{0,2}\/[^\s<>"')\]]+|[a-zA-Z0-9_@~.-]+\/[a-zA-Z0-9_@~.-][^\s<>"')\]]*?\.[a-zA-Z0-9]{1,8})(:\d{1,6})?(?=[\s.,;:!?)}\]<'"`]|$)/;

// Trailing punctuation is sentence punctuation, not part of the URL.
function splitTrailing(raw: string): [string, string] {
  let url = raw;
  let trail = '';
  while (url.length > 0 && '.,;:!?)]}'.includes(url[url.length - 1])) {
    trail = url[url.length - 1] + trail;
    url = url.slice(0, -1);
  }
  return [url, trail];
}

function Link(props: { href: string; label: React.ReactNode }) {
  return (
    <a href={props.href} target="_blank" rel="noopener noreferrer" className={linkCls}>
      {props.label}
    </a>
  );
}

function FileLink(props: { target: FileTarget; onFile?: (t: FileTarget) => void }) {
  const label = props.target.line ? `${props.target.path}:${props.target.line}` : props.target.path;
  if (!props.onFile) {
    return <code className="rounded bg-black/20 px-1 font-mono text-[12px]">{label}</code>;
  }
  return (
    <button
      onClick={() => props.onFile!(props.target)}
      title={props.target.line ? `Open ${props.target.path} at line ${props.target.line}` : `Open ${props.target.path}`}
      className="rounded bg-black/20 px-1 font-mono text-[12px] text-sky-300 underline decoration-sky-300/40 underline-offset-2 hover:text-sky-200"
    >
      {label}
    </button>
  );
}

// ponytail: single-pass inline parser; _italic_ deliberately unsupported
// (snake_case would false-positive). Ceiling: nested same-marker spans.
function renderInline(
  text: string,
  kid: string,
  onFile?: (t: FileTarget) => void,
): React.ReactNode[] {
  const out: React.ReactNode[] = [];
  let i = 0;
  let k = 0;
  const key = () => `${kid}-${k++}`;
  while (i < text.length) {
    const rest = text.slice(i);
    // `code` — render inner file paths as links when an opener exists.
    if (rest[0] === '`') {
      const end = rest.indexOf('`', 1);
      if (end > 1) {
        const inner = rest.slice(1, end);
        const fm = fileRe.exec(inner);
        if (onFile && fm && fm[0].trim().length > 0 && fm.index + fm[0].length >= inner.trim().length) {
          const line = fm[3] ? parseInt(fm[3].slice(1), 10) : undefined;
          out.push(<FileLink key={key()} target={{ path: fm[2], line }} onFile={onFile} />);
        } else {
          out.push(
            <code key={key()} className="rounded bg-black/20 px-1 font-mono text-[12px]">
              {inner}
            </code>,
          );
        }
        i += end + 1;
        continue;
      }
    }
    // **bold** (before *italic*)
    if (rest.startsWith('**') && rest.length > 4) {
      const end = rest.indexOf('**', 2);
      if (end > 2) {
        out.push(<strong key={key()}>{renderInline(rest.slice(2, end), `${kid}b`, onFile)}</strong>);
        i += end + 2;
        continue;
      }
    }
    // *italic*
    if (rest[0] === '*' && rest[1] !== '*' && rest.length > 2) {
      const end = rest.indexOf('*', 1);
      if (end > 1) {
        out.push(<em key={key()}>{renderInline(rest.slice(1, end), `${kid}i`, onFile)}</em>);
        i += end + 1;
        continue;
      }
    }
    // [label](href)
    if (rest[0] === '[') {
      const m = /^\[([^\]]+)\]\((https?:\/\/[^\s)]+|www\.[^\s)]+)\)/.exec(rest);
      if (m) {
        const href = m[2].startsWith('http') ? m[2] : 'https://' + m[2];
        out.push(<Link key={key()} href={href} label={m[1]} />);
        i += m[0].length;
        continue;
      }
    }
    // bare URL — always opens in a new tab.
    const um = urlRe.exec(rest);
    if (um && um.index === 0) {
      const [url, trail] = splitTrailing(um[0]);
      const href = url.startsWith('http') ? url : 'https://' + url;
      out.push(<Link key={key()} href={href} label={url} />);
      if (trail) out.push(trail);
      i += um[0].length;
      continue;
    }
    // file path mention → diff-view link (opencode v2: click opens file).
    const fmatch = fileRe.exec(rest);
    if (fmatch && fmatch.index === 0) {
      const prefix = fmatch[1];
      const path = fmatch[2];
      const line = fmatch[3] ? parseInt(fmatch[3].slice(1), 10) : undefined;
      if (prefix) out.push(prefix);
      out.push(<FileLink key={key()} target={{ path, line }} onFile={onFile} />);
      i += fmatch[0].length;
      continue;
    }
    // plain run until next special char
    let j = i;
    while (j < text.length && !'`*['.includes(text[j])) {
      if (text.startsWith('http', j) || text.startsWith('www.', j)) break;
      const ch = text[j];
      if (ch === '/' || ch === '.' || ch === '~') break;
      j++;
    }
    if (j === i) j = i + 1; // lone special char or http mid-word: literal
    out.push(text.slice(i, j));
    i = j;
  }
  return out;
}

const headerCls = ['text-[16px] font-semibold', 'text-[15px] font-semibold', 'text-[14px] font-semibold', 'text-[13px] font-semibold'];

function renderBlocks(text: string, kid: string, onFile?: (t: FileTarget) => void): React.ReactNode[] {
  // Fenced code blocks stay plain code (no link carving inside diffs).
  const fence = text.split(/```/);
  if (fence.length > 1) {
    return fence.map((seg, n) => {
      if (n % 2 === 1) {
        const nl = seg.indexOf('\n');
        const body = nl >= 0 ? seg.slice(nl + 1) : seg;
        return (
          <pre
            key={`${kid}-f${n}`}
            className="my-1 overflow-x-auto rounded-[6px] border border-[var(--v2-border-base)] bg-black/20 px-2 py-1.5 font-mono text-[12px] leading-relaxed"
          >
            {body.replace(/\n$/, '')}
          </pre>
        );
      }
      return <React.Fragment key={`${kid}-p${n}`}>{renderProse(seg, `${kid}-p${n}`, onFile)}</React.Fragment>;
    });
  }
  return renderProse(text, kid, onFile);
}

function renderProse(text: string, kid: string, onFile?: (t: FileTarget) => void): React.ReactNode[] {
  const lines = text.split('\n');
  const out: React.ReactNode[] = [];
  let para: string[] = [];
  let k = 0;
  const flush = () => {
    if (para.length > 0) {
      out.push(
        <div key={`${kid}-d${k++}`} className="whitespace-pre-wrap">
          {renderInline(para.join('\n'), `${kid}-d${k}`, onFile)}
        </div>,
      );
      para = [];
    }
  };
  for (const line of lines) {
    const hm = /^(#{1,4})\s+(.*)$/.exec(line);
    if (hm) {
      flush();
      out.push(
        <div key={`${kid}-d${k++}`} className={headerCls[hm[1].length - 1]}>
          {renderInline(hm[2], `${kid}-d${k}`, onFile)}
        </div>,
      );
      continue;
    }
    const lm = /^\s*([-*]|\d+[.)])\s+(.*)$/.exec(line);
    if (lm) {
      flush();
      const bullet = /^\d/.test(lm[1]) ? `${lm[1]} ` : '• ';
      out.push(
        <div key={`${kid}-d${k++}`} className="flex gap-2">
          <span className="shrink-0 text-[var(--v2-text-muted)]">{bullet}</span>
          <span className="min-w-0">{renderInline(lm[2], `${kid}-d${k}`, onFile)}</span>
        </div>,
      );
      continue;
    }
    if (line.trim() === '') {
      flush();
      continue;
    }
    para.push(line);
  }
  flush();
  return out;
}

export function LinkifiedText(props: { text: string; onFile?: (t: FileTarget) => void }) {
  if (!props.text) return null;
  return <>{renderBlocks(props.text, 'md', props.onFile)}</>;
}
