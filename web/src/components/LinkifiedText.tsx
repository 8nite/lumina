import React from 'react';

// Renders plain text with clickable links. Every URL becomes an anchor that
// opens in a new tab (target _blank + noopener).
const urlRe = /(https?:\/\/[^\s<>"')\]]+|www\.[^\s<>"')\]]+)/g;

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

export function LinkifiedText(props: { text: string }) {
  const parts: React.ReactNode[] = [];
  let last = 0;
  let key = 0;
  const matches = props.text.matchAll(urlRe);
  for (const m of matches) {
    const idx = m.index ?? 0;
    if (idx > last) parts.push(props.text.slice(last, idx));
    const [url, trail] = splitTrailing(m[0]);
    const href = url.startsWith('http') ? url : 'https://' + url;
    parts.push(
      <a
        key={key++}
        href={href}
        target="_blank"
        rel="noopener noreferrer"
        className="text-sky-300 underline decoration-sky-300/40 underline-offset-2 hover:text-sky-200"
      >
        {url}
      </a>,
    );
    if (trail) parts.push(trail);
    last = idx + m[0].length;
  }
  if (last < props.text.length) parts.push(props.text.slice(last));
  if (parts.length === 0) return <>{props.text}</>;
  return <>{parts}</>;
}
