use crate::Result;
use chrono::{DateTime, Duration, Utc};
use regex::Regex;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SopTask {
    pub task_id: String,
    pub openid: String,
    pub sop_name: String,
    pub enterprise_name: Option<String>,
    pub enterprise_id: Option<i64>,
    pub qualification_enterprise_id: Option<i64>,
    pub status: String,
    pub deeplink: Option<String>,
    pub response_text: Option<String>,
    pub error_message: Option<String>,
    pub estimated_seconds: i64,
    /// Workspace-relative path of the brief this run produced, e.g.
    /// `briefs/cambricon/brief_2026-08-12.md`. Filled when the run finishes.
    /// Without it there is no way to tell which brief came from which task.
    pub artifact_path: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
}

impl<'r> sqlx::FromRow<'r, sqlx::sqlite::SqliteRow> for SopTask {
    fn from_row(row: &'r sqlx::sqlite::SqliteRow) -> std::result::Result<Self, sqlx::Error> {
        use sqlx::Row;
        Ok(SopTask {
            task_id: row.try_get("task_id")?,
            openid: row.try_get("openid")?,
            sop_name: row.try_get("sop_name")?,
            enterprise_name: row.try_get("enterprise_name")?,
            enterprise_id: row.try_get("enterprise_id")?,
            qualification_enterprise_id: row.try_get("qualification_enterprise_id")?,
            status: row.try_get("status")?,
            deeplink: row.try_get("deeplink")?,
            response_text: row.try_get("response_text")?,
            error_message: row.try_get("error_message")?,
            estimated_seconds: row.try_get("estimated_seconds")?,
            artifact_path: row.try_get("artifact_path")?,
            created_at: row.try_get("created_at")?,
            updated_at: row.try_get("updated_at")?,
            completed_at: row.try_get("completed_at")?,
        })
    }
}

/// Generate a task_id with "tsk_" prefix + UUID v4 (simpler hex). Cheap,
/// collision-resistant enough for this scale (single SQLite, < 1M rows).
pub fn new_task_id() -> String {
    format!("tsk_{}", uuid::Uuid::new_v4().simple())
}

/// Insert a new task at status=pending. Used at the start of /chat
/// handler when SOP intent is detected; status quickly moves to
/// running once the background spawn picks it up.
pub async fn insert_pending(
    pool: &SqlitePool,
    task_id: &str,
    openid: &str,
    sop_name: &str,
    enterprise_name: Option<&str>,
    estimated_seconds: u32,
) -> Result<()> {
    let now = Utc::now();
    sqlx::query(
        "INSERT INTO sop_tasks (task_id, openid, sop_name, enterprise_name, status, \
         estimated_seconds, created_at, updated_at) \
         VALUES (?, ?, ?, ?, 'pending', ?, ?, ?)",
    )
    .bind(task_id)
    .bind(openid)
    .bind(sop_name)
    .bind(enterprise_name)
    .bind(estimated_seconds as i64)
    .bind(now)
    .bind(now)
    .execute(pool)
    .await?;
    Ok(())
}

/// Insert a row directly at status=done, copying deeplink + ids from
/// a prior cache-hit task. Used when /chat sees a recent done task
/// for the same (openid, sop_name, enterprise_id) — we still record
/// this as a new entry in the user's task list so the UX is uniform.
#[allow(clippy::too_many_arguments)]
pub async fn insert_cached_done(
    pool: &SqlitePool,
    task_id: &str,
    openid: &str,
    sop_name: &str,
    enterprise_name: Option<&str>,
    enterprise_id: Option<i64>,
    qualification_enterprise_id: Option<i64>,
    deeplink: Option<&str>,
    response_text: Option<&str>,
    estimated_seconds: u32,
) -> Result<()> {
    let now = Utc::now();
    sqlx::query(
        "INSERT INTO sop_tasks (task_id, openid, sop_name, enterprise_name, enterprise_id, \
         qualification_enterprise_id, status, deeplink, response_text, estimated_seconds, \
         created_at, updated_at, completed_at) \
         VALUES (?, ?, ?, ?, ?, ?, 'done', ?, ?, ?, ?, ?, ?)",
    )
    .bind(task_id)
    .bind(openid)
    .bind(sop_name)
    .bind(enterprise_name)
    .bind(enterprise_id)
    .bind(qualification_enterprise_id)
    .bind(deeplink)
    .bind(response_text)
    .bind(estimated_seconds as i64)
    .bind(now)
    .bind(now)
    .bind(now)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn mark_running(pool: &SqlitePool, task_id: &str) -> Result<()> {
    let now = Utc::now();
    sqlx::query("UPDATE sop_tasks SET status = 'running', updated_at = ? WHERE task_id = ?")
        .bind(now)
        .bind(task_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Set/overwrite the enterprise_name on an existing task. Used when /chat adopts
/// a fallback task that the "starting" webhook created without a name (dedup),
/// so the task record and workspace-deeplink fallback carry the right company.
pub async fn update_enterprise_name(
    pool: &SqlitePool,
    task_id: &str,
    enterprise_name: &str,
) -> Result<()> {
    let now = Utc::now();
    sqlx::query("UPDATE sop_tasks SET enterprise_name = ?, updated_at = ? WHERE task_id = ?")
        .bind(enterprise_name)
        .bind(now)
        .bind(task_id)
        .execute(pool)
        .await?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub async fn mark_done(
    pool: &SqlitePool,
    task_id: &str,
    enterprise_id: Option<i64>,
    qualification_enterprise_id: Option<i64>,
    deeplink: Option<&str>,
    response_text: &str,
) -> Result<()> {
    let now = Utc::now();
    sqlx::query(
        "UPDATE sop_tasks SET status = 'done', enterprise_id = ?, \
         qualification_enterprise_id = ?, deeplink = ?, response_text = ?, \
         updated_at = ?, completed_at = ? WHERE task_id = ?",
    )
    .bind(enterprise_id)
    .bind(qualification_enterprise_id)
    .bind(deeplink)
    .bind(response_text)
    .bind(now)
    .bind(now)
    .bind(task_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Timeout stale running tasks: if a task has been in running/pending status
/// for more than `timeout_minutes`, mark it failed. Called by a background job.
/// Returns the (openid, sop_name) of the tasks that were timed out, so the
/// caller can notify those users (the SOP hung with no done event).
pub async fn timeout_stale(
    pool: &SqlitePool,
    timeout_minutes: i64,
) -> Result<Vec<(String, String)>> {
    let cutoff = Utc::now() - Duration::minutes(timeout_minutes);
    // Capture who is about to be timed out before flipping the status.
    let stale: Vec<(String, String)> = sqlx::query_as(
        "SELECT openid, sop_name FROM sop_tasks \
         WHERE status IN ('running','pending') AND updated_at < ?",
    )
    .bind(cutoff)
    .fetch_all(pool)
    .await?;
    sqlx::query(
        "UPDATE sop_tasks SET status='failed', error_message='timeout: no done event received', \
         updated_at=?, completed_at=? \
         WHERE status IN ('running','pending') AND updated_at < ?",
    )
    .bind(Utc::now())
    .bind(Utc::now())
    .bind(cutoff)
    .execute(pool)
    .await?;
    Ok(stale)
}

pub async fn mark_failed(pool: &SqlitePool, task_id: &str, error: &str) -> Result<()> {
    let now = Utc::now();
    sqlx::query(
        "UPDATE sop_tasks SET status = 'failed', error_message = ?, \
         updated_at = ?, completed_at = ? WHERE task_id = ?",
    )
    .bind(error)
    .bind(now)
    .bind(now)
    .bind(task_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// List tasks for the right-top "task list" UI. Only returns within
/// the 30-day retention window. Sort: newest first.
pub async fn list_for_user(
    pool: &SqlitePool,
    openid: &str,
    status: Option<&str>,
    sop_name: Option<&str>,
    limit: i64,
) -> Result<(Vec<SopTask>, i64)> {
    let limit = limit.clamp(1, 200);
    let cutoff = Utc::now() - Duration::days(30);

    // Total count (for has_more decision)
    let total: i64 = match (status, sop_name) {
        (None, None) => sqlx::query_scalar(
            "SELECT COUNT(*) FROM sop_tasks WHERE openid = ? AND created_at > ?",
        )
        .bind(openid)
        .bind(cutoff)
        .fetch_one(pool)
        .await?,
        (Some(s), None) => sqlx::query_scalar(
            "SELECT COUNT(*) FROM sop_tasks WHERE openid = ? AND created_at > ? AND status = ?",
        )
        .bind(openid)
        .bind(cutoff)
        .bind(s)
        .fetch_one(pool)
        .await?,
        (None, Some(n)) => sqlx::query_scalar(
            "SELECT COUNT(*) FROM sop_tasks WHERE openid = ? AND created_at > ? AND sop_name = ?",
        )
        .bind(openid)
        .bind(cutoff)
        .bind(n)
        .fetch_one(pool)
        .await?,
        (Some(s), Some(n)) => sqlx::query_scalar(
            "SELECT COUNT(*) FROM sop_tasks WHERE openid = ? AND created_at > ? AND status = ? AND sop_name = ?",
        )
        .bind(openid)
        .bind(cutoff)
        .bind(s)
        .bind(n)
        .fetch_one(pool)
        .await?,
    };

    let rows: Vec<SopTask> = match (status, sop_name) {
        (None, None) => sqlx::query_as(
            "SELECT * FROM sop_tasks WHERE openid = ? AND created_at > ? \
             ORDER BY created_at DESC LIMIT ?",
        )
        .bind(openid)
        .bind(cutoff)
        .bind(limit)
        .fetch_all(pool)
        .await?,
        (Some(s), None) => sqlx::query_as(
            "SELECT * FROM sop_tasks WHERE openid = ? AND created_at > ? AND status = ? \
             ORDER BY created_at DESC LIMIT ?",
        )
        .bind(openid)
        .bind(cutoff)
        .bind(s)
        .bind(limit)
        .fetch_all(pool)
        .await?,
        (None, Some(n)) => sqlx::query_as(
            "SELECT * FROM sop_tasks WHERE openid = ? AND created_at > ? AND sop_name = ? \
             ORDER BY created_at DESC LIMIT ?",
        )
        .bind(openid)
        .bind(cutoff)
        .bind(n)
        .bind(limit)
        .fetch_all(pool)
        .await?,
        (Some(s), Some(n)) => sqlx::query_as(
            "SELECT * FROM sop_tasks WHERE openid = ? AND created_at > ? AND status = ? AND sop_name = ? \
             ORDER BY created_at DESC LIMIT ?",
        )
        .bind(openid)
        .bind(cutoff)
        .bind(s)
        .bind(n)
        .bind(limit)
        .fetch_all(pool)
        .await?,
    };

    Ok((rows, total))
}

/// Look up the most recent done task for cache check.
/// Returns None if no cached result within ttl_days.
pub async fn find_cached_done(
    pool: &SqlitePool,
    openid: &str,
    sop_name: &str,
    enterprise_name: Option<&str>,
    ttl_days: u32,
) -> Result<Option<SopTask>> {
    let cutoff = Utc::now() - Duration::days(ttl_days as i64);
    // Cache key for now: (openid, sop_name, enterprise_name).
    // We match by enterprise_name (string) since enterprise_id is only
    // known after step 1 of the SOP runs.
    let row: Option<SopTask> = match enterprise_name {
        Some(name) => sqlx::query_as(
            "SELECT * FROM sop_tasks WHERE openid = ? AND sop_name = ? \
             AND enterprise_name = ? AND status = 'done' AND deeplink IS NOT NULL \
             AND created_at > ? ORDER BY created_at DESC LIMIT 1",
        )
        .bind(openid)
        .bind(sop_name)
        .bind(name)
        .bind(cutoff)
        .fetch_optional(pool)
        .await?,
        None => sqlx::query_as(
            "SELECT * FROM sop_tasks WHERE openid = ? AND sop_name = ? \
             AND enterprise_name IS NULL AND status = 'done' AND deeplink IS NOT NULL \
             AND created_at > ? ORDER BY created_at DESC LIMIT 1",
        )
        .bind(openid)
        .bind(sop_name)
        .bind(cutoff)
        .fetch_optional(pool)
        .await?,
    };
    Ok(row)
}

/// Find the most recent pending task for (openid, sop_name).
/// Used by the "starting" webhook to grab the row /chat just created and move
/// it to running. Intentionally pending-only: matching running could grab a
/// stale row from a prior run instead of the fresh pending one.
pub async fn find_pending_by_sop(
    pool: &SqlitePool,
    openid: &str,
    sop_name: &str,
) -> Result<Option<String>> {
    let task_id: Option<String> = sqlx::query_scalar(
        "SELECT task_id FROM sop_tasks WHERE openid = ? AND sop_name = ? \
         AND status = 'pending' ORDER BY created_at DESC LIMIT 1",
    )
    .bind(openid)
    .bind(sop_name)
    .fetch_optional(pool)
    .await?;
    Ok(task_id)
}

/// Find the most recent *active* (pending OR running) task for (openid, sop_name).
/// Used by the "done" webhook: by the time "done" fires, the "starting" event has
/// already moved the task to 'running', so matching only 'pending' misses it —
/// the result then gets orphaned into a fresh enterprise-less done row while the
/// real task lingers until timeout_stale marks it failed. Matching running too
/// lets the real task be marked done with its enterprise_name + deeplink intact.
/// How recently a task must have been created to be adopted rather than
/// duplicated.
///
/// The point of adoption is to collapse the two records that a *single*
/// trigger can produce — the model calls sop_execute and the "starting"
/// webhook lands before /chat returns, so both paths try to create a row.
/// Those land milliseconds apart.
///
/// Matching on (user, sop_name) alone made this window infinite, which
/// silently merged genuinely separate requests: ask about one company, then
/// another while the first is still running, and the second got folded into
/// the first — the user saw "已为您发起" both times but only one task existed.
///
/// Two seconds, because the race this exists to fix resolves in milliseconds,
/// while a person typing a second company name needs longer. It is still a
/// heuristic: two requests fired programmatically within the window would
/// collapse into one. A precise fix needs the daemon to say which run an
/// event belongs to, which today's webhook payload doesn't carry.
const ADOPT_WINDOW_SECS: i64 = 2;

/// Find a just-created task to adopt instead of inserting a duplicate.
///
/// Deliberately time-boxed — see `ADOPT_WINDOW_SECS`. Do **not** use this to
/// match a finishing run: by then the task is minutes old and would fall
/// outside the window, leaving the completion to create an orphan row.
pub async fn find_adoptable(
    pool: &SqlitePool,
    openid: &str,
    sop_name: &str,
) -> Result<Option<String>> {
    let cutoff = Utc::now() - chrono::Duration::seconds(ADOPT_WINDOW_SECS);
    let task_id: Option<String> = sqlx::query_scalar(
        "SELECT task_id FROM sop_tasks WHERE openid = ? AND sop_name = ? \
         AND status IN ('pending', 'running') AND created_at >= ? \
         ORDER BY created_at DESC LIMIT 1",
    )
    .bind(openid)
    .bind(sop_name)
    .bind(cutoff)
    .fetch_optional(pool)
    .await?;
    Ok(task_id)
}

/// Find the unfinished task a completion belongs to.
///
/// No time bound — a run takes minutes, so the row is long past any adoption
/// window. Oldest-first: with several runs in flight the completions tend to
/// arrive in the order they were started, so first-in-first-out is the best
/// guess available. It is a guess — the webhook carries no run identifier, so
/// two runs finishing out of order would be attributed to each other's task.
/// The brief itself is matched separately and correctly, via the marker in the
/// wrap-up, so a mix-up here swaps which task a brief hangs off, not which
/// company a brief describes.
pub async fn find_active_by_sop(
    pool: &SqlitePool,
    openid: &str,
    sop_name: &str,
) -> Result<Option<String>> {
    let task_id: Option<String> = sqlx::query_scalar(
        "SELECT task_id FROM sop_tasks WHERE openid = ? AND sop_name = ? \
         AND status IN ('pending', 'running') ORDER BY created_at ASC LIMIT 1",
    )
    .bind(openid)
    .bind(sop_name)
    .fetch_optional(pool)
    .await?;
    Ok(task_id)
}

/// Record which brief a finished run produced, plus the company name read
/// from that brief. Both are best-effort — a run that produced nothing simply
/// leaves them null.
pub async fn set_artifact(
    pool: &SqlitePool,
    task_id: &str,
    artifact_path: &str,
    enterprise_name: Option<&str>,
) -> Result<()> {
    if let Some(name) = enterprise_name {
        sqlx::query(
            "UPDATE sop_tasks SET artifact_path = ?, enterprise_name = COALESCE(enterprise_name, ?), \
             updated_at = ? WHERE task_id = ?",
        )
        .bind(artifact_path)
        .bind(name)
        .bind(Utc::now())
        .bind(task_id)
        .execute(pool)
        .await?;
    } else {
        sqlx::query("UPDATE sop_tasks SET artifact_path = ?, updated_at = ? WHERE task_id = ?")
            .bind(artifact_path)
            .bind(Utc::now())
            .bind(task_id)
            .execute(pool)
            .await?;
    }
    Ok(())
}

/// Fetch a single task row by task_id.
pub async fn get_by_id(pool: &SqlitePool, task_id: &str) -> Result<Option<SopTask>> {
    let row: Option<SopTask> =
        sqlx::query_as("SELECT * FROM sop_tasks WHERE task_id = ?")
            .bind(task_id)
            .fetch_optional(pool)
            .await?;
    Ok(row)
}

/// Extract the first mini-program deeplink and numeric id from response text.
/// Pattern: `/pages/<path>?id=<digits>` (or `&id=<digits>`).
pub fn extract_deeplink_and_qid(text: &str) -> (Option<String>, Option<i64>) {
    let re = match Regex::new(r"(/pages/[a-zA-Z0-9_/]+\?id=(\d+))") {
        Ok(r) => r,
        Err(_) => return (None, None),
    };
    if let Some(caps) = re.captures(text) {
        let path = caps.get(1).map(|m| m.as_str().to_string());
        let id = caps.get(2).and_then(|m| m.as_str().parse::<i64>().ok());
        (path, id)
    } else {
        (None, None)
    }
}
