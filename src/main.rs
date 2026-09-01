use clap::{Parser, Subcommand};
use clawops::auth::WxClient;
use clawops::config::Config;
use clawops::http::AppState;
use clawops::limits::AppLimiters;
use clawops::provisioner::Provisioner;
use clawops::reaper::Reaper;
use clawops::{db, http, process, users};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::broadcast;
use tracing_subscriber::{fmt, EnvFilter};

#[derive(Parser)]
#[command(name = "clawops", version, about = "ZeroClaw multi-tenant ops gateway")]
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
        /// Which breed's templates to render this tenant from.
        /// Defaults to `provisioner.default_breed`.
        #[arg(long)]
        breed: Option<String>,
    },

    /// Stop a user's zeroclaw process and release its port.
    Stop {
        #[arg(long)]
        openid: String,
    },

    /// Re-render IDENTITY.md / SOUL.md / USER.md / config.toml / skills/* /
    /// sops/* for one or more users from current templates, then restart
    /// their daemon. Use after deploying template changes to roll out the
    /// new prompts/SOPs to existing users without making them re-pair.
    /// paired_token is preserved — existing client sessions stay valid.
    RefreshWorkspace {
        /// Single openid to refresh. Mutually exclusive with --all.
        #[arg(long, conflicts_with = "all")]
        openid: Option<String>,
        /// Refresh every user in the DB (sequentially, stopping on first error).
        #[arg(long)]
        all: bool,
    },

    /// List all known users.
    List,

    /// Run a single reaper pass against the configured DB and exit.
    /// Useful for ad-hoc cleanup or cron-driven invocations.
    Reap,

    /// List the breeds this box can render, with the digest of each
    /// template tree and the tenant count on it. The digest is what to
    /// compare against a development machine to answer "is the swarm
    /// actually running the lobster I pushed?".
    Breeds,

    /// Install a breed bundle from a local tar/tar.gz, then re-render
    /// that breed's tenants. Same code path as `PUT /admin/breeds/:breed`
    /// — for use on the box itself, where there is no admin token to hand.
    InstallBreed {
        /// Breed name, `[a-z0-9_-]+`.
        #[arg(long)]
        breed: String,
        /// Path to the bundle (`.tar` or `.tar.gz`).
        #[arg(long)]
        bundle: PathBuf,
        /// Install only; leave existing tenants on the old templates.
        #[arg(long)]
        no_refresh: bool,
    },

    /// Re-render every tenant on one breed from the templates already on
    /// disk. Unlike `refresh-workspace --all` this leaves other breeds'
    /// daemons untouched.
    RefreshBreed {
        #[arg(long)]
        breed: String,
    },

    /// Move one tenant to another breed and re-render them.
    SetBreed {
        #[arg(long)]
        openid: String,
        #[arg(long)]
        breed: String,
    },
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

    let (sop_event_tx, _) = broadcast::channel(256);

    match cli.cmd {
        Cmd::Serve => {
            let _reaper = Reaper::new(pool.clone(), provisioner.clone(), cfg.reaper.clone()).spawn();
            http::spawn_sop_task_watchdog(
                pool.clone(),
                http_client.clone(),
                cfg.policy_match.api_base.clone(),
            );
            http::spawn_qualification_reminder_cron(pool.clone(), cfg.clone(), http_client.clone());

            let state = AppState {
                pool,
                cfg: cfg.clone(),
                provisioner,
                http: http_client,
                wx,
                limiters,
                sop_event_tx,
            };
            // An empty wx.backend_base_url puts WxClient in mock mode, where
            // /auth/wx-login trusts a caller-supplied `mock_openid` verbatim —
            // anyone can impersonate anyone. `#[serde(default)]` means simply
            // omitting the line lands you there with no error, so make it a
            // deliberate choice rather than an accident: on a real backend the
            // operator must write `allow_mock_login = true` to proceed, and we
            // shout about it on every boot.
            if cfg.provisioner.backend != "mock" && cfg.wx.backend_base_url.trim().is_empty() {
                if !cfg.wx.allow_mock_login {
                    tracing::error!(
                        "refusing to start: [wx].backend_base_url is empty while \
                         provisioner.backend = \"{}\". That enables mock login, where \
                         any caller can impersonate any user via the `mock_openid` \
                         field. Set backend_base_url to the platform's code2session \
                         endpoint — or, if this host is intentionally pre-launch, set \
                         [wx].allow_mock_login = true and keep the port off the public \
                         internet.",
                        cfg.provisioner.backend
                    );
                    std::process::exit(1);
                }
                tracing::warn!(
                    "MOCK LOGIN ENABLED — /auth/wx-login accepts any `mock_openid`, \
                     so anyone who can reach this port can impersonate any user. \
                     Acceptable only while the port is unreachable from the public \
                     internet. Set [wx].backend_base_url to turn this off."
                );
            }

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
            let out = reaper.tick().await?;
            println!(
                "reaper one-shot: stopped {} idle user(s), purged {} expired session(s)",
                out.stopped, out.sessions_purged
            );
        }
        Cmd::Provision {
            openid,
            phone,
            display_name,
            enterprise_profile,
            breed,
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
                avatar_url: None,
                enterprise_profile: profile,
                breed,
            };
            let out = provisioner.provision(&new).await?;
            println!(
                "provisioned: openid={} uid={} port={} breed={} workspace={} paired={}",
                out.openid, out.linux_uid, out.port, out.breed, out.workspace_path, out.paired
            );
        }
        Cmd::Stop { openid } => {
            provisioner.stop(&openid).await?;
            println!("stopped: {openid}");
        }
        Cmd::RefreshWorkspace { openid, all } => {
            if !all && openid.is_none() {
                anyhow::bail!("refresh-workspace requires --openid <id> or --all");
            }
            let targets: Vec<String> = if all {
                sqlx::query_scalar::<_, String>("SELECT openid FROM users")
                    .fetch_all(&pool)
                    .await?
            } else {
                vec![openid.expect("checked above")]
            };
            println!("refreshing {} user(s)...", targets.len());
            for oid in &targets {
                match provisioner.refresh_workspace(oid).await {
                    Ok(()) => println!("  ok: {oid}"),
                    Err(e) => {
                        eprintln!("  FAILED: {oid} — {e}");
                        anyhow::bail!("refresh failed at {oid}");
                    }
                }
            }
            println!("done: refreshed {} user(s)", targets.len());
        }
        Cmd::List => {
            let rows: Vec<users::User> = sqlx::query_as(
                "SELECT * FROM users ORDER BY created_at DESC",
            )
            .fetch_all(&pool)
            .await?;
            for u in rows {
                println!(
                    "{:<40} {:<10} breed={:<16} status={:<14} port={:?} active={}",
                    u.openid, u.linux_uid, u.breed, u.status, u.port, u.last_active_at
                );
            }
        }
        Cmd::Breeds => {
            let counts: std::collections::BTreeMap<String, i64> =
                users::counts_by_breed(&pool).await?.into_iter().collect();
            let breeds = clawops::breeds::list(&cfg, &counts)?;
            println!("{:<20} {:<8} {:<8} {:<66} PATH", "BREED", "TENANTS", "FILES", "DIGEST");
            for b in breeds {
                let name = if b.builtin {
                    format!("{} (builtin)", b.name)
                } else {
                    b.name.clone()
                };
                println!(
                    "{:<20} {:<8} {:<8} {:<66} {}",
                    name, b.tenants, b.files, b.digest, b.path
                );
            }
            // A breed with tenants but no template directory can only
            // come from a directory deleted underneath a live swarm, and
            // every one of those tenants will fail its next refresh.
            let known: std::collections::BTreeSet<String> = clawops::breeds::list(&cfg, &counts)?
                .into_iter()
                .map(|b| b.name)
                .collect();
            for (breed, n) in &counts {
                if !known.contains(breed) {
                    eprintln!(
                        "WARNING: {n} tenant(s) on breed '{breed}', which has no template directory"
                    );
                }
            }
        }
        Cmd::InstallBreed {
            breed,
            bundle,
            no_refresh,
        } => {
            let bytes = std::fs::read(&bundle)?;
            // Same gate as the HTTP path. Installing on the box is not a
            // reason to skip the checks — the first breed that needed them
            // was pushed by someone with a shell on the server.
            let (info, warnings) = clawops::breeds::install_checked(
                &cfg,
                &breed,
                &bytes,
                false,
                |b| provisioner.probe_ctx(b),
            )?;
            println!(
                "installed breed={} files={} digest={} path={}",
                info.name, info.files, info.digest, info.path
            );
            for w in &warnings {
                eprintln!("  ⚠️  [{}] {}", w.rule, w.message);
            }
            if no_refresh {
                println!("skipped rollout (--no-refresh)");
            } else {
                let (ok, failures) = provisioner.refresh_breed(&breed).await?;
                println!("refreshed {ok} tenant(s)");
                for (openid, err) in &failures {
                    eprintln!("  FAILED: {openid} — {err}");
                }
                if !failures.is_empty() {
                    anyhow::bail!("{} tenant(s) failed to refresh", failures.len());
                }
            }
        }
        Cmd::RefreshBreed { breed } => {
            let (ok, failures) = provisioner.refresh_breed(&breed).await?;
            println!("refreshed {ok} tenant(s) on breed '{breed}'");
            for (openid, err) in &failures {
                eprintln!("  FAILED: {openid} — {err}");
            }
            if !failures.is_empty() {
                anyhow::bail!("{} tenant(s) failed to refresh", failures.len());
            }
        }
        Cmd::SetBreed { openid, breed } => {
            if cfg.provisioner.breed_dir(&breed).is_none() {
                anyhow::bail!("unknown breed '{breed}' — run `clawops breeds` to see what exists");
            }
            users::set_breed(&pool, &openid, &breed).await?;
            provisioner.refresh_workspace(&openid).await?;
            println!("{openid} moved to breed '{breed}' and re-rendered");
        }
    }

    Ok(())
}
