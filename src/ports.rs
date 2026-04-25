use crate::{Error, Result};
use chrono::Utc;
use sqlx::SqlitePool;

pub struct PortRange {
    pub start: u16,
    pub end: u16,
}

pub async fn allocate(pool: &SqlitePool, range: &PortRange, openid: &str) -> Result<u16> {
    let mut tx = pool.begin().await?;

    let taken: Vec<(i64,)> = sqlx::query_as("SELECT port FROM port_allocations ORDER BY port")
        .fetch_all(&mut *tx)
        .await?;
    let taken: std::collections::BTreeSet<u16> =
        taken.into_iter().map(|(p,)| p as u16).collect();

    let mut chosen: Option<u16> = None;
    for p in range.start..=range.end {
        if !taken.contains(&p) {
            chosen = Some(p);
            break;
        }
    }
    let port = chosen.ok_or(Error::NoFreePort)?;

    sqlx::query(
        "INSERT INTO port_allocations (port, owner_openid, allocated_at) VALUES (?, ?, ?)",
    )
    .bind(port as i64)
    .bind(openid)
    .bind(Utc::now())
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(port)
}

pub async fn release(pool: &SqlitePool, port: u16) -> Result<()> {
    sqlx::query("DELETE FROM port_allocations WHERE port = ?")
        .bind(port as i64)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn release_for_user(pool: &SqlitePool, openid: &str) -> Result<()> {
    sqlx::query("DELETE FROM port_allocations WHERE owner_openid = ?")
        .bind(openid)
        .execute(pool)
        .await?;
    Ok(())
}
