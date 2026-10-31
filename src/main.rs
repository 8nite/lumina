//! Lumina CLI (Rust port, M1).
//! Parity target: `cmd/lumina/main.go` — TUI, run, serve, web, attach,
//! session, plugin, kb, evolve, models, auth, pair, openapi.
//!
//! M1 implements: `serve` (axum + sessions/prompt/export), `run`, `session`,
//! `models --provider`, `openapi --out`. TUI (`M3`), web auth/pair (`M4`),
//! plugins/kb/evolve (`M2`/`M5`) exit 66 until their milestone.

mod agent;
mod approve;
mod config;
mod kb;
mod llm;
mod rules;
mod sdk;
mod security;
mod server;
mod store;
mod tools;
mod tui;

use agent::Runner;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

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
        #[arg(long, default_value_t = 7432)]
        port: u16,
    },
    Run {
        #[arg(allow_hyphen_values = true)]
        prompt: Vec<String>,
        #[arg(last = true)]
        rest: Vec<String>,
    },
    Web {},
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
            let base = std::env::var("LUMINA_URL").unwrap_or_else(|_| "http://127.0.0.1:7432".into());
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
        Some(Cmd::Serve { host, port }) => {
            if !server::is_loopback(&host) && std::env::var("LUMINA_TOKEN").unwrap_or_default().is_empty() {
                eprintln!("refusing non-loopback bind without LUMINA_TOKEN");
                std::process::exit(77);
            }
            let store = store::Store::open(None).map_err(|e| format!("store: {e}"))?;
            let router = llm::Router::new(&cfg).map_err(|e| format!("router: {e}"))?;
            let compat = (cfg.compat.import_global_rules, cfg.compat.import_global_skills);
            let app = server::router(server::AppState::new(store, router, compat));
            let addr = format!("{host}:{port}");
            tracing::info!(%addr, agent = %cfg.defaults.agent, "lumina serve (rust M1)");
            let listener = tokio::net::TcpListener::bind(&addr).await?;
            axum::serve(listener, app).await?;
            Ok(())
        }
        Some(Cmd::Run { mut prompt, rest }) => {
            prompt.extend(rest);
            let text = prompt.join(" ");
            if text.trim().is_empty() {
                eprintln!("usage: lumina run \"<prompt>\"");
                std::process::exit(64);
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
            let runner = Runner::new();
            let compat = (cfg.compat.import_global_rules, cfg.compat.import_global_skills);
            let answer = runner
                .run_prompt(&store, &router, &sess.id, &text, &llm::normalize_effort(&effort), compat, |_, _| {})
                .await
                .map_err(|e| format!("{e}"))?;
            if dflt_format == "json" {
                println!("{}", serde_json::json!({"session": sess.id, "text": answer}));
            } else {
                println!("{answer}");
            }
            Ok(())
        }
        Some(Cmd::Session { args }) => cmd_session(&args),
        Some(Cmd::Kb { args }) => cmd_kb(&args),
        Some(Cmd::Models { provider }) => {
            let router = llm::Router::new(&cfg).map_err(|e| format!("router: {e}"))?;
            let names: Vec<String> = match provider {
                Some(p) => router.providers.keys().filter(|k| *k == &p).cloned().collect(),
                None => router.providers.keys().cloned().collect(),
            };
            if names.is_empty() {
                eprintln!("unknown provider");
                std::process::exit(64);
            }
            // M1: list configured provider keys (live /models enumeration lands with TUI pickers in M3).
            for n in names {
                println!("{n}");
            }
            Ok(())
        }
        Some(Cmd::Openapi { out }) => {
            let spec = serde_json::json!({"openapi": "3.1.0", "info": {"title": "Lumina daemon", "version": env!("CARGO_PKG_VERSION")}});
            let text = serde_json::to_string_pretty(&spec)?;
            if let Some(p) = out {
                std::fs::write(&p, text)?;
                println!("{}", p.display());
            } else {
                println!("{text}");
            }
            Ok(())
        }
        Some(_) => {
            eprintln!("not yet ported in Rust M1 (see plan.md milestones); Go binary still owns this subcommand");
            std::process::exit(66);
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
        _ => {
            eprintln!("usage: lumina session [list [--all]|show <id>|fork <id> [turn]|export <id> [--sanitize]]");
            std::process::exit(64);
        }
    }
}

type MainResult = Result<(), Box<dyn std::error::Error>>;

/// Split "http://host:port" without a URL crate (localhost tooling only).
fn split_base(base: &str) -> (String, u16) {
    let s = base.trim_end_matches('/');
    let s = s.strip_prefix("http://").or_else(|| s.strip_prefix("https://")).unwrap_or(s);
    match s.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse().unwrap_or(7432)),
        None => (s.to_string(), 7432),
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
    let router = llm::Router::new(cfg).map_err(|e| format!("router: {e}"))?;
    let compat = (cfg.compat.import_global_rules, cfg.compat.import_global_skills);
    let app = server::router(server::AppState::new(store, router, compat));
    let listener = tokio::net::TcpListener::bind((host.as_str(), port)).await?;
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    for _ in 0..50 {
        if tokio::net::TcpStream::connect((host.as_str(), port)).await.is_ok() {
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    Err("daemon failed to start".into())
}
