-- SOP async task tracking.
-- Created when user message hits a SOP intent (e.g. policy-match) via
-- the LLM classifier in chat handler. Daemon runs the SOP in the
-- background (~6-10 min); status transitions pending -> running -> done|failed.
-- Cache hit short-circuit: re-insert with status=done copying deeplink
-- from the most recent done task within ttl, no new daemon run.
--
-- Frontend pulls /me/sop/tasks and listens to SSE 'sop_task' events to
-- refresh. 30-day retention (API filters older out; rows kept for ops
-- introspection).

CREATE TABLE IF NOT EXISTS sop_tasks (
    task_id                       TEXT    PRIMARY KEY,           -- ULID with 'tsk_' prefix
    openid                        TEXT    NOT NULL,
    sop_name                      TEXT    NOT NULL,              -- e.g. 'policy-match'
    enterprise_name               TEXT,                          -- best-effort, copied from user input or profile
    enterprise_id                 INTEGER,                       -- filled after step 1 of SOP (from enterprise_profile_sync.data.enterprise_id)
    qualification_enterprise_id   INTEGER,                       -- used to build deeplink (/pages/recommendation/index?id={qid})
    status                        TEXT    NOT NULL,              -- pending | running | done | failed
    deeplink                      TEXT,                          -- mini-program deeplink, populated when done
    response_text                 TEXT,                          -- raw LLM response (for cache lookup hit, also reused)
    error_message                 TEXT,                          -- human-readable, populated when failed
    estimated_seconds             INTEGER NOT NULL,              -- from sop_metadata at creation time
    created_at                    TEXT    NOT NULL,              -- ISO-8601 UTC
    updated_at                    TEXT    NOT NULL,              -- ISO-8601 UTC
    completed_at                  TEXT,                          -- ISO-8601 UTC, populated when done/failed
    FOREIGN KEY (openid) REFERENCES users(openid) ON DELETE CASCADE
);

-- Per-user task list query: WHERE openid=? AND created_at > 30d ago ORDER BY created_at DESC
CREATE INDEX IF NOT EXISTS idx_sop_tasks_openid_created
    ON sop_tasks (openid, created_at DESC);

-- Cache lookup: WHERE openid=? AND sop_name=? AND enterprise_id=? AND status='done' AND created_at > ttl
-- (per-openid cache; same enterprise across different openids doesn't share cache for now)
CREATE INDEX IF NOT EXISTS idx_sop_tasks_cache
    ON sop_tasks (openid, sop_name, enterprise_id, status, created_at DESC);
