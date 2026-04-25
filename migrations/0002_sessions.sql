-- Sessions: ClawOps issues opaque bearer tokens after a successful
-- WeChat code2session exchange (or mock login in dev). One row per
-- active session; expired rows are pruned by Reaper later.

CREATE TABLE IF NOT EXISTS sessions (
    token       TEXT PRIMARY KEY,
    openid      TEXT NOT NULL,
    issued_at   TEXT NOT NULL,
    expires_at  TEXT NOT NULL,
    user_agent  TEXT,
    FOREIGN KEY(openid) REFERENCES users(openid) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_sessions_openid     ON sessions(openid);
CREATE INDEX IF NOT EXISTS idx_sessions_expires_at ON sessions(expires_at);
