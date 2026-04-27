use clap::{Parser, Subcommand};
use clawops::auth::WxClient;
use clawops::config::Config;
use clawops::http::AppState;
use clawops::provisioner::Provisioner;
use clawops::limits::AppLimiters;
use clawops::reaper::Reaper;
use clawops::{db, http, process, users};
use std::path::PathBuf;
use std::sync::Arc;
use tracing_subscriber::{fmt, EnvFilter};

#[derive(Parser)]
#[command(name = "clawops", about = "ZeroClaw multi-tenant ops gateway")]
struct Cli {
    /// Path to clawops.toml.
    #[arg(short, long, default_value = "clawops.toml", global = true)]
    config: PathBuf,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the HTTP server.
    Serve,

    /// Manually provision a new user (useful for bootstrap / testing).
    Provision {
        #[arg(long)]
        openid: String,
        #[arg(long)]
        phone: Option<String>,
        #[arg(long)]
        display_name: Option<String>,
        /// Path to a JSON file describing enterprise_profile.
        #[arg(long)]
        enterprise_profile: Option<PathBuf>,
    },

    /// Stop a user's zeroclaw process and release its port.
    Stop {
        #[arg(long)]
        openid: String,
    },

    /// List all known users.
    List,

    /// Run a single reaper pass against the configured DB and exit.
    /// Useful for ad-hoc cleanup or cron-driven invocations.
    Reap,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,clawops=debug")),
        )
        .init();

    let cli = Cli::parse();
    let cfg = Arc::new(Config::load(&cli.config)?);
    let pool = db::connect(&cfg.database.url).await?;

    let backend = process::make(
        &cfg.provisioner.backend,
        cfg.zeroclaw.binary.clone(),
        cfg.zeroclaw.home_base.clone(),
    )?;
    let backend: Arc<dyn process::ProcessManager> = Arc::from(backend);

    let http_client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()?;

    let provisioner = Arc::new(Provisioner {
        pool: pool.clone(),
        cfg: cfg.clone(),
        backend: backend.clone(),
        http: http_client.clone(),
    });

    let wx = Arc::new(WxClient::new(cfg.wx.clone(), http_client.clone()));
    let limiters = Arc::new(AppLimiters::new(&cfg.rate_limit));

    match cli.cmd {
        Cmd::Serve => {
            // Reaper runs alongside the HTTP server, sharing pool +
            // provisioner. It outlives no longer than the process; tokio
            // drops the JoinHandle on shutdown.
            let _reaper = Reaper::new(pool.clone(), provisioner.clone(), cfg.reaper.clone()).spawn();

            let state = AppState {
                pool,
                cfg: cfg.clone(),
                provisioner,
                http: http_client,
                wx,
                limiters,
            };
            let app = http::router(state);
            let addr: std::net::SocketAddr =
                format!("{}:{}", cfg.server.host, cfg.server.port).parse()?;
            tracing::info!("clawops listening on http://{addr}");
            let listener = tokio::net::TcpListener::bind(addr).await?;
            axum::serve(listener, app).await?;
        }
        Cmd::Reap => {
            // Manual one-shot: run a single reaper tick from the CLI for
            // testing or ad-hoc cleanup. Useful with --config pointing at
            // production DB while clawops.service stays up — both share
            // the same SQLite file with WAL.
            let reaper = Reaper::new(pool.clone(), provisioner.clone(), cfg.reaper.clone());
            let n = reaper.tick().await?;
            println!("reaper one-shot stopped {n} idle user(s)");
        }
        Cmd::Provision {
            openid,
            phone,
            display_name,
            enterprise_profile,
        } => {
            let profile = match enterprise_profile {
                Some(p) => Some(serde_json::from_str::<serde_json::Value>(
                    &std::fs::read_to_string(p)?,
                )?),
                None => None,
            };
            let new = users::NewUser {
                openid,
                phone,
                display_name,
                enterprise_profile: profile,
            };
            let out = provisioner.provision(&new).await?;
            println!(
                "provisioned: openid={} uid={} port={} workspace={} paired={}",
                out.openid, out.linux_uid, out.port, out.workspace_path, out.paired
            );
        }
        Cmd::Stop { openid } => {
            provisioner.stop(&openid).await?;
            println!("stopped: {openid}");
        }
        Cmd::List => {
            let rows: Vec<users::User> = sqlx::query_as(
                "SELECT * FROM users ORDER BY created_at DESC",
            )
            .fetch_all(&pool)
            .await?;
            for u in rows {
                println!(
                    "{:<40} {:<10} status={:<14} port={:?} active={}",
                    u.openid, u.linux_uid, u.status, u.port, u.last_active_at
                );
            }
        }
    }

    Ok(())
}
