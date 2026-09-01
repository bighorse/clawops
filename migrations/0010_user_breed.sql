-- Per-tenant breed (品种) binding.
--
-- Until now one ClawOps deployment rendered every tenant from one
-- `provisioner.template_dir`, so serving a second kind of lobster meant
-- forking the whole repo (see branch `tenant/zhongbolun`) onto its own
-- server. This column lets one swarm host several breeds side by side:
-- the provisioner resolves the template directory from the tenant's
-- breed instead of a single global path.
--
-- 'default' maps back to `provisioner.template_dir`, so every existing
-- row keeps rendering from exactly the files it rendered from before.
--
-- Numbered 0010 on purpose: branch `tenant/zhongbolun` already carries a
-- 0009 (`0009_task_artifact.sql`). Two different 0009s would collide the
-- day that branch is folded back onto main — which is the whole point of
-- breeds.
ALTER TABLE users ADD COLUMN breed TEXT NOT NULL DEFAULT 'default';

CREATE INDEX IF NOT EXISTS idx_users_breed ON users(breed);
