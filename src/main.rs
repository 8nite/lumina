//! Lumina CLI (Rust port, M0).
//! Parity target: `cmd/lumina/main.go` — TUI, run, serve, web, attach,
//! session, plugin, kb, evolve, models, auth, pair, openapi.
//!
//! M0 implements: `serve` (real axum bind), `openapi --out`, `--help` surface.
//! Everything else exits 66 (EX_NOINPUT-style "not yet ported") until its milestone.

mod config;
mod rules;
mod security;
mod server;

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
        prompt: Vec<String>,
    },
    Web {},
    Attach {
        url: String,
    },
    Session {
        args: Vec<String>,
    },
    Plugin {
        args: Vec<String>,
    },
    Kb {
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

#[tokio::main]
async fn main() -> MainResult {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let cli = Cli::parse();
    let cfg = config::Config::load(None).unwrap_or_default();
    match cli.cmd {
        None => {
            eprintln!("TUI lands in M3 (ratatui). Daemon API is up via `lumina serve`.");
            std::process::exit(66);
        }
        Some(Cmd::Serve { host, port }) => {
            if !server::is_loopback(&host) && std::env::var("LUMINA_TOKEN").unwrap_or_default().is_empty() {
                eprintln!("refusing non-loopback bind without LUMINA_TOKEN");
                std::process::exit(77);
            }
            let app = server::router(server::DaemonOpts {
                auth_token: std::env::var("LUMINA_TOKEN").unwrap_or_default(),
            });
            let addr = format!("{host}:{port}");
            tracing::info!(%addr, agent = %cfg.defaults.agent, "lumina serve (rust M0)");
            let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
            axum::serve(listener, app).await.unwrap();
            Ok(())
        }
        Some(Cmd::Openapi { out }) => {
            // Reuse the same spec shape as the live route (single source of truth lives in server.rs M1).
            let spec = serde_json::json!({"openapi": "3.1.0", "info": {"title": "Lumina daemon", "version": env!("CARGO_PKG_VERSION")}});
            let text = serde_json::to_string_pretty(&spec).unwrap();
            if let Some(p) = out {
                std::fs::write(&p, text).unwrap();
                println!("{}", p.display());
            } else {
                println!("{text}");
            }
            Ok(())
        }
        Some(_) => {
            eprintln!("not yet ported in Rust M0 (see plan.md milestones); Go binary still owns this subcommand");
            std::process::exit(66);
        }
    }
}

type MainResult = Result<(), Box<dyn std::error::Error>>;
