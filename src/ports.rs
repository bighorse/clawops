use crate::{Error, Result};
use chrono::Utc;
use sqlx::SqlitePool;
use std::net::TcpListener;

pub struct PortRange {
    pub start: u16,
    pub end: u16,
}

/// Is `port` actually free right now — not merely free according to us?
///
/// `port_allocations` records only the tenants ClawOps provisioned. Anything
/// else on the host — an independently deployed zeroclaw, the shared model
/// gateway, some unrelated service — is invisible to that ledger. Trusting the
/// ledger alone once handed a live process's port to a new tenant: the tenant's
/// own daemon could not bind, so ClawOps' traffic reached the *other* process,
/// which rejected it with 401. The user saw a 500, hours after provisioning had
/// reported success.
///
/// Binding on 127.0.0.1 also detects listeners bound to 0.0.0.0. std sets
/// SO_REUSEADDR on Unix, which permits rebinding a socket in TIME_WAIT but never
/// one with an active listener — exactly the distinction wanted here.
fn is_port_free(port: u16) -> bool {
    TcpListener::bind(("127.0.0.1", port)).is_ok()
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
        if taken.contains(&p) {
            continue;
        }
        if !is_port_free(p) {
            // Unallocated yet occupied: something outside ClawOps is squatting
            // inside the configured range. Skipping keeps provisioning correct,
            // but it remains a misconfiguration someone should fix, so say so
            // rather than paper over it.
            tracing::warn!(
                port = p,
                "port is unallocated but already bound by another process; skipping it"
            );
            continue;
        }
        chosen = Some(p);
        break;
    }
    // Nothing holds the port between this probe and the daemon's own bind, so a
    // race remains possible in principle. The daemon then fails to start loudly,
    // which is still the outcome this check exists to produce.
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

#[cfg(test)]
mod tests {
    use super::*;

    async fn ledger() -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::query(
            "CREATE TABLE port_allocations (
                 port          INTEGER PRIMARY KEY,
                 owner_openid  TEXT NOT NULL,
                 allocated_at  TEXT NOT NULL)",
        )
        .execute(&pool)
        .await
        .unwrap();
        pool
    }

    /// Binds a port and hands it back still held, so a range under test can
    /// start on a port that is genuinely occupied.
    fn occupied_port() -> (TcpListener, u16) {
        let l = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let p = l.local_addr().unwrap().port();
        (l, p)
    }

    /// The 2026-08-16 regression: a port absent from the ledger but held by a
    /// process ClawOps does not manage must never be handed to a tenant.
    #[tokio::test]
    async fn skips_a_port_held_by_a_foreign_process() {
        let pool = ledger().await;
        let (_squatter, occupied) = occupied_port();
        let range = PortRange {
            start: occupied,
            end: occupied.saturating_add(200),
        };

        let got = allocate(&pool, &range, "openid-1").await.unwrap();

        assert_ne!(
            got, occupied,
            "handed out a port another process is listening on"
        );
        assert!(got > occupied, "should have moved past the occupied port");
    }

    /// The probe must not replace the ledger: a port that is physically free but
    /// already booked stays off limits.
    #[tokio::test]
    async fn still_skips_a_port_recorded_in_the_ledger() {
        let pool = ledger().await;
        let booked = {
            let (l, p) = occupied_port();
            drop(l); // now physically free, but we pretend it is allocated
            p
        };
        sqlx::query(
            "INSERT INTO port_allocations (port, owner_openid, allocated_at)
             VALUES (?, 'someone-else', '2026-01-01T00:00:00Z')",
        )
        .bind(booked as i64)
        .execute(&pool)
        .await
        .unwrap();
        let range = PortRange {
            start: booked,
            end: booked.saturating_add(200),
        };

        let got = allocate(&pool, &range, "openid-2").await.unwrap();

        assert_ne!(got, booked);
    }

    /// Before the probe existed this returned `Ok(occupied)`; the whole point is
    /// that an occupied-but-unbooked port now exhausts the range instead.
    #[tokio::test]
    async fn reports_no_free_port_when_the_only_candidate_is_occupied() {
        let pool = ledger().await;
        let (_squatter, occupied) = occupied_port();
        let range = PortRange {
            start: occupied,
            end: occupied,
        };

        assert!(matches!(
            allocate(&pool, &range, "openid-3").await,
            Err(Error::NoFreePort)
        ));
    }
}
