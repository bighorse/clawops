//! Persistent chat history for the mini-program UI.
//!
//! The zeroclaw daemon already keeps an in-memory `api_chat_history` as
//! the LLM context window for the current process; this module owns the
//! durable copy that the mini-program reads to populate its message list
//! on page load and to scroll-back-load older turns.
//!
//! Failed chats are deliberately not persisted — the user only sees
//! turns that completed end-to-end.

use crate::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub id: i64,
    pub openid: String,
    pub role: String,
    pub content: String,
    pub created_at: DateTime<Utc>,
}

impl<'r> sqlx::FromRow<'r, sqlx::sqlite::SqliteRow> for ChatMessage {
    fn from_row(row: &'r sqlx::sqlite::SqliteRow) -> std::result::Result<Self, sqlx::Error> {
        use sqlx::Row;
        Ok(ChatMessage {
            id: row.try_get("id")?,
            openid: row.try_get("openid")?,
            role: row.try_get("role")?,
            content: row.try_get("content")?,
            created_at: row.try_get("created_at")?,
        })
    }
}

/// Append one user turn and one assistant turn in a single transaction.
/// Caller must ensure both strings are non-empty.
pub async fn record_turn(
    pool: &SqlitePool,
    openid: &str,
    user_content: &str,
    assistant_content: &str,
) -> Result<()> {
    let mut tx = pool.begin().await?;
    let now = Utc::now();
    sqlx::query(
        "INSERT INTO chat_messages (openid, role, content, created_at) VALUES (?, 'user', ?, ?)",
    )
    .bind(openid)
    .bind(user_content)
    .bind(now)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO chat_messages (openid, role, content, created_at) VALUES (?, 'assistant', ?, ?)",
    )
    .bind(openid)
    .bind(assistant_content)
    .bind(now)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Page of messages, newest-first within a page.
///
/// Cursor semantics:
/// - `before_id = None` → most recent `limit` messages
/// - `before_id = Some(N)` → up to `limit` messages with `id < N`
///
/// Returns messages already ordered DESC by id. The caller / front-end
/// typically reverses for display (oldest at top within the page).
pub async fn fetch_page(
    pool: &SqlitePool,
    openid: &str,
    before_id: Option<i64>,
    limit: i64,
) -> Result<Vec<ChatMessage>> {
    let limit = limit.clamp(1, 100);
    let rows: Vec<ChatMessage> = match before_id {
        Some(cursor) => sqlx::query_as(
            "SELECT * FROM chat_messages WHERE openid = ? AND id < ? ORDER BY id DESC LIMIT ?",
        )
        .bind(openid)
        .bind(cursor)
        .bind(limit)
        .fetch_all(pool)
        .await?,
        None => sqlx::query_as(
            "SELECT * FROM chat_messages WHERE openid = ? ORDER BY id DESC LIMIT ?",
        )
        .bind(openid)
        .bind(limit)
        .fetch_all(pool)
        .await?,
    };
    Ok(rows)
}
