-- Qualification reminders: zeroclaw SOP writes patent/trademark/cert
-- expiry events here; background cron sends SMS (wx users) or webhook
-- (wecom uin: users) at remind_at.
CREATE TABLE IF NOT EXISTS qualification_reminders (
    id                          INTEGER PRIMARY KEY AUTOINCREMENT,
    openid                      TEXT    NOT NULL,
    enterprise_name             TEXT    NOT NULL,
    qualification_enterprise_id INTEGER,
    -- PATENT_FEE | PATENT_ANNUAL | TRADEMARK_RENEWAL | CERT_RENEWAL | HONOR_RENEWAL
    reminder_type               TEXT    NOT NULL,
    qualification_title         TEXT    NOT NULL,
    reg_no                      TEXT,
    remind_at                   TEXT    NOT NULL,   -- ISO-8601 fire time
    event_date                  TEXT    NOT NULL,   -- actual expiry / deadline
    sent_at                     TEXT,
    failed_at                   TEXT,
    fail_reason                 TEXT,
    created_at                  TEXT    NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX IF NOT EXISTS idx_qual_rem_openid    ON qualification_reminders(openid);
CREATE INDEX IF NOT EXISTS idx_qual_rem_remind_at ON qualification_reminders(remind_at);
CREATE INDEX IF NOT EXISTS idx_qual_rem_pending   ON qualification_reminders(remind_at)
    WHERE sent_at IS NULL AND failed_at IS NULL;
