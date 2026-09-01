//! Reaper — background task that stops idle zeroclaw daemons to reclaim
//! memory. Runs inside `clawops serve`. Every `tick_secs` it scans the
//! `users` table and stops any user whose `last_active_at` is older than
//! `idle_stop_minutes`. Workspace files are preserved; the user can come
//! back at any time and ClawOps will re-start their daemon on next /chat
//! request via Provisioner::ensure_running.
//!
//! Stopping releases the port back into the pool and sets status='stopped'.
//! `idle_archive_minutes` is recorded for now (not enforced) — Phase 4
//! may move very-old users into a permanent 'archived' state.
//!
//! The same tick also deletes expired rows from `sessions`. Nothing else
//! ever did: `sessions::purge_expired` existed from the start and was
//! never called, so the table only ever grew — one row per login, kept
//! forever, on a path callers hit as often as they like (each
//! `/auth/*-login` inserts a fresh row rather than reusing a live
//! session). `migrations/0002_sessions.sql` claimed the reaper pruned
//! them; now it does.

use crate::config::ReaperConfig;
use crate::provisioner::Provisioner;
use crate::sessions;
use chrono::{DateTime, Duration, Utc};
use sqlx::SqlitePool;
use std::sync::Arc;
use std::time::Duration as StdDuration;

/// What one tick did. Both numbers are reported so `clawops reap` can say
/// which of the two jobs actually had work to do.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReapOutcome {
    /// Idle daemons stopped.
    pub stopped: usize,
    /// Expired session rows deleted.
    pub sessions_purged: u64,
}

pub struct Reaper {
    pub pool: SqlitePool,
    pub provisioner: Arc<Provisioner>,
    pub cfg: ReaperConfig,
    /// How often to scan. Defaults to 1 hour. Overridable for tests.
    pub tick_secs: u64,
}

impl Reaper {
    pub fn new(
        pool: SqlitePool,
        provisioner: Arc<Provisioner>,
        cfg: ReaperConfig,
    ) -> Self {
        let tick_secs = cfg.tick_secs;
        Self {
            pool,
            provisioner,
            cfg,
            tick_secs,
        }
    }

    /// Spawn the loop. Returns the JoinHandle so callers can shut it down,
    /// though in practice the process exits and tokio drops it.
    pub fn spawn(self) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            tracing::info!(
                idle_stop_minutes = self.cfg.idle_stop_minutes,
                tick_secs = self.tick_secs,
                "reaper started"
            );
            loop {
                tokio::time::sleep(StdDuration::from_secs(self.tick_secs)).await;
                if let Err(e) = self.tick().await {
                    tracing::warn!("reaper tick failed: {e:#}");
                }
            }
        })
    }

    /// One pass: stop idle daemons, then drop expired sessions. Public for
    /// direct tests.
    pub async fn tick(&self) -> crate::Result<ReapOutcome> {
        let stopped = self.stop_idle_daemons().await?;

        // Deliberately not `?`: reclaiming memory is the reaper's main job
        // and it has already succeeded by this point. A failed DELETE must
        // not turn the whole tick into "reaper tick failed" and hide that.
        let sessions_purged = match sessions::purge_expired(&self.pool).await {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!("reaper: purging expired sessions failed: {e:#}");
                0
            }
        };
        if sessions_purged > 0 {
            tracing::info!(sessions_purged, "reaper: purged expired sessions");
        }

        Ok(ReapOutcome {
            stopped,
            sessions_purged,
        })
    }

    async fn stop_idle_daemons(&self) -> crate::Result<usize> {
        let cutoff = Utc::now() - Duration::minutes(self.cfg.idle_stop_minutes);
        let rows: Vec<(String, DateTime<Utc>)> = sqlx::query_as(
            "SELECT openid, last_active_at FROM users WHERE status = 'running' AND last_active_at < ?",
        )
        .bind(cutoff)
        .fetch_all(&self.pool)
        .await?;

        let mut stopped = 0usize;
        for (openid, last_active) in rows {
            tracing::info!(
                openid = openid,
                last_active = %last_active,
                "reaper: stopping idle user"
            );
            match self.provisioner.stop(&openid).await {
                Ok(()) => stopped += 1,
                Err(e) => {
                    tracing::warn!(openid = openid, "reaper: stop failed: {e:#}");
                }
            }
        }
        if stopped > 0 {
            tracing::info!(stopped = stopped, "reaper tick complete");
        }
        Ok(stopped)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::{db, process, sessions};

    /// A real pool (migrations and all), a mock process backend, and a
    /// reaper wired the way `serve` wires it.
    ///
    /// File-backed rather than `sqlite::memory:` on purpose: an in-memory
    /// URL gives each pooled connection its own empty database, so a row
    /// written on one connection is invisible to the next and the test
    /// passes or fails depending on which connection it draws.
    async fn harness(idle_stop_minutes: i64) -> (tempfile::TempDir, Reaper) {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("clawops.db");
        let pool = db::connect(&format!("sqlite://{}?mode=rwc", db_path.display()))
            .await
            .unwrap();

        let cfg: Config = toml::from_str(&format!(
            r#"
[server]
host = "127.0.0.1"
port = 8088
[database]
url = "sqlite://{db}"
[zeroclaw]
binary = "/usr/local/bin/zeroclaw"
home_base = "{home}"
port_range_start = 43000
port_range_end = 43100
[provisioner]
backend = "mock"
template_dir = "{tpl}"
[zeroclaw_template]
default_provider = "deepseek"
default_model = "deepseek-v4-pro"
"#,
            db = db_path.display(),
            home = tmp.path().join("homes").display(),
            tpl = tmp.path().join("templates").display(),
        ))
        .unwrap();

        let backend: Arc<dyn process::ProcessManager> = Arc::from(
            process::make("mock", cfg.zeroclaw.binary.clone(), cfg.zeroclaw.home_base.clone())
                .unwrap(),
        );
        let provisioner = Arc::new(Provisioner {
            pool: pool.clone(),
            cfg: Arc::new(cfg),
            backend,
            http: reqwest::Client::new(),
        });

        let reaper = Reaper::new(
            pool,
            provisioner,
            ReaperConfig {
                idle_stop_minutes,
                idle_archive_minutes: 365 * 24 * 60,
                tick_secs: 60,
            },
        );
        (tmp, reaper)
    }

    /// `sessions.openid` is a foreign key, so a session needs a user.
    async fn add_user(pool: &SqlitePool, openid: &str, last_active: DateTime<Utc>) {
        sqlx::query(
            r#"INSERT INTO users
               (openid, linux_uid, workspace_path, status, created_at, last_active_at)
               VALUES (?, ?, '/tmp/ws', 'running', ?, ?)"#,
        )
        .bind(openid)
        .bind(format!("claw-{openid}"))
        .bind(Utc::now())
        .bind(last_active)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn add_session(pool: &SqlitePool, token: &str, openid: &str, expires: DateTime<Utc>) {
        sqlx::query(
            "INSERT INTO sessions (token, openid, issued_at, expires_at) VALUES (?, ?, ?, ?)",
        )
        .bind(token)
        .bind(openid)
        .bind(expires - Duration::days(30))
        .bind(expires)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn session_tokens(pool: &SqlitePool) -> Vec<String> {
        sqlx::query_as::<_, (String,)>("SELECT token FROM sessions ORDER BY token")
            .fetch_all(pool)
            .await
            .unwrap()
            .into_iter()
            .map(|(t,)| t)
            .collect()
    }

    /// The bug this whole change exists for: `purge_expired` was written
    /// but never called from anywhere, so `sessions` only ever grew.
    #[tokio::test]
    async fn tick_purges_expired_sessions_and_keeps_live_ones() {
        let (_tmp, reaper) = harness(90 * 24 * 60).await;
        let pool = reaper.pool.clone();

        add_user(&pool, "u1", Utc::now()).await;
        add_session(&pool, "live", "u1", Utc::now() + Duration::days(7)).await;
        add_session(&pool, "stale", "u1", Utc::now() - Duration::minutes(1)).await;
        add_session(&pool, "ancient", "u1", Utc::now() - Duration::days(400)).await;

        let out = reaper.tick().await.unwrap();

        assert_eq!(out.sessions_purged, 2);
        assert_eq!(session_tokens(&pool).await, vec!["live"]);
    }

    /// Purging must not depend on there being idle daemons to stop, and
    /// vice versa — they are independent jobs sharing one tick.
    #[tokio::test]
    async fn tick_purges_even_when_nothing_is_idle() {
        let (_tmp, reaper) = harness(90 * 24 * 60).await;
        let pool = reaper.pool.clone();

        add_user(&pool, "active", Utc::now()).await;
        add_session(&pool, "stale", "active", Utc::now() - Duration::days(1)).await;

        let out = reaper.tick().await.unwrap();

        assert_eq!(out.stopped, 0, "nobody was idle");
        assert_eq!(out.sessions_purged, 1);
    }

    /// A tick with nothing to do must report zero on both counts rather
    /// than erroring — `clawops reap` runs it on demand against a live DB.
    #[tokio::test]
    async fn empty_tick_is_a_no_op() {
        let (_tmp, reaper) = harness(90 * 24 * 60).await;
        assert_eq!(reaper.tick().await.unwrap(), ReapOutcome::default());
    }

    /// Stopping an idle daemon releases its port; the purge runs in the
    /// same tick and neither disturbs the other.
    #[tokio::test]
    async fn tick_stops_idle_daemons_alongside_the_purge() {
        let (_tmp, reaper) = harness(60).await;
        let pool = reaper.pool.clone();

        add_user(&pool, "idle", Utc::now() - Duration::minutes(120)).await;
        add_session(&pool, "stale", "idle", Utc::now() - Duration::days(1)).await;

        let out = reaper.tick().await.unwrap();

        assert_eq!(out.stopped, 1);
        assert_eq!(out.sessions_purged, 1);
        let status: (String,) = sqlx::query_as("SELECT status FROM users WHERE openid = 'idle'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(status.0, "stopped");
    }

    /// Sessions are FK'd to users with ON DELETE CASCADE, so a purge must
    /// never be what removes a *user*. Guards against someone "fixing"
    /// the query into a join later.
    #[tokio::test]
    async fn purge_never_touches_the_users_table() {
        let (_tmp, reaper) = harness(90 * 24 * 60).await;
        let pool = reaper.pool.clone();

        add_user(&pool, "u1", Utc::now()).await;
        add_session(&pool, "stale", "u1", Utc::now() - Duration::days(1)).await;

        reaper.tick().await.unwrap();

        let n: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM users")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(n.0, 1, "the user must outlive their expired session");
    }

    /// `sessions::purge_expired` is the function the reaper calls; assert
    /// its boundary directly so an off-by-one there can't hide behind the
    /// reaper's own accounting.
    #[tokio::test]
    async fn purge_boundary_is_expiry_in_the_past() {
        let (_tmp, reaper) = harness(90 * 24 * 60).await;
        let pool = reaper.pool.clone();

        add_user(&pool, "u1", Utc::now()).await;
        add_session(&pool, "one_second_left", "u1", Utc::now() + Duration::seconds(1)).await;

        assert_eq!(sessions::purge_expired(&pool).await.unwrap(), 0);
        assert_eq!(session_tokens(&pool).await, vec!["one_second_left"]);
    }
}
