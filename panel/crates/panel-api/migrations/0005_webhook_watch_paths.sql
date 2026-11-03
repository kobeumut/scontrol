-- Webhook watch paths: repository-relative glob patterns. A push deploys
-- only when one of its changed files matches; an empty list deploys every
-- push to the branch. Patterns are validated by panel-api before storage.
ALTER TABLE compose_projects
    ADD COLUMN webhook_watch_paths TEXT[] NOT NULL DEFAULT '{}';
