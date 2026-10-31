//! Terminal TUI (ratatui + crossterm) talking to the daemon via `sdk`.
//! Mirrors the Go Bubble Tea client: Tab agent flip, Shift+Tab approval cycle,
//! streaming timeline, inline permission/question modals, /command + @file +
//! [[memory]] autocomplete, /resume + /model pickers, steer/queue semantics.

use crate::sdk::{Event, Sdk, SessionView};
use crossterm::event::{self, Event as CEvent, KeyCode, KeyModifiers};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph},
    Terminal,
};
use std::{
    io::Stdout,
    time::{Duration, Instant},
};

pub struct LaunchOpts {
    pub base_url: String,
    pub token: String,
    pub cont: bool,
    pub session: Option<String>,
    pub agent: Option<String>,
    pub mode: Option<String>,
}

const EFFORTS: [&str; 6] = ["auto", "low", "medium", "high", "xhigh", "max"];
const MODES: [&str; 4] = ["ask", "auto-edit", "auto-approve", "bypass"];

struct PermModal {
    req_id: String,
    tool: String,
    target: String,
    choice: usize, // 0 once, 1 session, 2 deny
}

struct QModal {
    req_id: String,
    questions: Vec<Q>,
    qi: usize,
    oi: usize,
}
struct Q {
    text: String,
    options: Vec<String>,
}

enum Popup {
    Complete { items: Vec<String>, idx: usize },
    Models { providers: Vec<String>, pi: usize, models: Vec<String>, mi: usize, picking_model: bool },
    Sessions { list: Vec<SessionView>, idx: usize },
}

struct App {
    sdk: Sdk,
    session: SessionView,
    workspace: String,
    messages: Vec<(String, String)>,
    input: Vec<char>,
    cursor: usize,
    scroll: usize,
    follow: bool,
    running: bool,
    queued: Option<String>,
    effort: String,
    popup: Option<Popup>,
    perm: Option<PermModal>,
    ques: Option<QModal>,
    status: String,
    status_at: Instant,
}

impl App {
    fn set_status(&mut self, s: &str) {
        self.status = s.to_string();
        self.status_at = Instant::now();
    }

    fn input_text(&self) -> String {
        self.input.iter().collect()
    }

    fn push_msg(&mut self, role: &str, text: String) {
        self.messages.push((role.to_string(), text));
        if self.follow {
            self.scroll = usize::MAX;
        }
    }

    fn append_delta(&mut self, delta: &str) {
        if delta.is_empty() {
            return;
        }
        match self.messages.last_mut() {
            Some((r, t)) if r == "assistant" && self.running => t.push_str(delta),
            _ => self.messages.push(("assistant".into(), delta.to_string())),
        }
        if self.follow {
            self.scroll = usize::MAX;
        }
    }

    fn submit(&mut self) {
        let text: String = self.input_text();
        if text.trim().is_empty() {
            return;
        }
        self.input.clear();
        self.cursor = 0;
        self.popup = None;
        if text.starts_with('/') {
            self.dispatch_command(&text);
            return;
        }
        self.send_prompt(text);
    }

    fn send_prompt(&mut self, text: String) {
        if self.running {
            self.queued = Some(text);
            self.set_status("queued (running turn active)");
            return;
        }
        self.push_msg("user", text.clone());
        self.running = true;
        let sdk = self.sdk.clone();
        let id = self.session.id.clone();
        let effort = self.effort.clone();
        std::thread::spawn(move || {
            // Result surfaces over SSE (turn.completed/failed); nothing to do here.
            let _ = sdk.submit(&id, &text, &effort);
        });
    }

    fn dispatch_command(&mut self, text: &str) {
        let mut parts = text[1..].split_whitespace();
        let cmd = parts.next().unwrap_or("");
        let rest: Vec<&str> = parts.collect();
        match cmd {
            "plan" => self.set_agent("plan"),
            "build" => self.set_agent("build"),
            "mode" => match rest.first() {
                Some(m) if MODES.contains(m) => self.set_mode(m),
                _ => self.set_status("usage: /mode ask|auto-edit|auto-approve|bypass"),
            },
            "variant" | "effort" => match rest.first() {
                Some(v) if EFFORTS.contains(v) => {
                    self.effort = v.to_string();
                    self.set_status(&format!("effort → {}", self.effort));
                }
                _ => self.set_status("usage: /variant auto|low|medium|high|xhigh|max"),
            },
            "model" => self.open_models(),
            "resume" => self.open_sessions(),
            "continue" => self.continue_latest(),
            "fork" => self.fork(rest.first().copied()),
            "graphify" => {
                let q = rest.join(" ");
                if q.is_empty() {
                    self.set_status("usage: /graphify <query>");
                    return;
                }
                match self.sdk.kb_query(&q) {
                    Ok(ctx) => self.push_msg("system", format!("graphify: {q}\n{ctx}")),
                    Err(e) => self.set_status(&format!("graphify: {e}")),
                }
            }
            "clear" => {
                self.messages.clear();
                self.set_status("cleared");
            }
            "help" => self.push_msg("system", HELP.into()),
            "compact" | "thinking" | "undo" | "redo" | "evolve" | "init" | "connect" => {
                self.set_status(&format!("/{cmd} not yet ported in Rust M3 (lands with M5/M6)"));
            }
            _ => self.set_status(&format!("unknown /{cmd} — try /help")),
        }
    }

    fn set_agent(&mut self, agent: &str) {
        match self.sdk.patch(&self.session.id, &serde_json::json!({"agent": agent})) {
            Ok(s) => {
                self.session = s;
                self.set_status(&format!("agent → {agent}"));
            }
            Err(e) => self.set_status(&format!("agent: {e}")),
        }
    }

    fn set_mode(&mut self, mode: &str) {
        match self.sdk.patch(&self.session.id, &serde_json::json!({"permission_mode": mode})) {
            Ok(s) => {
                self.session = s;
                self.set_status(&format!("approval → {mode}"));
            }
            Err(e) => self.set_status(&format!("mode: {e}")),
        }
    }

    fn cycle_agent(&mut self) {
        let next = if self.session.agent == "plan" { "build" } else { "plan" };
        self.set_agent(next);
    }

    fn cycle_mode(&mut self) {
        let cur = MODES.iter().position(|m| *m == self.session.permission_mode).unwrap_or(3);
        self.set_mode(MODES[(cur + 1) % MODES.len()]);
    }

    fn open_models(&mut self) {
        match self.sdk.providers() {
            Ok(p) => {
                let providers: Vec<String> = p.iter().filter_map(|v| v.get("name")?.as_str().map(|s| s.to_string())).collect();
                self.popup = Some(Popup::Models { providers, pi: 0, models: vec![], mi: 0, picking_model: false });
            }
            Err(e) => self.set_status(&format!("models: {e}")),
        }
    }

    fn open_sessions(&mut self) {
        match self.sdk.list_sessions(Some(&self.workspace), false) {
            Ok(list) => self.popup = Some(Popup::Sessions { list, idx: 0 }),
            Err(e) => self.set_status(&format!("resume: {e}")),
        }
    }

    fn continue_latest(&mut self) {
        match self.sdk.latest(&self.workspace) {
            Ok(s) => self.attach_session(s),
            Err(e) => self.set_status(&format!("continue: {e}")),
        }
    }

    fn fork(&mut self, turn: Option<&str>) {
        match self.sdk.fork(&self.session.id, turn) {
            Ok(s) => {
                self.attach_session(s);
                self.set_status("forked");
            }
            Err(e) => self.set_status(&format!("fork: {e}")),
        }
    }

    fn attach_session(&mut self, s: SessionView) {
        self.session = s;
        self.messages.clear();
        self.load_transcript();
        self.set_status("session attached");
    }

    /// Best-effort transcript restore via export (## role sections).
    fn load_transcript(&mut self) {
        // ponytail: export is markdown; parse the ## role headers directly.
        let md = self.sdk.export(&self.session.id).unwrap_or_default();
        let mut role = String::new();
        let mut buf = String::new();
        let flush = |role: &mut String, buf: &mut String, msgs: &mut Vec<(String, String)>| {
            if (role == "user" || role == "assistant") && !buf.trim().is_empty() {
                msgs.push((std::mem::take(role), std::mem::take(buf)));
            } else {
                buf.clear();
            }
        };
        for line in md.lines() {
            if let Some(h) = line.strip_prefix("## ") {
                flush(&mut role, &mut buf, &mut self.messages);
                role = h.trim().to_string();
                continue;
            }
            if role == "user" || role == "assistant" {
                buf.push_str(line);
                buf.push('\n');
            }
        }
        flush(&mut role, &mut buf, &mut self.messages);
    }

    fn on_event(&mut self, ev: Event) {
        match ev {
            Event::TurnStarted { .. } => {
                self.running = true;
            }
            Event::TurnDelta { delta, .. } => self.append_delta(&delta),
            Event::TurnCompleted { .. } => {
                self.running = false;
                if let Some(q) = self.queued.take() {
                    self.send_prompt(q);
                }
            }
            Event::TurnFailed { error, .. } => {
                self.running = false;
                self.push_msg("system", format!("turn failed: {error}"));
            }
            Event::PermissionAsked { req_id, tool, target, .. } => {
                self.perm = Some(PermModal { req_id, tool, target, choice: 0 });
            }
            Event::PermissionResolved { .. } => {}
            Event::QuestionAsked { req_id, questions, .. } => {
                self.ques = Some(QModal {
                    req_id,
                    questions: questions.into_iter().map(|q| Q { text: q.text, options: q.options }).collect(),
                    qi: 0,
                    oi: 0,
                });
            }
            Event::ConfigUpdated { role, model } => self.set_status(&format!("{role} → {model}")),
            Event::Other { .. } => {}
        }
    }

    fn update_complete(&mut self) {
        let text = self.input_text();
        let (kind, prefix) = if let Some(rest) = text.strip_prefix('/') {
            ("cmd", rest)
        } else if let Some(at) = text.rfind('@') {
            let before_ok = text[..at].chars().last().map(|c| c.is_whitespace()).unwrap_or(true);
            if before_ok {
                ("file", &text[at + 1..])
            } else {
                self.popup = None;
                return;
            }
        } else if let Some(idx) = text.rfind("[[") {
            ("memory", &text[idx + 2..])
        } else {
            self.popup = None;
            return;
        };
        let items = match kind {
            "cmd" => ["plan", "build", "mode", "model", "variant", "resume", "continue", "fork", "graphify", "clear", "help"]
                .into_iter()
                .filter(|c| c.starts_with(prefix))
                .map(|s| format!("/{s}"))
                .collect(),
            "file" => complete_files(&self.workspace, prefix),
            "memory" => complete_memory(&self.workspace, prefix),
            _ => vec![],
        };
        if items.is_empty() {
            self.popup = None;
        } else {
            self.popup = Some(Popup::Complete { items, idx: 0 });
        }
    }

    fn accept_complete(&mut self) {
        let Some(Popup::Complete { items, idx }) = &self.popup else { return };
        let pick = items[*idx].clone();
        let text = self.input_text();
        if pick.starts_with('/') {
            self.input = pick.chars().collect();
            self.input.push(' ');
        } else if let Some(at) = text.rfind('@') {
            let mut head: Vec<char> = text[..at + 1].chars().collect();
            head.extend(pick.chars());
            self.input = head;
        } else if let Some(idx2) = text.rfind("[[") {
            let mut head: Vec<char> = text[..idx2 + 2].chars().collect();
            let stripped = pick.strip_suffix("]]").unwrap_or(&pick);
            head.extend(stripped.chars());
            self.input = head;
        }
        self.cursor = self.input.len();
        self.popup = None;
    }
}

fn complete_files(workspace: &str, prefix: &str) -> Vec<String> {
    let mut out = Vec::new();
    fn walk(dir: &std::path::Path, base: &std::path::Path, out: &mut Vec<String>, prefix: &str) {
        if out.len() >= 30 {
            return;
        }
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            if p.file_name().map(|n| n == ".git" || n == "target" || n == "node_modules").unwrap_or(false) {
                continue;
            }
            if p.is_dir() {
                walk(&p, base, out, prefix);
            } else if let Ok(rel) = p.strip_prefix(base) {
                let s = rel.display().to_string();
                if s.starts_with(prefix) {
                    out.push(s);
                }
            }
            if out.len() >= 30 {
                return;
            }
        }
    }
    walk(std::path::Path::new(workspace), std::path::Path::new(workspace), &mut out, prefix);
    out.sort();
    out
}

fn complete_memory(workspace: &str, prefix: &str) -> Vec<String> {
    let dir = std::path::Path::new(workspace).join("memory");
    let Ok(rd) = std::fs::read_dir(&dir) else { return vec![] };
    let mut out: Vec<String> = rd
        .flatten()
        .filter_map(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            if !n.ends_with(".md") || !n.starts_with(prefix) {
                return None;
            }
            Some(format!("[[{}]]", n.trim_end_matches(".md")))
        })
        .take(30)
        .collect();
    out.sort();
    out
}

const HELP: &str = "keys: Tab flip plan/build · Shift+Tab approval · Enter send · Ctrl+X queue · Esc cancel/close\n/plan /build /mode /model /variant /resume /continue /fork /graphify <q> /clear · @file paths · [[memory]] notes";

pub fn run(opts: LaunchOpts) -> Result<(), String> {
    let sdk = Sdk::new(&opts.base_url, &opts.token);
    let ws = std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_else(|_| ".".into());
    let session = if let Some(id) = &opts.session {
        sdk.get(id).map_err(|e| format!("session: {e}"))?
    } else if opts.cont {
        sdk.latest(&ws).map_err(|e| format!("continue: {e} (omit -c to start fresh)"))?
    } else {
        sdk.create(
            &ws,
            opts.agent.as_deref().unwrap_or("build"),
            opts.mode.as_deref().unwrap_or("bypass"),
            "",
        )
        .map_err(|e| format!("create: {e}"))?
    };
    let events = sdk.subscribe();
    let mut app = App {
        sdk,
        workspace: ws,
        session,
        messages: vec![],
        input: vec![],
        cursor: 0,
        scroll: 0,
        follow: true,
        running: false,
        queued: None,
        effort: "auto".into(),
        popup: None,
        perm: None,
        ques: None,
        status: String::new(),
        status_at: Instant::now(),
    };
    app.load_transcript();

    crossterm::terminal::enable_raw_mode().map_err(|e| e.to_string())?;
    let mut stdout = std::io::stdout();
    crossterm::execute!(stdout, crossterm::terminal::EnterAlternateScreen).map_err(|e| e.to_string())?;
    let backend = CrosstermBackend::new(stdout);
    let mut term = Terminal::new(backend).map_err(|e| e.to_string())?;
    let res = event_loop(&mut term, &mut app, events);
    crossterm::terminal::disable_raw_mode().map_err(|e| e.to_string())?;
    crossterm::execute!(term.backend_mut(), crossterm::terminal::LeaveAlternateScreen).map_err(|e| e.to_string())?;
    res
}

fn event_loop(
    term: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
    events: std::sync::mpsc::Receiver<Event>,
) -> Result<(), String> {
    loop {
        for ev in events.try_iter() {
            app.on_event(ev);
        }
        term.draw(|f| draw(f, app)).map_err(|e| e.to_string())?;
        if !event::poll(Duration::from_millis(100)).map_err(|e| e.to_string())? {
            continue;
        }
        let CEvent::Key(k) = event::read().map_err(|e| e.to_string())? else { continue };
        // Approval modal captures all keys.
        if app.perm.is_some() {
            match k.code {
                KeyCode::Left | KeyCode::Char('h') => {
                    if let Some(p) = &mut app.perm {
                        p.choice = (p.choice + 2) % 3;
                    }
                }
                KeyCode::Right | KeyCode::Char('l') => {
                    if let Some(p) = &mut app.perm {
                        p.choice = (p.choice + 1) % 3;
                    }
                }
                KeyCode::Enter => {
                    if let Some(p) = app.perm.take() {
                        let d = ["allow_once", "allow_session", "deny"][p.choice];
                        if let Err(e) = app.sdk.answer_permission(&p.req_id, d) {
                            app.set_status(&format!("approval: {e}"));
                        }
                    }
                }
                KeyCode::Esc => {
                    if let Some(p) = app.perm.take() {
                        let _ = app.sdk.answer_permission(&p.req_id, "deny");
                    }
                }
                _ => {}
            }
            continue;
        }
        // Question modal captures navigation keys.
        if app.ques.is_some() {
            match k.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    if let Some(q) = &mut app.ques {
                        let n = q.questions[q.qi].options.len().max(1);
                        q.oi = (q.oi + n - 1) % n;
                    }
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    if let Some(q) = &mut app.ques {
                        let n = q.questions[q.qi].options.len().max(1);
                        q.oi = (q.oi + 1) % n;
                    }
                }
                KeyCode::Enter => {
                    let done = if let Some(q) = &mut app.ques {
                        let ans = q.questions[q.qi].options.get(q.oi).cloned().unwrap_or_default();
                        // stash answer in status-adjacent vec via options reuse
                        q.questions[q.qi].options = vec![ans];
                        q.qi += 1;
                        q.oi = 0;
                        q.qi >= q.questions.len()
                    } else {
                        false
                    };
                    if done {
                        if let Some(q) = app.ques.take() {
                            let answers: Vec<String> = q.questions.into_iter().map(|x| x.options.into_iter().next().unwrap_or_default()).collect();
                            if let Err(e) = app.sdk.answer_question(&q.req_id, &answers) {
                                app.set_status(&format!("question: {e}"));
                            }
                        }
                    }
                }
                KeyCode::Esc => {
                    app.ques = None;
                    app.set_status("question dismissed (turn will time out)");
                }
                _ => {}
            }
            continue;
        }
        match (k.code, k.modifiers) {
            (KeyCode::Char('c'), m) if m.contains(KeyModifiers::CONTROL) => return Ok(()),
            (KeyCode::Char('d'), m) if m.contains(KeyModifiers::CONTROL) => return Ok(()),
            (KeyCode::Char('u'), m) if m.contains(KeyModifiers::CONTROL) => {
                app.input.clear();
                app.cursor = 0;
                app.popup = None;
            }
            (KeyCode::Char('x'), m) if m.contains(KeyModifiers::CONTROL) => {
                // Arm queue: next Enter queues behind the running turn.
                let t = app.input_text();
                if !t.trim().is_empty() {
                    app.input.clear();
                    app.cursor = 0;
                    if app.running {
                        app.queued = Some(t);
                        app.set_status("armed: queued behind running turn");
                    } else {
                        app.send_prompt(t);
                    }
                }
            }
            (KeyCode::Esc, _) => {
                if app.popup.is_some() {
                    app.popup = None;
                } else if app.queued.take().is_some() {
                    app.set_status("queue cancelled");
                }
            }
            (KeyCode::Tab, _) => app.cycle_agent(),
            (KeyCode::BackTab, _) => app.cycle_mode(),
            (KeyCode::Enter, _) => {
                if popup_open(app) {
                    popup_enter(app);
                } else {
                    app.submit();
                }
            }
            (KeyCode::Up, _) => popup_nav(app, true),
            (KeyCode::Down, _) => popup_nav(app, false),
            (KeyCode::Left, _) => {
                app.cursor = app.cursor.saturating_sub(1);
            }
            (KeyCode::Right, _) => {
                app.cursor = (app.cursor + 1).min(app.input.len());
            }
            (KeyCode::Backspace, _) => {
                if app.cursor > 0 {
                    app.cursor -= 1;
                    app.input.remove(app.cursor);
                    app.update_complete();
                }
            }
            (KeyCode::Delete, _) => {
                if app.cursor < app.input.len() {
                    app.input.remove(app.cursor);
                    app.update_complete();
                }
            }
            (KeyCode::PageUp, _) => {
                app.follow = false;
                app.scroll = app.scroll.saturating_sub(10);
            }
            (KeyCode::PageDown, _) => {
                app.scroll = app.scroll.saturating_add(10);
            }
            (KeyCode::Char(c), m) if !m.contains(KeyModifiers::CONTROL) && !m.contains(KeyModifiers::ALT) => {
                app.input.insert(app.cursor, c);
                app.cursor += 1;
                app.update_complete();
            }
            _ => {}
        }
    }
}

fn popup_open(app: &App) -> bool {
    matches!(app.popup, Some(Popup::Complete { .. } | Popup::Models { .. } | Popup::Sessions { .. }))
}

fn popup_nav(app: &mut App, up: bool) {
    match &mut app.popup {
        Some(Popup::Complete { items, idx }) => {
            *idx = if up { (*idx + items.len() - 1) % items.len() } else { (*idx + 1) % items.len() };
        }
        Some(Popup::Models { providers, pi, models, mi, picking_model }) => {
            if *picking_model && !models.is_empty() {
                *mi = if up { (*mi + models.len() - 1) % models.len() } else { (*mi + 1) % models.len() };
            } else if !providers.is_empty() {
                *pi = if up { (*pi + providers.len() - 1) % providers.len() } else { (*pi + 1) % providers.len() };
            }
        }
        Some(Popup::Sessions { list, idx }) => {
            if !list.is_empty() {
                *idx = if up { (*idx + list.len() - 1) % list.len() } else { (*idx + 1) % list.len() };
            }
        }
        None => {}
    }
}

fn popup_enter(app: &mut App) {
    match app.popup.take() {
        Some(Popup::Complete { .. }) => {
            // restore then accept (accept reads popup, so re-set dance):
            app.update_complete();
            app.accept_complete();
        }
        Some(Popup::Models { providers, pi, models, mi, picking_model }) => {
            if !picking_model {
                if let Some(p) = providers.get(pi) {
                    match app.sdk.models(p) {
                        Ok(m) => {
                            let names: Vec<String> = m.iter().filter_map(|v| v.get("id")?.as_str().map(|s| s.to_string())).collect();
                            app.popup = Some(Popup::Models { providers, pi, models: names, mi: 0, picking_model: true });
                        }
                        Err(e) => app.set_status(&format!("models: {e}")),
                    }
                }
            } else if let (Some(p), Some(m)) = (providers.get(pi), models.get(mi)) {
                match app.sdk.set_router("executor", &format!("{p}/{m}")) {
                    Ok(()) => {
                        app.session.model = format!("{p}/{m}");
                        app.set_status(&format!("executor → {p}/{m}"));
                    }
                    Err(e) => app.set_status(&format!("model: {e}")),
                }
            }
        }
        Some(Popup::Sessions { list, idx }) => {
            if let Some(s) = list.get(idx).cloned() {
                app.attach_session(s);
            }
        }
        None => {}
    }
}

fn draw(f: &mut ratatui::Frame, app: &App) {
    let area = f.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1), Constraint::Length(3), Constraint::Length(1)])
        .split(area);
    // Header: workspace badge, agent toggle, approval pill, model + effort.
    let header = Line::from(vec![
        Span::styled(format!(" {} ", app.workspace), Style::default().bg(Color::DarkGray)),
        Span::raw(" "),
        Span::styled(
            format!(" {} ", app.session.agent),
            Style::default().fg(Color::Black).bg(if app.session.agent == "plan" { Color::Yellow } else { Color::Green }),
        ),
        Span::raw(" "),
        Span::styled(format!(" {} ", app.session.permission_mode), Style::default().fg(Color::Black).bg(Color::Cyan)),
        Span::raw(format!(" {} · {}", app.session.model, app.effort)),
        Span::raw(if app.running { " · working…" } else { "" }),
    ]);
    f.render_widget(Paragraph::new(header), chunks[0]);

    // Messages (wrapped, auto-follow tail).
    let width = chunks[1].width.saturating_sub(2) as usize;
    let mut lines: Vec<Line> = vec![];
    for (role, text) in &app.messages {
        let color = match role.as_str() {
            "user" => Color::Green,
            "assistant" => Color::White,
            _ => Color::DarkGray,
        };
        lines.push(Line::from(Span::styled(format!("{role}"), Style::default().fg(color).add_modifier(Modifier::BOLD))));
        for w in wrap(text, width.max(10)) {
            lines.push(Line::from(w));
        }
        lines.push(Line::from(""));
    }
    let total = lines.len();
    let height = chunks[1].height as usize;
    let start = if app.follow || app.scroll == usize::MAX { total.saturating_sub(height) } else { app.scroll.min(total) };
    let view: Vec<Line> = lines.into_iter().skip(start).take(height).collect();
    f.render_widget(Paragraph::new(view).block(Block::default().borders(Borders::NONE)), chunks[1]);

    // Composer.
    let mut shown: String = app.input.iter().collect();
    shown.insert(app.cursor.min(shown.len()), '▌');
    let composer = Paragraph::new(format!("> {shown}"))
        .block(Block::default().borders(Borders::ALL).title(if app.running { "steer: Enter queues" } else { "prompt (Enter send · / commands)" }));
    f.render_widget(composer, chunks[2]);

    // Status bar.
    let status = if !app.status.is_empty() && app.status_at.elapsed() < Duration::from_secs(8) {
        app.status.clone()
    } else {
        "Tab plan/build · Shift+Tab approval · /help".to_string()
    };
    f.render_widget(Paragraph::new(status), chunks[3]);

    // Popups.
    if let Some(popup) = &app.popup {
        let items: Vec<ListItem> = match popup {
            Popup::Complete { items, idx } => items
                .iter()
                .enumerate()
                .map(|(i, s)| ListItem::new(s.clone()).style(sel(i == *idx)))
                .collect(),
            Popup::Models { providers, pi, models, mi, picking_model } => {
                if *picking_model {
                    models.iter().enumerate().map(|(i, s)| ListItem::new(s.clone()).style(sel(i == *mi))).collect()
                } else {
                    providers.iter().enumerate().map(|(i, s)| ListItem::new(s.clone()).style(sel(i == *pi))).collect()
                }
            }
            Popup::Sessions { list, idx } => list
                .iter()
                .enumerate()
                .map(|(i, s)| ListItem::new(format!("{} {} {}", &s.id[..8.min(s.id.len())], s.agent, s.title)).style(sel(i == *idx)))
                .collect(),
        };
        let r = centered(chunks[1], 60, items.len().min(12) as u16 + 2);
        f.render_widget(Clear, r);
        f.render_widget(List::new(items).block(Block::default().borders(Borders::ALL).title("pick (↑↓ · Enter · Esc)")), r);
    }
    if let Some(p) = &app.perm {
        let labels = ["Allow once", "Allow session", "Deny"];
        let spans: Vec<Span> = labels
            .iter()
            .enumerate()
            .flat_map(|(i, l)| {
                let s = if i == p.choice {
                    Span::styled(format!(" [{l}] "), Style::default().fg(Color::Black).bg(Color::Yellow))
                } else {
                    Span::raw(format!("  {l}  "))
                };
                [s, Span::raw(" ")]
            })
            .collect();
        let r = centered(chunks[1], 70, 7);
        f.render_widget(Clear, r);
        f.render_widget(
            Paragraph::new(vec![
                Line::from(format!("approve: {} {}", p.tool, p.target)),
                Line::from(""),
                Line::from(spans),
                Line::from("←/→ choose · Enter answer · Esc deny"),
            ])
            .block(Block::default().borders(Borders::ALL).title("permission")),
            r,
        );
    }
    if let Some(q) = &app.ques {
        let cur = &q.questions[q.qi.min(q.questions.len().saturating_sub(1))];
        let items: Vec<ListItem> = cur
            .options
            .iter()
            .enumerate()
            .map(|(i, o)| ListItem::new(o.clone()).style(sel(i == q.oi)))
            .collect();
        let r = centered(chunks[1], 70, items.len().min(10) as u16 + 5);
        f.render_widget(Clear, r);
        f.render_widget(
            List::new(items).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!("Q{}/{}: {} (↑↓ · Enter · Esc)", q.qi + 1, q.questions.len(), cur.text)),
            ),
            r,
        );
    }
}

fn sel(on: bool) -> Style {
    if on {
        Style::default().fg(Color::Black).bg(Color::White)
    } else {
        Style::default()
    }
}

fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut out = Vec::new();
    for para in text.split('\n') {
        if para.is_empty() {
            out.push(String::new());
            continue;
        }
        let mut cur = String::new();
        for w in para.split_whitespace() {
            if !cur.is_empty() && cur.len() + 1 + w.len() > width {
                out.push(std::mem::take(&mut cur));
            }
            if !cur.is_empty() {
                cur.push(' ');
            }
            cur.push_str(w);
        }
        out.push(cur);
    }
    out
}

fn centered(area: Rect, w_pct: u16, h: u16) -> Rect {
    let w = area.width * w_pct / 100;
    let x = area.x + area.width.saturating_sub(w) / 2;
    let y = area.y + area.height.saturating_sub(h) / 2;
    Rect { x, y, width: w.min(area.width), height: h.min(area.height) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy() -> App {
        App {
            sdk: Sdk::new("http://127.0.0.1:1", ""),
            session: SessionView {
                id: "s".into(),
                parent_id: String::new(),
                workspace_dir: "/repo".into(),
                agent: "build".into(),
                permission_mode: "ask".into(),
                model: "o/m".into(),
                title: String::new(),
            },
            workspace: "/repo".into(),
            messages: vec![("user".into(), "hi".into()), ("assistant".into(), "hello world".into())],
            input: vec!['h', 'i'],
            cursor: 2,
            scroll: 0,
            follow: true,
            running: true,
            queued: None,
            effort: "auto".into(),
            popup: None,
            perm: Some(PermModal { req_id: "r".into(), tool: "bash".into(), target: "ls".into(), choice: 1 }),
            ques: None,
            status: "ok".into(),
            status_at: Instant::now(),
        }
    }

    #[test]
    fn frame_renders_with_modal() {
        let backend = ratatui::backend::TestBackend::new(100, 30);
        let mut term = Terminal::new(backend).unwrap();
        let app = dummy();
        term.draw(|f| draw(f, &app)).unwrap();
        let buf = term.backend().buffer().clone();
        let text: String = buf.content().iter().map(|c| c.symbol().to_string()).collect();
        assert!(text.contains("permission") && text.contains("working"), "{text}");
    }

    #[test]
    fn completion_triggers() {
        let mut app = dummy();
        app.input = "/mo".chars().collect();
        app.update_complete();
        assert!(matches!(app.popup, Some(Popup::Complete { .. })));
        app.input = "plain text".chars().collect();
        app.update_complete();
        assert!(app.popup.is_none());
    }
}
