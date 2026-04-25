-- ClawOps initial schema.
-- SQLite dialect. Timestamps stored as ISO-8601 strings (chrono DateTime<Utc>).

CREATE TABLE IF NOT EXISTS users (
    openid              TEXT PRIMARY KEY,
    phone               TEXT,
    display_name        TEXT,
    enterprise_profile  TEXT,                     -- JSON blob
    linux_uid           TEXT NOT NULL UNIQUE,     -- e.g. "claw-001"
    workspace_path      TEXT NOT NULL,
    port                INTEGER,                  -- null when stopped/archived
    paired_token_enc    TEXT,                     -- encrypted bearer token
    status              TEXT NOT NULL,            -- provisioning|running|stopped|archived|failed
    created_at          TEXT NOT NULL,
    last_active_at      TEXT NOT NULL,
    last_error          TEXT
);

CREATE INDEX IF NOT EXISTS idx_users_status       ON users(status);
CREATE INDEX IF NOT EXISTS idx_users_last_active  ON users(last_active_at);

CREATE TABLE IF NOT EXISTS port_allocations (
    port          INTEGER PRIMARY KEY,
    owner_openid  TEXT NOT NULL,
    allocated_at  TEXT NOT NULL,
    FOREIGN KEY(owner_openid) REFERENCES users(openid) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS provision_log (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    openid        TEXT NOT NULL,
    step          TEXT NOT NULL,
    result        TEXT NOT NULL,                  -- ok|err
    detail        TEXT,
    ts            TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_provlog_openid ON provision_log(openid);
