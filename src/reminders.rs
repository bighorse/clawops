use crate::Result;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, SqlitePool};

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct Reminder {
    pub id: i64,
    pub openid: String,
    pub activity_id: String,
    pub activity_name: String,
    pub activity_time: String,
    pub activity_venue: String,
    pub remind_at: String,
    pub sent_at: Option<String>,
    pub failed_at: Option<String>,
    pub fail_reason: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Deserialize)]
pub struct CreateReminderReq {
    pub openid: String,
    pub activity_id: String,
    pub activity_name: String,
    /// ISO-8601 activity start time (shown in notification body).
    pub activity_time: String,
    #[serde(default)]
    pub activity_venue: String,
    /// ISO-8601 when to fire the notification (e.g. 1 day before activity_time).
    pub remind_at: String,
}

pub async fn insert(pool: &SqlitePool, req: &CreateReminderReq) -> Result<i64> {
    let row = sqlx::query_scalar(
        "INSERT INTO reminders \
         (openid, activity_id, activity_name, activity_time, activity_venue, remind_at) \
         VALUES (?, ?, ?, ?, ?, ?) RETURNING id",
    )
    .bind(&req.openid)
    .bind(&req.activity_id)
    .bind(&req.activity_name)
    .bind(&req.activity_time)
    .bind(&req.activity_venue)
    .bind(&req.remind_at)
    .fetch_one(pool)
    .await?;
    Ok(row)
}

/// Fetch all pending reminders whose remind_at <= now. Called by background job.
pub async fn fetch_due(pool: &SqlitePool) -> Result<Vec<Reminder>> {
    let now = Utc::now().to_rfc3339();
    let rows = sqlx::query_as::<_, Reminder>(
        "SELECT * FROM reminders \
         WHERE sent_at IS NULL AND failed_at IS NULL AND remind_at <= ? \
         ORDER BY remind_at ASC LIMIT 50",
    )
    .bind(now)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn mark_sent(pool: &SqlitePool, id: i64) -> Result<()> {
    let now = Utc::now().to_rfc3339();
    sqlx::query("UPDATE reminders SET sent_at = ? WHERE id = ?")
        .bind(now)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn mark_failed(pool: &SqlitePool, id: i64, reason: &str) -> Result<()> {
    let now = Utc::now().to_rfc3339();
    sqlx::query("UPDATE reminders SET failed_at = ?, fail_reason = ? WHERE id = ?")
        .bind(now)
        .bind(reason)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// List reminders for a user (for debugging / admin).
pub async fn list_for_user(pool: &SqlitePool, openid: &str) -> Result<Vec<Reminder>> {
    let rows = sqlx::query_as::<_, Reminder>(
        "SELECT * FROM reminders WHERE openid = ? ORDER BY remind_at DESC LIMIT 100",
    )
    .bind(openid)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Check if a reminder already exists for this user + activity (dedup).
pub async fn exists(pool: &SqlitePool, openid: &str, activity_id: &str) -> Result<bool> {
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM reminders \
         WHERE openid = ? AND activity_id = ? AND sent_at IS NULL AND failed_at IS NULL",
    )
    .bind(openid)
    .bind(activity_id)
    .fetch_one(pool)
    .await?;
    Ok(count > 0)
}
