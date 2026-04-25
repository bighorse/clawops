use crate::{Error, Result};
use chrono::{DateTime, Duration, Utc};
use sqlx::SqlitePool;
use uuid::Uuid;

const DEFAULT_TTL_DAYS: i64 = 30;

#[derive(Debug, Clone)]
pub struct Session {
    pub token: String,
    pub openid: String,
    pub issued_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

pub fn new_token() -> String {
    format!(
        "{}{}",
        Uuid::new_v4().simple(),
        Uuid::new_v4().simple()
    )
}

pub async fn issue(
    pool: &SqlitePool,
    openid: &str,
    user_agent: Option<&str>,
) -> Result<Session> {
    let token = new_token();
    let now = Utc::now();
    let expires_at = now + Duration::days(DEFAULT_TTL_DAYS);

    sqlx::query(
        r#"INSERT INTO sessions (token, openid, issued_at, expires_at, user_agent)
           VALUES (?, ?, ?, ?, ?)"#,
    )
    .bind(&token)
    .bind(openid)
    .bind(now)
    .bind(expires_at)
    .bind(user_agent)
    .execute(pool)
    .await?;

    Ok(Session {
        token,
        openid: openid.to_string(),
        issued_at: now,
        expires_at,
    })
}

pub async fn resolve(pool: &SqlitePool, token: &str) -> Result<String> {
    let row: Option<(String, DateTime<Utc>)> = sqlx::query_as(
        "SELECT openid, expires_at FROM sessions WHERE token = ?",
    )
    .bind(token)
    .fetch_optional(pool)
    .await?;

    match row {
        Some((openid, expires)) if expires > Utc::now() => Ok(openid),
        Some(_) => Err(Error::Other("session expired".into())),
        None => Err(Error::Other("invalid token".into())),
    }
}

pub async fn revoke(pool: &SqlitePool, token: &str) -> Result<()> {
    sqlx::query("DELETE FROM sessions WHERE token = ?")
        .bind(token)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn purge_expired(pool: &SqlitePool) -> Result<u64> {
    let r = sqlx::query("DELETE FROM sessions WHERE expires_at < ?")
        .bind(Utc::now())
        .execute(pool)
        .await?;
    Ok(r.rows_affected())
}
