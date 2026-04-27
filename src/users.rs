use crate::{Error, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub openid: String,
    pub phone: Option<String>,
    pub display_name: Option<String>,
    pub enterprise_profile: Option<String>,
    pub linux_uid: String,
    pub workspace_path: String,
    pub port: Option<i64>,
    pub paired_token_enc: Option<String>,
    pub status: String,
    pub created_at: DateTime<Utc>,
    pub last_active_at: DateTime<Utc>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewUser {
    pub openid: String,
    pub phone: Option<String>,
    pub display_name: Option<String>,
    pub enterprise_profile: Option<serde_json::Value>,
}

pub async fn get(pool: &SqlitePool, openid: &str) -> Result<Option<User>> {
    let row = sqlx::query_as::<_, User>("SELECT * FROM users WHERE openid = ?")
        .bind(openid)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

pub async fn get_required(pool: &SqlitePool, openid: &str) -> Result<User> {
    get(pool, openid)
        .await?
        .ok_or_else(|| Error::UserNotFound(openid.to_string()))
}

pub async fn insert_provisioning(
    pool: &SqlitePool,
    new: &NewUser,
    linux_uid: &str,
    workspace_path: &str,
) -> Result<User> {
    let now = Utc::now();
    let profile_json = new
        .enterprise_profile
        .as_ref()
        .map(|v| v.to_string());

    sqlx::query(
        r#"INSERT INTO users
           (openid, phone, display_name, enterprise_profile, linux_uid, workspace_path,
            port, paired_token_enc, status, created_at, last_active_at)
           VALUES (?, ?, ?, ?, ?, ?, NULL, NULL, 'provisioning', ?, ?)"#,
    )
    .bind(&new.openid)
    .bind(&new.phone)
    .bind(&new.display_name)
    .bind(&profile_json)
    .bind(linux_uid)
    .bind(workspace_path)
    .bind(now)
    .bind(now)
    .execute(pool)
    .await
    .map_err(|e| match e {
        sqlx::Error::Database(db) if db.is_unique_violation() => {
            Error::UserAlreadyExists(new.openid.clone())
        }
        other => Error::Sqlx(other),
    })?;

    get_required(pool, &new.openid).await
}

pub async fn set_status(pool: &SqlitePool, openid: &str, status: &str) -> Result<()> {
    sqlx::query("UPDATE users SET status = ?, last_active_at = ? WHERE openid = ?")
        .bind(status)
        .bind(Utc::now())
        .bind(openid)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn set_port(pool: &SqlitePool, openid: &str, port: Option<u16>) -> Result<()> {
    sqlx::query("UPDATE users SET port = ? WHERE openid = ?")
        .bind(port.map(|p| p as i64))
        .bind(openid)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn set_paired_token(pool: &SqlitePool, openid: &str, token_enc: &str) -> Result<()> {
    sqlx::query("UPDATE users SET paired_token_enc = ? WHERE openid = ?")
        .bind(token_enc)
        .bind(openid)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn touch_active(pool: &SqlitePool, openid: &str) -> Result<()> {
    sqlx::query("UPDATE users SET last_active_at = ? WHERE openid = ?")
        .bind(Utc::now())
        .bind(openid)
        .execute(pool)
        .await?;
    Ok(())
}

/// Patch user profile fields. Any field set to `None` is left unchanged
/// (so callers can update only what they care about).
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ProfilePatch {
    pub display_name: Option<String>,
    pub phone: Option<String>,
    pub enterprise_profile: Option<serde_json::Value>,
}

pub async fn update_profile(
    pool: &SqlitePool,
    openid: &str,
    patch: &ProfilePatch,
) -> Result<()> {
    // Build the dynamic SET clause to only touch fields the caller sent.
    let mut set_parts: Vec<&'static str> = Vec::new();
    if patch.display_name.is_some() {
        set_parts.push("display_name = ?");
    }
    if patch.phone.is_some() {
        set_parts.push("phone = ?");
    }
    if patch.enterprise_profile.is_some() {
        set_parts.push("enterprise_profile = ?");
    }
    if set_parts.is_empty() {
        return Ok(()); // no-op
    }
    set_parts.push("last_active_at = ?");
    let sql = format!(
        "UPDATE users SET {} WHERE openid = ?",
        set_parts.join(", ")
    );
    let mut q = sqlx::query(&sql);
    if let Some(v) = &patch.display_name {
        q = q.bind(v);
    }
    if let Some(v) = &patch.phone {
        q = q.bind(v);
    }
    if let Some(v) = &patch.enterprise_profile {
        q = q.bind(v.to_string());
    }
    q = q.bind(Utc::now()).bind(openid);
    let res = q.execute(pool).await?;
    if res.rows_affected() == 0 {
        return Err(Error::UserNotFound(openid.to_string()));
    }
    Ok(())
}

pub async fn set_error(pool: &SqlitePool, openid: &str, err: &str) -> Result<()> {
    sqlx::query("UPDATE users SET status = 'failed', last_error = ? WHERE openid = ?")
        .bind(err)
        .bind(openid)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn next_linux_uid(pool: &SqlitePool) -> Result<String> {
    let row: Option<(String,)> =
        sqlx::query_as("SELECT linux_uid FROM users ORDER BY created_at DESC LIMIT 1")
            .fetch_optional(pool)
            .await?;
    let next_n = match row {
        Some((uid,)) => uid
            .strip_prefix("claw-")
            .and_then(|n| n.parse::<u32>().ok())
            .map(|n| n + 1)
            .unwrap_or(1),
        None => 1,
    };
    Ok(format!("claw-{next_n:03}"))
}

pub async fn log_step(
    pool: &SqlitePool,
    openid: &str,
    step: &str,
    ok: bool,
    detail: Option<&str>,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO provision_log (openid, step, result, detail, ts) VALUES (?, ?, ?, ?, ?)",
    )
    .bind(openid)
    .bind(step)
    .bind(if ok { "ok" } else { "err" })
    .bind(detail)
    .bind(Utc::now())
    .execute(pool)
    .await?;
    Ok(())
}

impl<'r> sqlx::FromRow<'r, sqlx::sqlite::SqliteRow> for User {
    fn from_row(row: &'r sqlx::sqlite::SqliteRow) -> std::result::Result<Self, sqlx::Error> {
        use sqlx::Row;
        Ok(User {
            openid: row.try_get("openid")?,
            phone: row.try_get("phone")?,
            display_name: row.try_get("display_name")?,
            enterprise_profile: row.try_get("enterprise_profile")?,
            linux_uid: row.try_get("linux_uid")?,
            workspace_path: row.try_get("workspace_path")?,
            port: row.try_get("port")?,
            paired_token_enc: row.try_get("paired_token_enc")?,
            status: row.try_get("status")?,
            created_at: row.try_get("created_at")?,
            last_active_at: row.try_get("last_active_at")?,
            last_error: row.try_get("last_error")?,
        })
    }
}
