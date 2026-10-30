import { useState } from 'react';

// Copilot web auth: device flow handled by the daemon. The daemon shows the
// user code + verification link; the user approves in their browser; the
// daemon polls, stores the token (0600, never returned), and emits
// auth.connected. No secret ever touches this page.
async function post(path: string, body: unknown) {
  const res = await fetch(path, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(body),
  });
  if (!res.ok) throw new Error(`${path}: ${res.status}`);
  return res.json();
}

export function CopilotConnect(props: { provider?: string }) {
  const provider = props.provider ?? 'copilot';
  const [host, setHost] = useState('https://github.com');
  const [code, setCode] = useState('');
  const [uri, setUri] = useState('');
  const [device, setDevice] = useState('');
  const [status, setStatus] = useState('(not connected)');

  const start = async () => {
    setStatus('starting…');
    const d = await post('/api/v1/auth/device/start', { provider, github_url: host });
    setCode(d.user_code);
    setUri(d.verification_uri);
    setDevice(d.device_code);
    setStatus('waiting for approval…');
  };

  const poll = async () => {
    const r = await post('/api/v1/auth/device/poll', { provider, device_code: device, github_url: host });
    if (r.status === 'authorized') {
      setStatus('connected ✓ (token stored daemon-side)');
      setCode('');
    } else {
      setStatus(`status: ${r.status}${r.error ? ' — ' + r.error : ''}`);
    }
  };

  return (
    <div className="rounded-lg border border-white/10 bg-white/[0.02] p-2 text-xs text-zinc-300">
      <div className="mb-1 font-semibold">Copilot web auth ({provider})</div>
      <div className="mb-1 text-gray-500">{status}</div>
      <input
        className="mb-1 w-full rounded border px-1"
        value={host}
        onChange={(e) => setHost(e.target.value)}
        placeholder="https://github.com (or GHES host)"
      />
      <button className="rounded bg-indigo-600 px-2 py-0.5 text-white" onClick={start}>
        Connect
      </button>
      {code && (
        <div className="mt-1">
          <div>
            Open <a className="underline" href={uri} target="_blank" rel="noreferrer">{uri}</a> and enter:
          </div>
          <div className="font-mono text-lg">{code}</div>
          <button className="mt-1 rounded bg-gray-100 px-2" onClick={poll}>
            I approved — check
          </button>
        </div>
      )}
    </div>
  );
}
