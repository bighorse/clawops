-- Per-user chat history persisted by ClawOps. The zeroclaw daemon
-- maintains an in-memory api_chat_history per process for the current
-- LLM context window; this table is the durable record used to:
--   - serve chat-history fetches to the mini-program (paginated)
--   - survive daemon restarts so the user UI shows continuity
--
-- Failed chats are not written. A chat write happens on success (after
-- the daemon returns 200), so user message and assistant message are
-- written together in one transaction.

CREATE TABLE IF NOT EXISTS chat_messages (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    openid      TEXT    NOT NULL,
    role        TEXT    NOT NULL,    -- 'user' | 'assistant'
    content     TEXT    NOT NULL,
    created_at  TEXT    NOT NULL,    -- ISO-8601 UTC
    FOREIGN KEY (openid) REFERENCES users(openid) ON DELETE CASCADE
);

-- Pagination + listing both filter by openid and order by id DESC
-- (latest first); a single composite index covers both.
CREATE INDEX IF NOT EXISTS idx_chat_messages_openid_id
    ON chat_messages (openid, id DESC);
