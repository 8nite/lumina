import { useCallback, useEffect, useRef, useState } from 'react';
import {
  api,
  subscribe,
  setApiBase,
  getApiBase,
  fileToAttachment,
  type Session,
  type DaemonEvent,
  type PromptAttachment,
} from './api/client';
import { TopBar } from './components/TopBar';
import { HomeView } from './components/HomeView';
import { SessionView, type Turn, type LiveStep, type TurnSummary } from './components/SessionView';
import { ReviewPane } from './components/ReviewPane';
import { ServersModal } from './components/ServersModal';
import { TerminalPane } from './components/TerminalPane';
import { GraphifyPane } from './components/GraphifyPane';
import { HarnessPane } from './components/HarnessPane';
import { CopilotConnect } from './components/CopilotConnect';

const perms = ['ask', 'auto-edit', 'auto-approve', 'bypass'];

export default function App() {
  const [base, setBase] = useState(getApiBase());
  const [sessions, setSessions] = useState<Session[]>([]);
  const [session, setSession] = useState<Session | null>(null);
  const [turns, setTurns] = useState<Turn[]>([]);
  const [input, setInput] = useState('');
  const [workspace, setWorkspace] = useState('.');
  const [branch, setBranch] = useState({ branch: '', modified: '' });
  const [reviewOpen, setReviewOpen] = useState(true);
  const [rightTab, setRightTab] = useState('review');
  const [serversOpen, setServersOpen] = useState(false);
  const [modelOptions, setModelOptions] = useState<string[]>([]);
  const [homeAgent, setHomeAgent] = useState('build');
  const [homePerm, setHomePerm] = useState('ask');
  const [homeModel, setHomeModel] = useState('');
  const [working, setWorking] = useState(false);
  const [liveText, setLiveText] = useState('');
  const [liveThinking, setLiveThinking] = useState('');
  const [liveSteps, setLiveSteps] = useState<LiveStep[]>([]);
  const [stage, setStage] = useState('');
  const [summary, setSummary] = useState<TurnSummary | null>(null);
  const [attachments, setAttachments] = useState<PromptAttachment[]>([]);
  const [effort, setEffort] = useState('');

  const refresh = useCallback(() => {
    api.sessions().then(setSessions).catch(() => {});
  }, []);

  const refreshModels = useCallback(() => {
    api
      .routerState()
      .then((r) => {
        const vals = Object.values(r.router || {}).filter(Boolean);
        setModelOptions((prev) => Array.from(new Set([...vals, ...prev])));
        if (!homeModelRef.current && vals.length) setHomeModel(vals[0]);
      })
      .catch(() => {});
    api
      .providers()
      .then((p) => {
        // Live models for every connected provider (never one hardcoded
        // name); failures are skipped so one down provider hides nothing.
        for (const pv of p.providers || []) {
          const name = typeof pv === 'string' ? pv : pv.name;
          api
            .models(name)
            .then((m) =>
              setModelOptions((prev) =>
                Array.from(
                  new Set([...prev, ...(m.models || []).map((x) => (x.id.includes('/') ? x.id : `${name}/${x.id}`))]),
                ),
              ),
            )
            .catch(() => {});
        }
      })
      .catch(() => {});
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const homeModelRef = useRef(homeModel);
  homeModelRef.current = homeModel;

  useEffect(() => {
    refresh();
    refreshModels();
    return subscribe((e: DaemonEvent) => {
      const forSession = !e.session_id || !session?.id || e.session_id === session.id;
      if (e.type === 'turn.started' && forSession) {
        setWorking(true);
        setLiveText('');
        setLiveThinking('');
        setLiveSteps([]);
        setStage('');
        setSummary(null);
      }
      if ((e.type === 'turn.delta') && forSession) {
        const d = (e.data || {}) as { delta?: string; thinking?: string };
        if (d.delta) setLiveText((t) => (t + d.delta!).slice(-8000));
        if (d.thinking) setLiveThinking((t) => (t + d.thinking!).slice(-4000));
      }
      if (e.type === 'turn.progress' && forSession) {
        const d = (e.data || {}) as { stage?: string };
        if (d.stage) setStage(d.stage);
      }
      if (e.type === 'turn.tool' && forSession) {
        const d = (e.data || {}) as Partial<LiveStep> & { phase?: 'start' | 'done' };
        if (d.tool) {
          setLiveSteps((steps) => {
            const next = [...steps];
            if (d.phase === 'done') {
              const i = [...next].reverse().findIndex((s) => s.tool === d.tool && s.phase === 'start');
              const step: LiveStep = {
                tool: d.tool!, target: d.target || '',
                phase: 'done', preview: d.preview, error: d.error, duration_ms: d.duration_ms,
              };
              if (i >= 0) next[next.length - 1 - i] = step;
              else next.push(step);
            } else {
              next.push({ tool: d.tool!, target: d.target || '', phase: 'start' });
            }
            return next.slice(-30);
          });
        }
      }
      if (e.type === 'turn.completed' && e.session_id && e.session_id === session?.id) {
        const sid = e.session_id;
        const d = (e.data || {}) as { text?: string; model?: string; duration_ms?: number; output_tokens?: number };
        setSummary({ model: d.model, duration_ms: d.duration_ms, output_tokens: d.output_tokens });
        api.export(sid).then((ex) => {
          setTurns(
            (ex.transcript || [])
              .filter((t) => t.role === 'user' || t.role === 'assistant' || t.role === 'tool' || t.role === 'tool_error')
              .map((t) => ({ role: t.role, text: t.text })),
          );
        }).catch(() => {
          const d = e.data as { text?: string };
          setTurns((t) => [...t, { role: 'assistant', text: d?.text ?? '' }]);
        });
        setLiveText('');
        setLiveThinking('');
        setLiveSteps([]);
        setStage('');
        setWorking(false);
      }
      if (e.type === 'turn.failed' && forSession) {
        setLiveText('');
        setLiveThinking('');
        setLiveSteps([]);
        setStage('');
        setSummary(null);
        setWorking(false);
      }
      if (e.type === 'session.created' || e.type === 'session.updated' || e.type === 'auth.connected') refresh();
    });
  }, [refresh, refreshModels, session?.id, base]);

  // Branch info follows the workspace.
  useEffect(() => {
    const t = setTimeout(() => {
      api
        .branch(workspace)
        .then((b) => setBranch({ branch: b.branch || '', modified: b.modified || '' }))
        .catch(() => setBranch({ branch: '', modified: '' }));
    }, 300);
    return () => clearTimeout(t);
  }, [workspace, base]);

  // Tab = agent toggle, Shift+Tab = approval cycle.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      const tag = (e.target as HTMLElement)?.tagName;
      if (tag === 'INPUT' || tag === 'TEXTAREA') return;
      if (e.key === 'Tab') {
        e.preventDefault();
        if (e.shiftKey) void cyclePerm();
        else void flipAgent();
      }
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  });

  const agent = session?.agent ?? homeAgent;
  const perm = session?.permission_mode ?? homePerm;
  const model = session?.model || homeModel || 'model';

  const flipAgent = async () => {
    const next = agent === 'plan' ? 'build' : 'plan';
    if (session) {
      const s = await api.patchSession(session.id, { agent: next });
      setSession(s);
    } else {
      setHomeAgent(next);
    }
  };

  const cyclePerm = async () => {
    const next = perms[(perms.indexOf(perm) + 1) % perms.length];
    if (session) {
      const s = await api.patchSession(session.id, { permission_mode: next });
      setSession(s);
    } else {
      setHomePerm(next);
    }
  };

  const applyModel = async (id: string) => {
    if (session) await api.patchSession(session.id, { model: id });
    else setHomeModel(id);
    await api.setRouter('executor', id).catch(() => {});
    setModelOptions((prev) => (prev.includes(id) ? prev : [...prev, id]));
    refreshModels();
  };

  const onAttach = async (files: FileList | null) => {
    if (!files) return;
    for (const f of Array.from(files)) {
      if (!f.type.startsWith('image/')) continue;
      if (f.size > 8 * 1024 * 1024) {
        alert(`${f.name} exceeds the 8MB attach cap`);
        continue;
      }
      try {
        const a = await fileToAttachment(f);
        setAttachments((prev) => [...prev, a]);
      } catch (e) {
        alert(String(e));
      }
    }
  };

  const pickSession = async (s: Session | null) => {
    setSession(s);
    setTurns([]);
    setInput('');
    if (s) {
      setWorkspace(s.workspace_dir);
      try {
        const ex = await api.export(s.id);
        setTurns(
          (ex.transcript || [])
            .filter((t) => t.role === 'user' || t.role === 'assistant' || t.role === 'tool' || t.role === 'tool_error')
            .map((t) => ({ role: t.role, text: t.text })),
        );
      } catch {
        /* offline transcript */
      }
    }
  };

  const send = async (delivery: 'steer' | 'queue' = 'steer') => {
    const text = input.trim();
    if (!text && attachments.length === 0) return;
    setInput('');
    const atts = attachments;
    setAttachments([]);
    let s = session;
    if (!s) {
      s = await api.createSession(workspace || '.', homeAgent, homePerm);
      if (homeModel) await api.patchSession(s.id, { model: homeModel });
      setSession(s);
      refresh();
    }
    const shown = text + atts.map((a) => `\n[file: ${a.name}]`).join('');
    setTurns((t) => [...t, { role: 'user', text: shown }]);
    setWorking(true);
    try {
      const r = await api.prompt(s.id, text, atts, effort || undefined, delivery);
      if (r.status === 'queued') setStage(`queued #${r.position ?? ''}`.trim());
    } catch (err) {
      setWorking(false);
      alert(String(err));
    }
  };

  const host = (() => {
    try {
      return base ? new URL(base).host : location.host;
    } catch {
      return base || location.host;
    }
  })();

  const switchServer = (url: string) => {
    setApiBase(url);
    setBase(url);
    setSession(null);
    setTurns([]);
  };

  return (
    <div className="flex h-screen flex-col bg-[#0a0a0b] text-sm text-zinc-200">
      <TopBar
        session={session}
        sessions={sessions}
        onSelect={(s) => void pickSession(s)}
        onNew={() => void pickSession(null)}
        onFork={() => session && api.fork(session.id).then((f) => { pickSession(f); refresh(); })}
        onExport={() => {
          if (!session) return;
          api.export(session.id).then((d) => {
            console.log('transcript', d.transcript);
            alert(`${d.transcript.length} turns exported (see console)`);
          });
        }}
        onShowServers={() => setServersOpen(true)}
        onToggleReview={() => setReviewOpen(!reviewOpen)}
        reviewOpen={reviewOpen}
        host={host}
      />
      <div className="flex min-h-0 flex-1">
        <main className="flex min-w-0 flex-1 flex-col">
          {!session ? (
            <HomeView
              workspace={workspace}
              onWorkspace={setWorkspace}
              branch={branch.branch}
              modified={branch.modified}
              input={input}
              onInput={setInput}
              onSend={(d) => void send(d)}
              agent={agent}
              onAgent={setHomeAgent}
              model={model}
              modelOptions={modelOptions}
              onModel={(m) => void applyModel(m)}
              onCustomModel={(m) => void applyModel(m)}
              perm={perm}
              onPerm={setHomePerm}
              effort={effort}
              onEffort={setEffort}
              attachments={attachments}
              onAttach={(f) => void onAttach(f)}
              onRemoveAttachment={(i) => setAttachments((a) => a.filter((_, j) => j !== i))}
              working={working}
            />
          ) : (
            <SessionView
              turns={turns}
              onUndo={(i) => session && api.revert(session.id).then(() => setTurns((t) => t.slice(0, i)))}
              input={input}
              onInput={setInput}
              onSend={(d) => void send(d)}
              agent={agent}
              onAgent={(a) => api.patchSession(session.id, { agent: a }).then(setSession)}
              model={model}
              modelOptions={modelOptions}
              onModel={(m) => void applyModel(m)}
              onCustomModel={(m) => void applyModel(m)}
              perm={perm}
              onPerm={(p) => api.patchSession(session.id, { permission_mode: p }).then(setSession)}
              effort={effort}
              onEffort={setEffort}
              attachments={attachments}
              onAttach={(f) => void onAttach(f)}
              onRemoveAttachment={(i) => setAttachments((a) => a.filter((_, j) => j !== i))}
              working={working}
              liveText={liveText}
              liveThinking={liveThinking}
              liveSteps={liveSteps}
              stage={stage}
              summary={summary}
            />
          )}
        </main>
        {reviewOpen && session && (
          <aside className="hidden w-[420px] shrink-0 md:block">
            <ReviewPane
              tab={rightTab}
              onTab={setRightTab}
              workspace={session.workspace_dir}
              term={<TerminalPane />}
              graph={<GraphifyPane />}
              memory={
                <div className="p-2 text-xs">
                  <GraphifyPane />
                  <div className="mt-2">
                    <CopilotConnect />
                  </div>
                </div>
              }
              harness={<HarnessPane />}
              connect={
                <div className="p-2">
                  <CopilotConnect />
                </div>
              }
            />
          </aside>
        )}
      </div>
      {serversOpen && (
        <ServersModal active={base} onPick={switchServer} onClose={() => setServersOpen(false)} />
      )}
    </div>
  );
}
