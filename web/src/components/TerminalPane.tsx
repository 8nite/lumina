import { useEffect, useRef } from 'react';
import { Terminal } from '@xterm/xterm';
import '@xterm/xterm/css/xterm.css';
import { getApiBase } from '../api/client';

// PTY drawer backed by the daemon WebSocket (RFC6455 text frames).
export function TerminalPane() {
  const ref = useRef<HTMLDivElement>(null);
  useEffect(() => {
    const term = new Terminal({ convertEol: true, theme: { background: '#0a0a0b' } });
    term.open(ref.current!);
    term.writeln('lumina pty — type a command, enter to run');
    const base = getApiBase();
    const host = base ? base.replace(/^https?:\/\//, '') : location.host;
    const proto = (base.startsWith('https:') || (!base && location.protocol === 'https:')) ? 'wss:' : 'ws:';
    const ws = new WebSocket(`${proto}//${host}/api/v1/pty`);
    ws.onmessage = (ev) => term.write(String(ev.data));
    let line = '';
    term.onData((d) => {
      if (d === '\r') {
        ws.send(line);
        term.write('\r\n');
        line = '';
      } else if (d === '\u007f') {
        line = line.slice(0, -1);
        term.write('\b \b');
      } else {
        line += d;
        term.write(d);
      }
    });
    return () => {
      ws.close();
      term.dispose();
    };
  }, []);
  return <div ref={ref} className="h-64 bg-black" />;
}
