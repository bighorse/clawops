-- Activity reminders: created by bot via POST /notify/subscribe after
-- the user grants wx.requestSubscribeMessage consent in the mini-program.
-- A background job scans this table and calls WeChat subscribeMessage.send.
CREATE TABLE IF NOT EXISTS reminders (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    openid        TEXT    NOT NULL,
    activity_id   TEXT    NOT NULL,
    activity_name TEXT    NOT NULL,
    activity_time TEXT    NOT NULL,   -- ISO-8601, shown in the notification
    activity_venue TEXT   NOT NULL DEFAULT '',
    remind_at     TEXT    NOT NULL,   -- ISO-8601, when to send
    sent_at       TEXT,               -- NULL = pending
    failed_at     TEXT,               -- NULL = not failed
    fail_reason   TEXT,
    created_at    TEXT    NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX IF NOT EXISTS idx_reminders_pending
    ON reminders (remind_at) WHERE sent_at IS NULL AND failed_at IS NULL;
