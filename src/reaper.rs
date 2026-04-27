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

use crate::config::ReaperConfig;
use crate::provisioner::Provisioner;
use chrono::{DateTime, Duration, Utc};
use sqlx::SqlitePool;
use std::sync::Arc;
use std::time::Duration as StdDuration;

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
        Self {
            pool,
            provisioner,
            cfg,
            tick_secs: 3600,
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

    /// One pass over the users table. Public for direct tests.
    pub async fn tick(&self) -> crate::Result<usize> {
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
