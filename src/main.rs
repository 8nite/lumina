//! Lumina CLI: TUI, run, serve, web, attach, session, plugin, kb, evolve,
//! models, auth, pair, openapi, vault.

mod agent;
mod approve;
mod begin_patch;
mod checkpoints;
mod config;
mod copilot;
mod harness;
mod kb;
mod llm;
mod mcp;
mod plugins;
mod pty;
mod rules;
mod sdk;
mod security;
mod server;
mod store;
mod tools;
mod tui;
mod vault;
mod websearch;

use agent::Runner;
use clap::{Parser, Subcommand};
use std::{path::PathBuf, sync::Arc};

#[derive(Parser)]
#[command(name = "lumina", version, about = "Local-first AI coding harness (Rust port)")]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
    /// Resume latest session for cwd (-c). M1.
    #[arg(short = 'c', long = "continue", global = true)]
    cont: bool,
    /// Resume session by id (-s). M1.
    #[arg(short = 's', long = "session", global = true)]
    session: Option<String>,
    #[arg(long = "agent", global = true)]
    agent: Option<String>,
    #[arg(long = "plan", global = true)]
    plan: bool,
    #[arg(long = "auto-edit", global = true)]
    auto_edit: bool,
    #[arg(short = 'y', long = "auto-approve", global = true)]
    auto_approve: bool,
    #[arg(long = "bypass", global = true)]
    bypass: bool,
    #[arg(short = 'm', long = "model", global = true)]
    model: Option<String>,
    #[arg(long = "format", global = true)]
    format: Option<String>,
    #[arg(long = "effort", global = true)]
    effort: Option<String>,
}

#[derive(Subcommand)]
enum Cmd {
    Serve {
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
        #[arg(long, default_value_t = 4089)]
        port: u16,
        /// Accepted for CLI parity (informational: loopback allowed by default).
        #[arg(long, default_value = "*")]
        cors: String,
    },
    Run {
        #[arg(allow_hyphen_values = true)]
        prompt: Vec<String>,
        /// Attach image files (repeatable, 8MB cap each; vision-capable models only).
        #[arg(long = "attach")]
        attach: Vec<PathBuf>,
        #[arg(last = true)]
        rest: Vec<String>,
    },
    Web {},
    Pair {
        #[arg(long)]
        lan: bool,
    },
    #[command(alias = "device")]
    Auth {
        #[arg(long)]
        provider: String,
        #[arg(long)]
        github_url: Option<String>,
    },
    Attach {
        url: String,
    },
    Session {
        #[arg(allow_hyphen_values = true)]
        args: Vec<String>,
    },
    Plugin {
        #[arg(allow_hyphen_values = true)]
        args: Vec<String>,
    },
    Kb {
        #[arg(allow_hyphen_values = true)]
        args: Vec<String>,
    },
    Evolve {
        session: Option<String>,
    },
    Models {
        #[arg(long)]
        provider: Option<String>,
    },
    Openapi {
        #[arg(long)]
        out: Option<PathBuf>,
    },
    Vault {
        #[arg(allow_hyphen_values = true)]
        args: Vec<String>,
    },
}

fn cwd() -> String {
    std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| ".".into())
}

fn permission_mode(cli: &Cli, cfg: &config::Config) -> String {
    if cli.bypass {
        "bypass".into()
    } else if cli.auto_approve {
        "auto-approve".into()
    } else if cli.auto_edit {
        "auto-edit".into()
    } else {
        cfg.defaults.permission_mode.clone()
    }
}

fn agent_name(cli: &Cli, cfg: &config::Config) -> String {
    if cli.plan {
        return "plan".into();
    }
    cli.agent.clone().unwrap_or_else(|| cfg.defaults.agent.clone())
}

#[tokio::main]
async fn main() -> MainResult {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let cli = Cli::parse();
    let cfg = config::Config::load(None).unwrap_or_default();
    // Extract borrow-dependent values before `cmd` is moved out of `cli`.
    let dflt_agent = agent_name(&cli, &cfg);
    let dflt_mode = permission_mode(&cli, &cfg);
    let dflt_model = cli.model.clone().unwrap_or_default();
    let dflt_effort = cli.effort.clone().unwrap_or_default();
    let dflt_format = cli.format.clone().unwrap_or_default();
    let dflt_cont = cli.cont;
    let dflt_session = cli.session.clone();
    let dflt_plan = cli.plan;
    match cli.cmd {
        None => {
            // Terminal TUI: ensure the workspace daemon is up, then attach.
            // Honors -c/-s/--agent/--mode/--plan like the Go client.
            let base = std::env::var("LUMINA_URL").unwrap_or_else(|_| "http://127.0.0.1:4089".into());
            ensure_daemon(&base, &cfg).await?;
            tokio::task::block_in_place(|| {
                tui::run(tui::LaunchOpts {
                    base_url: base,
                    token: std::env::var("LUMINA_TOKEN").unwrap_or_default(),
                    cont: dflt_cont,
                    session: dflt_session,
                    agent: Some(if dflt_plan { "plan".into() } else { dflt_agent.clone() }),
                    mode: Some(dflt_mode.clone()),
                })
            })
            .map_err(|e| format!("tui: {e}"))?;
            Ok(())
        }
        Some(Cmd::Attach { url }) => {
            tokio::task::block_in_place(|| {
                tui::run(tui::LaunchOpts {
                    base_url: url,
                    token: std::env::var("LUMINA_TOKEN").unwrap_or_default(),
                    cont: false,
                    session: None,
                    agent: None,
                    mode: None,
                })
            })
            .map_err(|e| format!("tui: {e}"))?;
            Ok(())
        }
        Some(Cmd::Web {}) => {
            let base = std::env::var("LUMINA_URL").unwrap_or_else(|_| "http://127.0.0.1:4089".into());
            ensure_daemon(&base, &cfg).await?;
            println!("{base}");
            // Best-effort browser open; the URL above is the contract.
            let _ = std::process::Command::new("xdg-open").arg(&base).output();
            Ok(())
        }
        Some(Cmd::Pair { lan: _ }) => {
            let mut token = std::env::var("LUMINA_TOKEN").unwrap_or_default();
            if token.is_empty() {
                token = uuid::Uuid::new_v4().to_string();
                println!("LUMINA_TOKEN not set; generated one-time token (export it before serving):");
                println!("  export LUMINA_TOKEN={token}");
            }
            let short = if token.len() > 6 { &token[..6] } else { &token };
            println!("attach from another host on your LAN/Tailscale network with:");
            println!("  lumina attach http://<this-host>:4089   (LUMINA_TOKEN={short}…)");
            Ok(())
        }
        Some(Cmd::Auth { provider, github_url }) => {
            if provider != "copilot" {
                eprintln!("auth device is only implemented for --provider copilot");
                std::process::exit(64);
            }
            let token_url = github_url.unwrap_or_else(|| "https://api.github.com".into());
            let dc = copilot::start_device_flow(&token_url, "")
                .await
                .map_err(|e| format!("{e}"))?;
            println!("open {} and enter code {}", dc.verification_uri, dc.user_code);
            let interval = dc.interval.max(5);
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(interval)).await;
                match copilot::poll_device_flow(&token_url, &dc.device_code).await {
                    Ok(copilot::Poll::Token(t)) => {
                        copilot::write_token_store(&provider, &t).map_err(|e| format!("{e}"))?;
                        println!("connected (token stored 0600, never printed)");
                        return Ok(());
                    }
                    Ok(copilot::Poll::Pending) => continue,
                    Err(e) => return Err(format!("{e}").into()),
                }
            }
        }
        Some(Cmd::Serve { host, port, cors: _ }) => {
            if !server::is_loopback(&host) && std::env::var("LUMINA_TOKEN").unwrap_or_default().is_empty() {
                eprintln!("refusing non-loopback bind without LUMINA_TOKEN");
                std::process::exit(77);
            }
            // Startup sweep: drop stale per-session TMPDIRs from dead daemons.
            if let Ok(rd) = std::fs::read_dir(std::env::temp_dir()) {
                for e in rd.flatten() {
                    if e.file_name().to_string_lossy().starts_with("lumina-ses-") {
                        let _ = std::fs::remove_dir_all(e.path());
                    }
                }
            }
            let store = store::Store::open(None).map_err(|e| format!("store: {e}"))?;
            // Crash recovery: a fresh daemon owns no live turns.
            let _ = store.clear_all_claims();
            let router = llm::Router::new(&cfg).map_err(|e| format!("router: {e}"))?;
            let app = server::router(server::AppState::new(store, router, cfg.clone(), config_path()));
            let addr = format!("{host}:{port}");
            // The store path is cwd-relative by default, so a restart from
            // another directory silently switches databases (sessions vanish
            // from every attached client). Print the absolute path so a wrong
            // cwd is visible immediately instead of surfacing as 404s.
            // (eprintln: startup banners must show without RUST_LOG set.)
            let db_display = std::env::var("LUMINA_DB").unwrap_or_else(|_| {
                std::env::current_dir()
                    .unwrap_or_else(|_| ".".into())
                    .join(".lumina/lumina.db")
                    .display()
                    .to_string()
            });
            eprintln!("lumina serve (rust) on {addr} (db {db_display})");
            tracing::info!(%addr, agent = %cfg.defaults.agent, db = %db_display, "lumina serve (rust)");
            let listener = tokio::net::TcpListener::bind(&addr).await?;
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .await?;
            Ok(())
        }
        Some(Cmd::Run { mut prompt, attach, rest }) => {
            prompt.extend(rest);
            let text = prompt.join(" ");
            if text.trim().is_empty() {
                eprintln!("usage: lumina run \"<prompt>\"");
                std::process::exit(64);
            }
            let mut images = Vec::new();
            for p in &attach {
                let raw = std::fs::read(p).map_err(|e| format!("attach {}: {e}", p.display()))?;
                if raw.len() > 8 * 1024 * 1024 {
                    eprintln!("attach {}: over 8MB cap", p.display());
                    std::process::exit(64);
                }
                let mt = match p.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase().as_str() {
                    "jpg" | "jpeg" => "image/jpeg",
                    "gif" => "image/gif",
                    "webp" => "image/webp",
                    _ => "image/png",
                };
                images.push(llm::ImageAttachment {
                    name: p.display().to_string(),
                    media_type: mt.into(),
                    data: base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &raw),
                });
            }
            let store = store::Store::open(None).map_err(|e| format!("store: {e}"))?;
            let router = llm::Router::new(&cfg).map_err(|e| format!("router: {e}"))?;
            let ws = cwd();
            let sess = if let Some(id) = &dflt_session {
                store.get_session(id).map_err(|e| format!("store: {e}"))?
                    .ok_or_else(|| format!("session {id} not found"))?
            } else if dflt_cont {
                store.latest_for_dir(&ws).map_err(|e| format!("store: {e}"))?
                    .ok_or_else(|| "no session for this directory (omit -c to start one)".to_string())?
            } else {
                store
                    .create_session(&ws, &dflt_agent, &dflt_mode, &dflt_model)
                    .map_err(|e| format!("store: {e}"))?
            };
            let effort = dflt_effort.clone();
            if !llm::valid_effort(&effort) {
                eprintln!("unknown --effort {effort:?} (auto|low|medium|high|xhigh|max)");
                std::process::exit(64);
            }
            let mut runner = Runner::new();
            {
                let files = plugins::discover(std::path::Path::new(&sess.workspace_dir), false);
                if !files.is_empty() {
                    runner.plugins = Some(Arc::new(plugins::Host::new(files)));
                }
            }
            crate::tools::set_background_hook(std::sync::Arc::new(
                |_session: &str, id: &str, kind: &str, _label: &str| {
                    eprintln!("\u{203a} background done: {id} ({kind})");
                },
            ));
            // Headless progress log (Go parity): stages on stderr, only the
            // final answer on stdout.
            let user_turn = runner
                .begin_turn(&store, &sess.id, &text)
                .map_err(|e| format!("store: {e}"))?;
            let outcome = runner
                .run_prompt(&store, &router, &cfg, &sess.id, &user_turn, llm::normalize_effort(&effort), &images,
                    |d, t| {
                        let preview = if d.is_empty() { t } else { d };
                        let one: String = preview.chars().take(120).collect();
                        if !one.trim().is_empty() {
                            eprintln!("\u{2192} {}", one.trim());
                        }
                    },
                    |s| eprintln!("\u{203a} {s}"),
                    |tool| match tool {
                        crate::agent::ToolEvent::Start { name, target } => {
                            let t = if target.is_empty() { name.clone() } else { format!("{name} {target}") };
                            eprintln!("\u{25b8} {t}");
                        }
                        crate::agent::ToolEvent::Done { preview, error, .. } => {
                            let mark = if error { "error: " } else { "" };
                            let one: String = preview.chars().take(160).collect();
                            if !one.trim().is_empty() {
                                eprintln!("\u{2192} {mark}{}", one.trim());
                            }
                        }
                    })
                .await
                .map_err(|e| format!("{e}"))?;
            eprintln!("\u{2713} done");
            let answer = outcome.text;
            if dflt_format == "json" {
                println!("{}", serde_json::json!({"session_id": sess.id, "text": answer}));
            } else {
                println!("{answer}");
            }
            Ok(())
        }
        Some(Cmd::Session { args }) => cmd_session(&args),
        Some(Cmd::Kb { args }) => cmd_kb(&args),
        Some(Cmd::Models { provider }) => {
            let router = llm::Router::new(&cfg).map_err(|e| format!("router: {e}"))?;
            match provider {
                Some(p) => {
                    for m in router.list_models(&p).await.map_err(|e| format!("{e}"))? {
                        if m.context_window > 0 {
                            println!("{}  (ctx {})", m.id, m.context_window);
                        } else {
                            println!("{}", m.id);
                        }
                    }
                }
                None => {
                    let mut names: Vec<&String> = router.providers.keys().collect();
                    names.sort();
                    for n in names {
                        println!("{n}");
                    }
                }
            }
            Ok(())
        }
        Some(Cmd::Openapi { out }) => {
            let text = serde_json::to_string_pretty(&server::openapi_spec())?;
            if let Some(p) = out {
                std::fs::write(&p, text)?;
                println!("{}", p.display());
            } else {
                println!("{text}");
            }
            Ok(())
        }
        Some(Cmd::Plugin { args }) => cmd_plugin(&args),
        Some(Cmd::Vault { args }) => cmd_vault(&args),
        Some(Cmd::Evolve { session: _ }) => {
            let ws = std::env::current_dir().unwrap_or_else(|_| ".".into());
            println!("{}", harness::offline_pass(&ws).map_err(|e| format!("{e}"))?);
            Ok(())
        }
    }
}

fn cmd_plugin(args: &[String]) -> MainResult {
    let ws = cwd();
    let wpath = PathBuf::from(&ws);
    let files = plugins::discover(&wpath, true);
    match args.first().map(|s| s.as_str()) {
        Some("list") | None => {
            if files.is_empty() {
                println!("(no plugins configured)");
            }
            for f in &files {
                println!("{}", f.display());
            }
            Ok(())
        }
        Some("doctor") => {
            let node = std::process::Command::new("node").arg("--version").output();
            match node {
                Ok(o) if o.status.success() => {
                    println!("node {}", String::from_utf8_lossy(&o.stdout).trim());
                }
                _ => {
                    println!("node v22 NOT found (plugin worker disabled)");
                    return Ok(());
                }
            }
            if files.is_empty() {
                println!("no plugin files discovered");
                return Ok(());
            }
            let host = plugins::Host::new(files);
            let t = std::time::Instant::now();
            match host.dispatch("config", &serde_json::json!({}), std::time::Duration::from_secs(15)) {
                Some(_) => {
                    println!("worker ok ({:?})", t.elapsed());
                    for p in host.loaded() {
                        println!("  {p}");
                    }
                }
                None => println!("worker FAILED to answer config hook"),
            }
            Ok(())
        }
        Some("enable") | Some("disable") => {
            println!("plugin {}: managed by editing ~/.config/lumina/plugins (no registry yet)", args[0]);
            Ok(())
        }
        _ => {
            eprintln!("usage: lumina plugin [list|doctor]");
            std::process::exit(64);
        }
    }
}

fn cmd_kb(args: &[String]) -> MainResult {
    let ws = cwd();
    let wpath = PathBuf::from(&ws);
    match args.first().map(|s| s.as_str()) {
        Some("query") => {
            let q = args.get(1).map(|s| s.as_str()).unwrap_or("");
            print!("{}", kb::query(&wpath, q, 4000).map_err(|e| format!("{e}"))?);
            Ok(())
        }
        Some("update") => {
            let (Some(topic), Some(note)) = (args.get(1), args.get(2)) else {
                eprintln!("usage: lumina kb update <topic> \"<note>\"");
                std::process::exit(64);
            };
            println!("{}", kb::memory_write(&wpath, topic, note).map_err(|e| format!("{e}"))?);
            Ok(())
        }
        Some("reflect") => {
            print!("{}", kb::reflect(&wpath));
            Ok(())
        }
        Some("status") => {
            for rel in ["memory/graphify-out/graph.json", "graphify-out/graph.json"] {
                let p = wpath.join(rel);
                if p.exists() {
                    let g = kb::Graph::load(&p).map_err(|e| format!("{e}"))?;
                    println!("{rel}: {} nodes, {} edges", g.nodes.len(), g.edges.len());
                } else {
                    println!("{rel}: missing");
                }
            }
            Ok(())
        }
        _ => {
            eprintln!("usage: lumina kb [query \"<q>\"|update <topic> \"<note>\"|reflect|status]");
            std::process::exit(64);
        }
    }
}

fn cmd_session(args: &[String]) -> MainResult {
    let store = store::Store::open(None).map_err(|e| format!("store: {e}"))?;
    match args.first().map(|s| s.as_str()) {
        Some("list") => {
            let all = args.iter().any(|a| a == "--all");
            let ws = cwd();
            let sessions = store
                .list_for_dir(if all { None } else { Some(ws.as_str()) })
                .map_err(|e| format!("store: {e}"))?;
            for s in sessions {
                println!("{}  {}  {}  {}", s.id, s.agent, s.permission_mode, s.title);
            }
            Ok(())
        }
        Some("show") => {
            let Some(id) = args.get(1) else { eprintln!("usage: lumina session show <id>"); std::process::exit(64); };
            if store.get_session(id).map_err(|e| format!("store: {e}"))?.is_none() {
                eprintln!("session {id} not found");
                std::process::exit(64);
            }
            for t in store.turns(id).map_err(|e| format!("store: {e}"))? {
                println!("## {}\n\n{}\n", t.role, t.text);
            }
            Ok(())
        }
        Some("fork") => {
            let Some(id) = args.get(1) else { eprintln!("usage: lumina session fork <id> [turn]"); std::process::exit(64); };
            let at = args.get(2).map(|s| s.as_str());
            let child = store.fork(id, at).map_err(|e| format!("store: {e}"))?;
            println!("{}", child.id);
            Ok(())
        }
        Some("export") => {
            let Some(id) = args.get(1) else { eprintln!("usage: lumina session export <id> [--sanitize]"); std::process::exit(64); };
            let sanitize = args.iter().any(|a| a == "--sanitize");
            print!("{}", store.export_markdown(id, sanitize).map_err(|e| format!("store: {e}"))?);
            Ok(())
        }
        Some("delete") => {
            let Some(id) = args.get(1) else { eprintln!("usage: lumina session delete <id>"); std::process::exit(64); };
            if store.delete_session(id).map_err(|e| format!("store: {e}"))? {
                let _ = std::fs::remove_dir_all(std::env::temp_dir().join(format!("lumina-ses-{id}")));
                println!("{id}");
                Ok(())
            } else {
                eprintln!("session {id} not found");
                std::process::exit(64);
            }
        }
        _ => {
            eprintln!("usage: lumina session [list [--all]|show <id>|fork <id> [turn]|export <id> [--sanitize]|delete <id>]");
            std::process::exit(64);
        }
    }
}

fn cmd_vault(args: &[String]) -> MainResult {
    // `lumina vault check "<text>"`: what the LLM would receive for text —
    // secret-shaped spans replaced with {placeholder_xxx} refs, everything
    // else byte-identical. Throwaway in-memory vault, never the live store.
    if args.first().map(|s| s.as_str()) != Some("check") || args.len() < 2 {
        eprintln!("usage: lumina vault check \"<text>\"");
        std::process::exit(64);
    }
    let store = store::Store::open(Some(":memory:")).map_err(|e| format!("store: {e}"))?;
    let sess = store.create_session("", "build", "ask", "").map_err(|e| format!("store: {e}"))?;
    let input = args[1..].join(" ");
    let out = store.vault.substitute_for_llm(&sess.id, &input).map_err(|e| format!("vault: {e}"))?;
    println!("user side: {input}\nllm sees:  {out}");
    if out == input {
        println!("verdict:   UNCHANGED — no detector matched (see vault patterns)");
    } else {
        println!("verdict:   VAULTED — values hidden from the model, remapped locally on return");
    }
    Ok(())
}

type MainResult = Result<(), Box<dyn std::error::Error>>;

/// Split "http://host:port" without a URL crate (localhost tooling only).
fn split_base(base: &str) -> (String, u16) {
    let s = base.trim_end_matches('/');
    let s = s.strip_prefix("http://").or_else(|| s.strip_prefix("https://")).unwrap_or(s);
    match s.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse().unwrap_or(4089)),
        None => (s.to_string(), 4089),
    }
}

/// Ensure a daemon answers at `base`; if the loopback port is closed, boot
/// an in-process server (mirrors `lumina` auto-start) and wait for readiness.
async fn ensure_daemon(base: &str, cfg: &config::Config) -> MainResult {
    let (host, port) = split_base(base);
    if tokio::net::TcpStream::connect((host.as_str(), port)).await.is_ok() {
        return Ok(());
    }
    if !server::is_loopback(&host) {
        return Err(format!("no daemon at {base} (set LUMINA_TOKEN for remote attach)").into());
    }
    let store = store::Store::open(None).map_err(|e| format!("store: {e}"))?;
    // Crash recovery: a fresh daemon owns no live turns.
    let _ = store.clear_all_claims();
    let router = llm::Router::new(cfg).map_err(|e| format!("router: {e}"))?;
    let app = server::router(server::AppState::new(store, router, cfg.clone(), config_path()));
    let listener = tokio::net::TcpListener::bind((host.as_str(), port)).await?;
    tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await;
    });
    for _ in 0..50 {
        if tokio::net::TcpStream::connect((host.as_str(), port)).await.is_ok() {
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    Err("daemon failed to start".into())
}

/// Config file path for model-persist (explicit env wins, else the default
/// path when it exists; None keeps overrides in-memory only).
fn config_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("LUMINA_CONFIG") {
        return Some(PathBuf::from(p));
    }
    config::default_path().filter(|p| p.exists())
}
