-- Durable deployment queue and Compose domain bindings.
--
-- compose_deployments doubles as the job queue: a 'queued' row is a pending
-- job that survives a panel-api restart. Rows never hold environment values;
-- those are written to the agent before the job is queued.

ALTER TABLE compose_deployments
    ADD COLUMN queued_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    ADD COLUMN pull_images BOOLEAN NOT NULL DEFAULT FALSE,
    ADD COLUMN trigger TEXT NOT NULL DEFAULT 'manual',
    ADD COLUMN commit_sha TEXT;

-- `started_at` now means "began running"; a queued job has not started.
ALTER TABLE compose_deployments ALTER COLUMN started_at DROP NOT NULL;
ALTER TABLE compose_deployments ALTER COLUMN started_at DROP DEFAULT;

CREATE INDEX idx_compose_deployments_pending
    ON compose_deployments (queued_at)
    WHERE status IN ('queued', 'running');

-- Each binding gets a loopback host port that the agent publishes the
-- service on and the generated Nginx vhost proxies to. No API wrote this
-- table before this migration, so the NOT NULL columns need no backfill.
ALTER TABLE compose_project_domains RENAME COLUMN upstream_port TO container_port;
ALTER TABLE compose_project_domains RENAME COLUMN ssl_enabled TO https;
ALTER TABLE compose_project_domains
    ADD COLUMN host_port INTEGER NOT NULL UNIQUE CHECK (host_port BETWEEN 40000 AND 49999);
