-- Rollbacks (D5).
--
-- A successful Compose deployment records a snapshot of its release: the
-- deployed Compose file with every environment value and known secret
-- removed, the generated override, environment key names, and the image
-- tags (`<slug>/<service>:deploy-<id>`) the agent pinned. It never holds a
-- credential; a rollback reads the project's current environment file.
--
-- `release_retained` mirrors whether the agent still keeps the release (it
-- keeps the newest few and prunes the rest with their tags). A rollback is
-- itself a queued deployment that points at its target via `rollback_of`.

ALTER TABLE compose_deployments
    ADD COLUMN snapshot JSONB,
    ADD COLUMN release_retained BOOLEAN NOT NULL DEFAULT FALSE,
    ADD COLUMN rollback_of UUID REFERENCES compose_deployments(id) ON DELETE SET NULL;

CREATE INDEX idx_compose_deployments_retained
    ON compose_deployments (project_id)
    WHERE release_retained;
