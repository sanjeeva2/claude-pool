use std::io::Read;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use claude_sync::apply::{self, LocalState, Paths};
use claude_sync::client::{self, HubClient};
use claude_sync::config::ClientConfig;
use claude_sync::{hub, init, secrets};

#[derive(Parser)]
#[command(
    name = "claude-sync",
    version,
    about = "Live-sync your team's Claude Code setup; everyone keeps their own subscription"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the hub server (one per team).
    Hub {
        #[arg(short, long, default_value = "claude-sync-hub.toml")]
        config: PathBuf,
    },
    /// Save the hub address and your token.
    Login {
        #[arg(long)]
        hub: String,
        /// Your member token (or env:VAR / file:PATH).
        #[arg(long)]
        token: String,
    },
    /// Stay connected and apply team changes the moment they are published.
    Connect,
    /// Apply the latest team setup once.
    Pull {
        #[arg(long)]
        dry_run: bool,
    },
    /// Show exactly what a pull would change, as diffs.
    Diff,
    /// Your sync state, the hub's revision, and who is connected.
    Status,
    /// Publish a team directory (publishers only).
    Publish {
        dir: PathBuf,
        #[arg(short, long, default_value = "update team setup")]
        message: String,
        /// Keep running and re-publish whenever the directory changes.
        #[arg(long)]
        watch: bool,
        #[arg(long)]
        allow_secrets: bool,
    },
    /// List published revisions.
    History,
    /// Re-publish an earlier revision (publishers only).
    Rollback { rev: u64 },
    /// Create a team directory from your own Claude Code setup.
    InitTeam { dir: PathBuf },
    /// Manage your personal secrets (never uploaded).
    Secrets {
        #[command(subcommand)]
        action: SecretAction,
    },
    /// Generate a random member token.
    GenToken,
}

#[derive(Subcommand)]
enum SecretAction {
    /// Set a secret. Reads the value from stdin if not given.
    Set { name: String, value: Option<String> },
    /// List secret names (values are not shown).
    List,
    /// Remove a secret.
    Rm { name: String },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "claude_sync=info,warn".into()),
        )
        .with_target(false)
        .init();
    let cli = Cli::parse();
    let paths = Paths::detect()?;
    match cli.command {
        Command::Hub { config } => {
            let text = std::fs::read_to_string(&config)
                .with_context(|| format!("reading {}", config.display()))?;
            let cfg = hub::HubConfig::from_toml(&text)?;
            let listen = cfg.listen.clone();
            let h = hub::Hub::new(cfg)?;
            let l = tokio::net::TcpListener::bind(&listen)
                .await
                .with_context(|| format!("binding {listen}"))?;
            tracing::info!(%listen, "claude-sync hub running");
            axum::serve(l, hub::app(h)).await?;
        }
        Command::Login { hub, token } => {
            let cfg = ClientConfig { hub, token };
            let mut resolved = cfg.clone();
            resolved.token = claude_sync::config::resolve_secret(&cfg.token)?;
            let head = HubClient::new(&resolved)?.head().await?;
            cfg.save(&paths)?;
            println!(
                "Logged in. Team setup is at revision {}. Run `claude-sync connect` to stay in sync.",
                head.rev
            );
        }
        Command::Connect => {
            let cfg = ClientConfig::load(&paths)?;
            client::connect(paths, cfg, |r| print!("{}", apply::render(r, false))).await?;
        }
        Command::Pull { dry_run } => {
            let c = HubClient::new(&ClientConfig::load(&paths)?)?;
            let r = client::pull(&paths, &c, dry_run).await?;
            if dry_run {
                println!("(dry run: nothing written)");
            }
            print!("{}", apply::render(&r, false));
        }
        Command::Diff => {
            let c = HubClient::new(&ClientConfig::load(&paths)?)?;
            print!(
                "{}",
                apply::render(&client::pull(&paths, &c, true).await?, true)
            );
        }
        Command::Status => {
            let c = HubClient::new(&ClientConfig::load(&paths)?)?;
            let local = LocalState::load(&paths)?;
            let st = c.status().await?;
            let mark = if local.applied_rev == st.rev {
                "up to date"
            } else {
                "behind: run `claude-sync pull`"
            };
            println!(
                "team revision {} | yours {} ({mark})",
                st.rev, local.applied_rev
            );
            println!("managed files: {}", local.managed_files.len());
            println!("\nconnected now ({}):", st.connected.len());
            for p in st.connected {
                let sync = if p.applied_rev == st.rev {
                    "in sync"
                } else {
                    "behind"
                };
                let conf = if p.conflicts > 0 {
                    format!(", {} kept local", p.conflicts)
                } else {
                    String::new()
                };
                println!(
                    "  {:<16} {:<20} rev {:<5} {sync}{conf}",
                    p.member, p.host, p.applied_rev
                );
            }
        }
        Command::Publish {
            dir,
            message,
            watch,
            allow_secrets,
        } => {
            let c = HubClient::new(&ClientConfig::load(&paths)?)?;
            if watch {
                tracing::info!(dir = %dir.display(), "watching for changes");
                client::watch_publish(&c, &dir, allow_secrets).await?;
            } else {
                let r = c.publish_dir(&dir, &message, allow_secrets).await?;
                if r.unchanged {
                    println!("Nothing changed (revision {}).", r.rev);
                } else {
                    println!(
                        "Published revision {}. Connected teammates receive it now.",
                        r.rev
                    );
                }
            }
        }
        Command::History => {
            let c = HubClient::new(&ClientConfig::load(&paths)?)?;
            for r in c.revs().await? {
                println!(
                    "{:>5}  {}  {:<14} {:>4} files  {}",
                    r.rev,
                    r.time.format("%Y-%m-%d %H:%M"),
                    r.author,
                    r.files,
                    r.message
                );
            }
        }
        Command::Rollback { rev } => {
            let c = HubClient::new(&ClientConfig::load(&paths)?)?;
            c.rev(rev).await?;
            let r = c.rollback(rev).await?;
            println!("Revision {rev} re-published as revision {}.", r.rev);
        }
        Command::InitTeam { dir } => {
            let r = init::init_team(&paths, &dir)?;
            println!("Created {} with {} file(s):", dir.display(), r.files.len());
            for f in &r.files {
                println!("  {f}");
            }
            if !r.secrets.is_empty() {
                println!(
                    "\nReplaced credentials with placeholders (your values were saved to your local secret store):"
                );
                for s in &r.secrets {
                    println!("  {{{{secret:{s}}}}}");
                }
                println!("Teammates set their own with `claude-sync secrets set NAME`.");
            }
            println!(
                "\nReview the directory (it can be a git repo), then: claude-sync publish {}",
                dir.display()
            );
        }
        Command::Secrets { action } => {
            let file = paths.secrets_file();
            let mut s = secrets::load(&file)?;
            match action {
                SecretAction::Set { name, value } => {
                    if !secrets::valid_name(&name) {
                        bail!("secret names use letters, digits and _ only");
                    }
                    let value = match value {
                        Some(v) => v,
                        None => {
                            let mut v = String::new();
                            std::io::stdin().read_to_string(&mut v)?;
                            v.trim_end_matches(['\n', '\r']).to_string()
                        }
                    };
                    s.insert(name.clone(), value);
                    secrets::save(&file, &s)?;
                    println!(
                        "Saved `{name}`. Run `claude-sync pull` (or keep `connect` running) to use it."
                    );
                }
                SecretAction::List => {
                    for k in s.keys() {
                        println!("{k}");
                    }
                }
                SecretAction::Rm { name } => {
                    s.remove(&name);
                    secrets::save(&file, &s)?;
                }
            }
        }
        Command::GenToken => {
            use rand::RngCore;
            let mut b = [0u8; 24];
            rand::thread_rng().fill_bytes(&mut b);
            println!(
                "cst-{}",
                b.iter().map(|x| format!("{x:02x}")).collect::<String>()
            );
        }
    }
    Ok(())
}
