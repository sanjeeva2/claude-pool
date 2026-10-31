use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use claude_pool::{AppState, alerts, app, config::Config, report, spawn_snapshotter};

#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[derive(Parser)]
#[command(
    name = "claude-pool",
    version,
    about = "Pool several Claude API accounts behind one fast endpoint for your team"
)]
struct Cli {
    /// Path to the TOML config file.
    #[arg(
        short,
        long,
        global = true,
        env = "CLAUDE_POOL_CONFIG",
        default_value = "claude-pool.toml"
    )]
    config: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the gateway.
    Serve,
    /// Validate the config (resolves env:/file: secrets) and exit.
    Check,
    /// Combined usage and cost across accounts, from the Admin API (needs `admin_key`).
    Usage {
        #[arg(long, default_value_t = 7)]
        days: u32,
        /// Print JSON instead of a table.
        #[arg(long)]
        json: bool,
    },
    /// Print live stats from a running gateway.
    Stats {
        #[arg(long, default_value = "http://127.0.0.1:8787")]
        url: String,
        /// Admin token, if `server.admin_token` is set.
        #[arg(long, env = "POOL_ADMIN_TOKEN")]
        token: Option<String>,
    },
    /// Generate a random client token for a team member.
    GenToken,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "claude_pool=info,warn".into()),
        )
        .init();
    let cli = Cli::parse();
    match cli.command {
        Command::Serve => serve(Config::load(&cli.config)?).await,
        Command::Check => {
            let cfg = Config::load(&cli.config)?;
            println!(
                "config OK: {} account(s), {} client(s), listen {}",
                cfg.accounts.len(),
                cfg.clients.len(),
                cfg.server.listen
            );
            Ok(())
        }
        Command::Usage { days, json } => {
            let cfg = Config::load(&cli.config)?;
            let reports = report::collect(&cfg, days).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&reports)?);
            } else {
                print!("{}", report::render(&reports));
            }
            Ok(())
        }
        Command::Stats { url, token } => {
            let mut req =
                reqwest::Client::new().get(format!("{}/pool/stats", url.trim_end_matches('/')));
            if let Some(t) = token {
                req = req.bearer_auth(t);
            }
            let v: serde_json::Value = req.send().await?.error_for_status()?.json().await?;
            println!("{}", serde_json::to_string_pretty(&v)?);
            Ok(())
        }
        Command::GenToken => {
            use rand::RngCore;
            let mut b = [0u8; 24];
            rand::thread_rng().fill_bytes(&mut b);
            let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
            println!("cpk-{hex}");
            Ok(())
        }
    }
}

async fn serve(cfg: Config) -> Result<()> {
    let listen = cfg.server.listen.clone();
    if cfg.clients.is_empty() {
        tracing::warn!(
            "no [[clients]] configured: the gateway is OPEN to anyone who can reach {listen}"
        );
    }
    let state = AppState::new(cfg)?;
    spawn_snapshotter(state.clone());
    alerts::spawn(state.clone());

    let listener = tokio::net::TcpListener::bind(&listen)
        .await
        .with_context(|| format!("binding {listen}"))?;
    // Disable Nagle so small SSE events reach clients immediately.
    let listener = axum::serve::ListenerExt::tap_io(listener, |tcp| {
        let _ = tcp.set_nodelay(true);
    });
    tracing::info!(%listen, accounts = state.accounts.len(), "claude-pool listening");
    axum::serve(listener, app(state.clone()))
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    tracing::info!("shutting down; saving state");
    state.save_state();
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! { _ = ctrl_c => {}, _ = term => {} }
}
