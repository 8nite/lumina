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
import { Sidebar } from './components/Sidebar';
import { HomeView } from './components/HomeView';
import { SessionView, type Turn, type LiveStep, type TurnSummary, type QueuedPrompt } from './components/SessionView';
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
  const [reviewOpen, setReviewOpen] = useState(false);
  const [rightTab, setRightTab] = useState('review');
  const [serversOpen, setServersOpen] = useState(false);
  const [modelOptions, setModelOptions] = useState<string[]>([]);
  const [homeAgent, setHomeAgent] = useState('build');
  const [homePerm, setHomePerm] = useState('bypass');
  const [homeModel, setHomeModel] = useState('');
  const [working, setWorking] = useState(false);
  const [liveText, setLiveText] = useState('');
  const [liveThinking, setLiveThinking] = useState('');
  const [liveSteps, setLiveSteps] = useState<LiveStep[]>([]);
  const [stage, setStage] = useState('');
  const [summary, setSummary] = useState<TurnSummary | null>(null);
  const [ctx, setCtx] = useState<{ used: number; max: number } | null>(null);
  const [permReq, setPermReq] = useState<{ request_id: string; tool: string; target: string } | null>(null);
  type QOption = { label: string; description?: string };
  type QModel = { question: string; header?: string; options?: QOption[]; multiple?: boolean };
  const [qReq, setQReq] = useState<{ request_id: string; questions: QModel[] } | null>(null);
  // Per-question UI state: selected option labels + custom text.
  const [qSel, setQSel] = useState<string[][]>([]);
  const [qText, setQText] = useState<string[]>([]);
  const [turnStart, setTurnStart] = useState<number | null>(null);
  const [now, setNow] = useState(Date.now());
  const [attachments, setAttachments] = useState<PromptAttachment[]>([]);
  const [effort, setEffort] = useState('xhigh');
  // v2 working-event state: queued prompts behind the active turn, terminal
  // turn error, per-turn file changes. queued mirrors the daemon inbox for
  // prompts sent with queue delivery while a turn runs (no list/cancel
  // endpoint exists, so rows are display-only until they deliver).
  const [queued, setQueued] = useState<QueuedPrompt[]>([]);
  const [turnError, setTurnError] = useState<string | null>(null);
  const [changed, setChanged] = useState<{ files: string[]; stat: string } | null>(null);
  const lastPrompt = useRef('');
  // Synchronous mirror of queued: the SSE callback re-subscribes per
  // session but still sees stale state, so delivery reads the ref and
  // publishes both updates outside of any updater (updaters must be pure).
  const queuedRef = useRef<QueuedPrompt[]>([]);
  const setQueuedBoth = (q: QueuedPrompt[]) => {
    queuedRef.current = q;
    setQueued(q);
  };
  // v2 shell state: vertical tabs sidebar + color scheme.
  const [sidebarOpen, setSidebarOpen] = useState(() => {
    try {
      return localStorage.getItem('lumina.sidebarOpen') === '1';
    } catch {
      return false;
    }
  });
  const [scheme, setScheme] = useState<'light' | 'dark'>(() => {
    try {
      const s = localStorage.getItem('lumina-color-scheme');
      return s === 'light' ? 'light' : 'dark';
    } catch {
      return 'dark';
    }
  });

  useEffect(() => {
    try {
      localStorage.setItem('lumina.sidebarOpen', sidebarOpen ? '1' : '0');
    } catch {
      /* ignore */
    }
  }, [sidebarOpen]);

  useEffect(() => {
    try {
      localStorage.setItem('lumina-color-scheme', scheme);
    } catch {
      /* ignore */
    }
    document.documentElement.dataset.colorScheme = scheme;
    document.documentElement.style.backgroundColor = scheme === 'dark' ? '#080808' : '#fafafa';
  }, [scheme]);

  const refresh = useCallback(() => {
    api.sessions().then(setSessions).catch(() => {});
  }, []);

  // Live mirror of the active session for the SSE callback. The subscription
  // is intentionally long-lived (one per API base); reading the ref avoids
  // resubscribe churn that drops events published mid-switch, and avoids
  // the stale-closure bug where turn.completed was ignored after a reload
  // because the handler still saw session=null.
  const sessionRef = useRef<Session | null>(null);
  sessionRef.current = session;
  // Mirror of turns for the same reason: turn.started must not overwrite a
  // just-sent optimistic user row with a stale export that raced ahead of
  // the daemon's AddTurn, and a reconnected client must insert the event
  // text it never appended via send().
  const turnsRef = useRef<Turn[]>([]);
  turnsRef.current = turns;

  // Resync one session from the daemon: transcript (history always renders,
  // even when turn.completed was missed offline) + running flag (spinner
  // recovers when turn.started was missed) + pending approval/question
  // modals (a turn waiting on approval no longer looks stuck).
  const syncActiveSession = useCallback(async (sid: string) => {
    try {
      const st = await api.sessionState(sid);
      const cur = sessionRef.current;
      if (cur && cur.id === sid) setSession({ ...cur, ...st.session });
      else setSession(st.session);
      if (st.session.workspace_dir) setWorkspace(st.session.workspace_dir);
      if (st.running) {
        setWorking((was) => {
          if (!was) {
            setTurnStart(Date.now());
            setNow(Date.now());
            setStage((prev) => prev || 'reconnected — catching up…');
          }
          return true;
        });
      } else {
        // Missed the terminal event while offline: stop the spinner and
        // fall through to the export refetch below so history renders.
        setWorking(false);
        setTurnStart(null);
        setLiveText('');
        setLiveThinking('');
        setLiveSteps([]);
        if (!st.permission && !st.question) setStage('');
      }
      if (st.permission?.request_id) {
        setPermReq({ request_id: st.permission.request_id, tool: st.permission.tool || 'tool', target: st.permission.target || '' });
        setStage((prev) => prev || `waiting approval: ${st.permission!.tool || 'tool'}`.trim());
      }
      if (st.question?.request_id && st.question.questions) {
        const qs = st.question.questions as { question: string; header?: string; options?: { label: string; description?: string }[]; multiple?: boolean }[];
        setQReq({ request_id: st.question.request_id, questions: qs });
        setQSel(qs.map(() => []));
        setQText(qs.map(() => ''));
      }
    } catch {
      /* state endpoint missing on old daemon; export fallback below */
    }
    try {
      const ex = await api.export(sid);
      setTurns(
        (ex.transcript || [])
          .filter((t) => t.role === 'user' || t.role === 'assistant' || t.role === 'tool' || t.role === 'tool_error')
          .map((t) => ({ role: t.role, text: t.text, created_at: t.created_at })),
      );
    } catch {
      /* offline transcript keeps whatever is on screen */
    }
  }, []);

  // Deep-link (?session=<id>): embedded hosts (ai-manager iframe at
  // /s/:id/?session=<hid>) land directly in the bound chat instead of the
  // home view. One-shot: once honored (or when the user already picked a
  // session), never re-fires.
  const deepLinked = useRef(false);
  useEffect(() => {
    if (deepLinked.current || session) return;
    let id: string | null = null;
    try {
      id = new URLSearchParams(location.search).get('session');
    } catch {
      /* non-browser */
    }
    if (id) {
      const hit = sessions.find((s) => s.id === id);
      if (hit) {
        deepLinked.current = true;
        void pickSession(hit);
        return;
      }
    }
    // Reload recovery: the active session survives a browser refresh via
    // localStorage, so a long task keeps its timeline + spinner instead of
    // landing on the home view with history apparently gone.
    try {
      const saved = localStorage.getItem('lumina.activeSession');
      if (saved) {
        const hit = sessions.find((s) => s.id === saved);
        if (hit) {
          deepLinked.current = true;
          void pickSession(hit);
        }
      }
    } catch {
      /* private mode */
    }
  }, [sessions, session]);

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
    if (!working) return;
    const t = setInterval(() => setNow(Date.now()), 500);
    return () => clearInterval(t);
  }, [working]);

  useEffect(() => {
    refresh();
    refreshModels();
    // Long-lived subscription: deps are [base] only. Session filtering uses
    // sessionRef so no resubscribe gap drops turn.started/completed, and a
    // reload that missed events resyncs via onConnect below.
    return subscribe((e: DaemonEvent) => {
      const activeId = sessionRef.current?.id;
      const forSession = !e.session_id || !activeId || e.session_id === activeId;
      if (e.type === 'turn.started' && forSession) {
        setWorking(true);
        setTurnStart(Date.now());
        setNow(Date.now());
        setLiveText('');
        setLiveThinking('');
        setLiveSteps([]);
        setStage('');
        setSummary(null);
        setCtx(null);
        setTurnError(null);
        const d0 = (e.data || {}) as { text?: string };
        const startedText = (d0.text || '').trim();
        // Reconcile the user row without ever losing it:
        // - sender already appended it optimistically via send(): keep it,
        //   never overwrite with a stale export that raced ahead of the
        //   daemon's AddTurn (that race wiped the just-sent message for the
        //   whole turn — the "history didn't render" half of the bug).
        // - reloaded client never saw send(): insert the event text now so
        //   history shows immediately, then converge on the export below.
        const local = turnsRef.current;
        const lastUser = [...local].reverse().find((t) => t.role === 'user');
        const hasStartedText = startedText && lastUser && lastUser.text.startsWith(startedText.slice(0, 80));
        // Queued-chain delivery appends below (with file names), so skip the
        // generic insert there to avoid a duplicate user row.
        if (startedText && !hasStartedText && queuedRef.current.length === 0) {
          const cur = sessionRef.current;
          setTurns((t) => [...t, { role: 'user', text: startedText, agent: cur?.agent, model: cur?.model }]);
        }
        if (e.session_id) {
          const sid = e.session_id;
          api.export(sid).then((ex) => {
            if (sessionRef.current?.id !== sid) return;
            const exported = (ex.transcript || [])
              .filter((t) => t.role === 'user' || t.role === 'assistant' || t.role === 'tool' || t.role === 'tool_error')
              .map((t) => ({ role: t.role, text: t.text, created_at: t.created_at }));
            // Stale-export guard: the daemon publishes turn.started before
            // its async Exec persists the user turn, so the export can be
            // shorter than what we already show. Never shrink the timeline
            // here; turn.completed (post-persist) is the authoritative sync.
            setTurns((prev) => (exported.length < prev.length ? prev : exported as Turn[]));
          }).catch(() => {});
        }
        // A chained queued turn delivers here (v2 Activity semantics): move
        // the head of the local queue into the timeline as its user message.
        const d = (e.data || {}) as { text?: string };
        const head = queuedRef.current[0];
        if (head) {
          setQueuedBoth(queuedRef.current.slice(1));
          const delivered = d.text && d.text.trim() ? d.text : head.text;
          const shown = delivered + head.files.map((f) => `\n[file: ${f}]`).join('');
          const cur = sessionRef.current;
          setTurns((t) => [...t, { role: 'user', text: shown, agent: cur?.agent, model: cur?.model }]);
        }
      }
      if (e.type === 'turn.context' && forSession) {
        const d = (e.data || {}) as { context_tokens?: number; max_tokens?: number };
        if (d.context_tokens) setCtx({ used: d.context_tokens, max: d.max_tokens || 0 });
      }
      if ((e.type === 'turn.delta') && forSession) {
        const d = (e.data || {}) as { delta?: string; thinking?: string };
        // A delta for a session we thought was idle means we missed
        // turn.started while offline: recover the spinner now.
        if (d.delta || d.thinking) {
          setWorking((was) => {
            if (!was && sessionRef.current) {
              setTurnStart(Date.now());
              setNow(Date.now());
            }
            return true;
          });
        }
        if (d.delta) setLiveText((t) => (t + d.delta!).slice(-8000));
        if (d.thinking) setLiveThinking((t) => (t + d.thinking!).slice(-4000));
      }
      if (e.type === 'turn.progress' && forSession) {
        const d = (e.data || {}) as { stage?: string };
        if (d.stage) {
          const stage: string = d.stage;
          // Progress for an apparently-idle session also implies a missed
          // turn.started (e.g. tool: bash after a refresh).
          setWorking((was) => {
            if (!was && sessionRef.current && !stage.startsWith('queued #')) {
              setTurnStart(Date.now());
              setNow(Date.now());
            }
            return stage.startsWith('queued #') ? was : true;
          });
          setStage(stage);
        }
      }
      if (e.type === 'turn.tool' && forSession) {
        const d = (e.data || {}) as Partial<LiveStep> & { phase?: 'start' | 'done' };
        if (d.tool) {
          setWorking((was) => {
            if (!was && sessionRef.current) {
              setTurnStart(Date.now());
              setNow(Date.now());
            }
            return true;
          });
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
      if (e.type === 'turn.completed' && e.session_id && e.session_id === activeId) {
        const sid = e.session_id;
        const d = (e.data || {}) as { text?: string; model?: string; duration_ms?: number; output_tokens?: number; input_tokens?: number; context_tokens?: number; max_tokens?: number };
        setSummary({ model: d.model, duration_ms: d.duration_ms, output_tokens: d.output_tokens, input_tokens: d.input_tokens, context_tokens: d.context_tokens, max_tokens: d.max_tokens });
        if (d.context_tokens) setCtx({ used: d.context_tokens, max: d.max_tokens || 0 });
        api.export(sid).then((ex) => {
          if (sessionRef.current?.id !== sid) return;
          setTurns(
            (ex.transcript || [])
              .filter((t) => t.role === 'user' || t.role === 'assistant' || t.role === 'tool' || t.role === 'tool_error')
              .map((t) => ({ role: t.role, text: t.text, created_at: t.created_at })),
          );
        }).catch(() => {
          const d = e.data as { text?: string };
          setTurns((t) => [...t, { role: 'assistant', text: d?.text ?? '' }]);
        });
        // v2 TurnDiff row: per-turn file changes once the session idles.
        const ws = sessionRef.current?.workspace_dir || '.';
        api.diff(ws).then((r) => {
          setChanged({ files: r.files || [], stat: r.stat || '' });
        }).catch(() => {});
        setLiveText('');
        setLiveThinking('');
        setLiveSteps([]);
        setStage('');
        setWorking(false);
        setTurnStart(null);
      }
      if (e.type === 'turn.failed' && forSession) {
        const d = (e.data || {}) as { error?: string };
        setTurnError(d.error || 'turn failed');
        setLiveText('');
        setLiveThinking('');
        setLiveSteps([]);
        setStage('');
        setSummary(null);
        setWorking(false);
        setTurnStart(null);
      }
      if (e.type === 'permission.asked' && forSession) {
        const d = (e.data || {}) as { request_id?: string; tool?: string; target?: string };
        if (d.request_id) {
          setWorking(true);
          setPermReq({ request_id: d.request_id, tool: d.tool || 'tool', target: d.target || '' });
          setStage(`waiting approval: ${d.tool || 'tool'} ${d.target || ''}`.trim());
        }
      }
      if (e.type === 'permission.resolved' && forSession) {
        const d = (e.data || {}) as { decision?: string };
        setPermReq(null);
        if (d.decision) setStage(`permission: ${d.decision}`);
      }
      if (e.type === 'question.asked' && forSession) {
        const d = (e.data || {}) as { request_id?: string; questions?: QModel[] };
        if (d.request_id && d.questions) {
          setWorking(true);
          setQReq({ request_id: d.request_id, questions: d.questions });
          setQSel(d.questions.map(() => []));
          setQText(d.questions.map(() => ''));
          setStage('question: awaiting answer');
        }
      }
      if (e.type === 'question.resolved' && forSession) {
        setQReq(null);
        setStage('question: answered');
      }
      if (e.type === 'session.created' || e.type === 'session.updated' || e.type === 'auth.connected') refresh();
    }, () => {
      // SSE (re)connected: pull the active session's authoritative state so
      // anything missed while disconnected (deltas, terminal event, pending
      // approval) reconciles instead of leaving a dead spinner + no history.
      const sid = sessionRef.current?.id;
      if (sid) void syncActiveSession(sid);
      refresh();
    });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [base]);

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

  // Tab = agent toggle, Shift+Tab = approval cycle, mod+shift+b = sidebar.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if ((e.metaKey || e.ctrlKey) && e.shiftKey && (e.key === 'b' || e.key === 'B')) {
        e.preventDefault();
        setSidebarOpen((v) => !v);
        return;
      }
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

  const onAttach = async (files: FileList | File[] | null) => {
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
    setCtx(null);
    setPermReq(null);
    setQReq(null);
    setTurnStart(null);
    setWorking(false);
    setLiveText('');
    setLiveThinking('');
    setLiveSteps([]);
    setStage('');
    setSummary(null);
    setQueuedBoth([]);
    setTurnError(null);
    setChanged(null);
    try {
      if (s) localStorage.setItem('lumina.activeSession', s.id);
      else localStorage.removeItem('lumina.activeSession');
    } catch {
      /* private mode */
    }
    if (s) {
      setWorkspace(s.workspace_dir);
      // Full resync (not just export): restores the spinner + pending
      // modals when the session is mid-turn, and guarantees history
      // renders even if the terminal SSE was missed.
      await syncActiveSession(s.id);
    }
  };

  const send = async (delivery: 'steer' | 'queue' = 'steer', overrideText?: string) => {
    const text = (overrideText ?? input).trim();
    if (!text && attachments.length === 0) return;
    setInput('');
    const atts = attachments;
    setAttachments([]);
    let s = session;
    if (!s) {
      s = await api.createSession(workspace || '.', homeAgent, homePerm);
      if (homeModel) await api.patchSession(s.id, { model: homeModel });
      setSession(s);
      try {
        localStorage.setItem('lumina.activeSession', s.id);
      } catch {
        /* private mode */
      }
      refresh();
    }
    if (text) lastPrompt.current = text;
    setTurnError(null);
    setChanged(null);
    // v2 queue semantics: a prompt queued behind a running turn waits in
    // the queue panel and enters the timeline when its chained turn starts.
    if (delivery === 'queue' && working && s) {
      setQueuedBoth([...queuedRef.current, { text, files: atts.map((a) => a.name) }]);
      try {
        const r = await api.prompt(s.id, text, atts, effort || undefined, delivery);
        if (r.status === 'queued') setStage(`queued #${r.position ?? ''}`.trim());
      } catch (err) {
        setQueuedBoth(queuedRef.current.slice(0, -1));
        alert(String(err));
      }
      return;
    }
    const shown = text + atts.map((a) => `\n[file: ${a.name}]`).join('');
    setTurns((t) => [...t, { role: 'user', text: shown, agent: s.agent, model: s.model, created_at: new Date().toISOString() }]);
    setWorking(true);
    setTurnStart(Date.now());
    setNow(Date.now());
    try {
      const r = await api.prompt(s.id, text, atts, effort || undefined, delivery);
      if (r.status === 'queued') setStage(`queued #${r.position ?? ''}`.trim());
    } catch (err) {
      setWorking(false);
      setTurnStart(null);
      alert(String(err));
    }
  };

  const retry = () => {
    if (!session || !lastPrompt.current) return;
    void send('steer', lastPrompt.current);
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
    try {
      localStorage.removeItem('lumina.activeSession');
    } catch {
      /* ignore */
    }
    setSession(null);
    setTurns([]);
    setCtx(null);
    setPermReq(null);
    setQReq(null);
    setTurnStart(null);
    setWorking(false);
    setLiveText('');
    setLiveThinking('');
    setLiveSteps([]);
    setStage('');
    setSummary(null);
    setQueuedBoth([]);
    setTurnError(null);
    setChanged(null);
  };

  const answerPerm = async (decision: string) => {
    if (!permReq) return;
    const id = permReq.request_id;
    setPermReq(null);
    try {
      await api.answerPermission(id, decision);
    } catch (err) {
      alert(String(err));
    }
  };

  const toggleQOpt = (qi: number, label: string) => {
    if (!qReq) return;
    const multi = qReq.questions[qi]?.multiple;
    setQSel((prev) => {
      const next = prev.map((a) => [...a]);
      if (multi) {
        next[qi] = next[qi].includes(label) ? next[qi].filter((l) => l !== label) : [...next[qi], label];
      } else {
        next[qi] = [label];
      }
      return next;
    });
  };

  const submitQuestions = async () => {
    if (!qReq) return;
    const answers = qReq.questions.map((q, i) => {
      const custom = (qText[i] || '').trim();
      if (custom) return [custom];
      if (qSel[i].length) return qSel[i];
      return [(q.options || [])[0]?.label || ''];
    });
    const id = qReq.request_id;
    setQReq(null);
    try {
      await api.answerQuestion(id, answers);
    } catch (err) {
      alert(String(err));
    }
  };

  const dismissQuestions = async () => {
    if (!qReq) return;
    const id = qReq.request_id;
    setQReq(null);
    try {
      await api.dismissQuestion(id);
    } catch (err) {
      alert(String(err));
    }
  };

  const elapsedMs = turnStart != null ? Math.max(0, now - turnStart) : 0;

  return (
    <div
      className="relative flex h-screen min-h-0 min-w-0 flex-1 flex-col bg-[var(--v2-bg-deep)] text-[13px] text-[var(--v2-text-base)] select-none [&_input]:select-text [&_textarea]:select-text"
      data-color-scheme={scheme}
    >
      <div className="relative flex min-h-0 flex-1 flex-row">
        {sidebarOpen && (
          <Sidebar
            sessions={sessions}
            activeId={session?.id ?? null}
            onSelect={(s) => void pickSession(s)}
            onNew={() => void pickSession(null)}
            open={sidebarOpen}
            width={276}
            host={host}
            onShowServers={() => setServersOpen(true)}
            scheme={scheme}
            onScheme={setScheme}
          />
        )}
        <main className="flex min-h-0 min-w-0 flex-1 flex-col" style={{ padding: '8px 8px 8px 0' }}>
          <div className="v2-panel flex min-h-0 flex-1 flex-col overflow-hidden">
            <TopBar
              session={session}
              onToggleSidebar={() => setSidebarOpen((v) => !v)}
              onNew={() => void pickSession(null)}
              onFork={() => session && api.fork(session.id).then((f) => { pickSession(f); refresh(); })}
              onExport={() => {
                if (!session) return;
                api.export(session.id).then((d) => {
                  console.log('transcript', d.transcript);
                  alert(`${d.transcript.length} turns exported (see console)`);
                });
              }}
              onToggleReview={() => setReviewOpen(!reviewOpen)}
              reviewOpen={reviewOpen}
              ctx={ctx}
            />
            <div className="flex min-h-0 flex-1 flex-row gap-2 p-2">
              <div className="flex min-h-0 min-w-0 flex-1 flex-col">
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
                    sessions={sessions}
                    onOpenSession={(s) => void pickSession(s)}
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
                    ctx={ctx}
                    elapsedMs={elapsedMs}
                    error={turnError}
                    onRetry={retry}
                    changed={changed}
                    queue={queued}
                  />
                )}
              </div>
              {reviewOpen && session && (
                <aside className="hidden w-[420px] shrink-0 md:block">
                  <div className="flex h-full min-h-0 flex-col overflow-hidden rounded-[10px] bg-[var(--v2-bg-base)]" style={{ border: '1px solid var(--v2-border-base)' }}>
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
                  </div>
                </aside>
              )}
            </div>
          </div>
        </main>
      </div>
      {permReq && (
        <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/60">
          <div className="w-96 rounded-[12px] bg-[var(--v2-bg-base)] p-4 shadow-[var(--v2-elevation-floating)]" style={{ border: '1px solid var(--v2-border-base)' }}>
            <div className="v2-title mb-1 text-[13px]">Approve tool call?</div>
            <div className="mb-4 font-mono text-[12px] text-[var(--v2-text-muted)]">
              {permReq.tool} {permReq.target.slice(0, 120)}
            </div>
            <div className="flex gap-2">
              <button
                className="v2-row flex-1 px-3 py-1.5 text-[12px]"
                style={{ background: 'var(--v2-bg-layer-02)' }}
                onClick={() => void answerPerm('allow_once')}
              >
                Allow once
              </button>
              <button
                className="v2-row flex-1 px-3 py-1.5 text-[12px]"
                style={{ background: 'var(--v2-bg-layer-02)' }}
                onClick={() => void answerPerm('allow_session')}
              >
                Allow session
              </button>
              <button
                className="flex-1 rounded-[6px] border border-red-500/30 px-3 py-1.5 text-[12px] text-red-500 hover:bg-red-500/10"
                onClick={() => void answerPerm('deny')}
              >
                Deny
              </button>
            </div>
          </div>
        </div>
      )}
      {qReq && (
        <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/60">
          <div className="max-h-[80vh] w-[28rem] overflow-y-auto rounded-[12px] bg-[var(--v2-bg-base)] p-4 shadow-[var(--v2-elevation-floating)]" style={{ border: '1px solid var(--v2-border-base)' }}>
            <div className="v2-title mb-3 text-[13px]">Answer the model&apos;s questions</div>
            {(qReq.questions || []).map((q, qi) => (
              <div key={qi} className="mb-3">
                <div className="text-[12px] font-medium text-[var(--v2-text-base)]">
                  {q.header ? `${q.header}: ` : ''}{q.question}
                  {q.multiple ? ' (pick several)' : ''}
                </div>
                <div className="mt-1 flex flex-wrap gap-1">
                  {(q.options || []).map((o) => {
                    const on = (qSel[qi] || []).includes(o.label);
                    return (
                      <button
                        key={o.label}
                        title={o.description || ''}
                        className="rounded-[6px] px-2 py-1 text-[12px]"
                        style={on ? { background: 'var(--v2-text-muted)', color: 'var(--v2-bg-base)' } : { background: 'var(--v2-bg-layer-02)' }}
                        onClick={() => void toggleQOpt(qi, o.label)}
                      >
                        {o.label}
                      </button>
                    );
                  })}
                </div>
                <input
                  className="mt-1 w-full rounded-[6px] bg-[var(--v2-bg-layer-02)] px-2 py-1 text-[12px] outline-none"
                  placeholder="...or type your own answer"
                  value={qText[qi] || ''}
                  onChange={(e) => setQText((prev) => prev.map((t, i) => (i === qi ? e.target.value : t)))}
                />
              </div>
            ))}
            <div className="flex gap-2">
              <button
                className="v2-row flex-1 px-3 py-1.5 text-[12px]"
                style={{ background: 'var(--v2-bg-layer-02)' }}
                onClick={() => void submitQuestions()}
              >
                Submit answers
              </button>
              <button
                className="flex-1 rounded-[6px] border border-red-500/30 px-3 py-1.5 text-[12px] text-red-500 hover:bg-red-500/10"
                onClick={() => void dismissQuestions()}
              >
                Dismiss
              </button>
            </div>
          </div>
        </div>
      )}
      {serversOpen && (
        <ServersModal active={base} onPick={switchServer} onClose={() => setServersOpen(false)} />
      )}
    </div>
  );
}
