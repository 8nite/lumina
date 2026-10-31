import React from 'react';

// Minimal chat-markdown renderer (no new deps): fenced code blocks,
// # headers, -/*/1. lists, **bold**, *italic*, `code`, [t](u) links,
// plus the original bare-URL linkify. Nodes are React elements, so text
// is never injected as HTML. Keeps the LinkifiedText name/props so all
// call sites keep working.
const urlRe = /(https?:\/\/[^\s<>"')\]]+|www\.[^\s<>"')\]]+)/;

const linkCls =
  'text-sky-300 underline decoration-sky-300/40 underline-offset-2 hover:text-sky-200';

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

// ponytail: single-pass inline parser; _italic_ deliberately unsupported
// (snake_case would false-positive). Ceiling: nested same-marker spans.
function renderInline(text: string, kid: string): React.ReactNode[] {
  const out: React.ReactNode[] = [];
  let i = 0;
  let k = 0;
  const key = () => `${kid}-${k++}`;
  while (i < text.length) {
    const rest = text.slice(i);
    // `code`
    if (rest[0] === '`') {
      const end = rest.indexOf('`', 1);
      if (end > 1) {
        out.push(
          <code key={key()} className="rounded bg-black/20 px-1 font-mono text-[12px]">
            {rest.slice(1, end)}
          </code>,
        );
        i += end + 1;
        continue;
      }
    }
    // **bold** (before *italic*)
    if (rest.startsWith('**') && rest.length > 4) {
      const end = rest.indexOf('**', 2);
      if (end > 2) {
        out.push(<strong key={key()}>{renderInline(rest.slice(2, end), `${kid}b`)}</strong>);
        i += end + 2;
        continue;
      }
    }
    // *italic*
    if (rest[0] === '*' && rest[1] !== '*' && rest.length > 2) {
      const end = rest.indexOf('*', 1);
      if (end > 1) {
        out.push(<em key={key()}>{renderInline(rest.slice(1, end), `${kid}i`)}</em>);
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
    // bare URL
    const um = urlRe.exec(rest);
    if (um && um.index === 0) {
      const [url, trail] = splitTrailing(um[0]);
      const href = url.startsWith('http') ? url : 'https://' + url;
      out.push(<Link key={key()} href={href} label={url} />);
      if (trail) out.push(trail);
      i += um[0].length;
      continue;
    }
    // plain run until next special char
    let j = i;
    while (j < text.length && !'`*['.includes(text[j])) {
      if (text.startsWith('http', j) || text.startsWith('www.', j)) break;
      j++;
    }
    if (j === i) j = i + 1; // lone special char or http mid-word: literal
    out.push(text.slice(i, j));
    i = j;
  }
  return out;
}

const headerCls = ['text-[16px] font-semibold', 'text-[15px] font-semibold', 'text-[14px] font-semibold', 'text-[13px] font-semibold'];

function renderBlocks(text: string, kid: string): React.ReactNode[] {
  // Fenced code blocks first; odd segments are code.
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
      return <React.Fragment key={`${kid}-p${n}`}>{renderProse(seg, `${kid}-p${n}`)}</React.Fragment>;
    });
  }
  return renderProse(text, kid);
}

function renderProse(text: string, kid: string): React.ReactNode[] {
  const lines = text.split('\n');
  const out: React.ReactNode[] = [];
  let para: string[] = [];
  let k = 0;
  const flush = () => {
    if (para.length > 0) {
      out.push(
        <div key={`${kid}-d${k++}`} className="whitespace-pre-wrap">
          {renderInline(para.join('\n'), `${kid}-d${k}`)}
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
          {renderInline(hm[2], `${kid}-d${k}`)}
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
          <span className="min-w-0">{renderInline(lm[2], `${kid}-d${k}`)}</span>
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

export function LinkifiedText(props: { text: string }) {
  if (!props.text) return null;
  return <>{renderBlocks(props.text, 'md')}</>;
}
