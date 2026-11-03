-- Private Git access for Compose projects.
--
-- Neither column holds a secret. The deploy key's private half and the
-- HTTPS token live only on the agent (0600 files in the project directory).
-- `deploy_public_key` is shown to the operator to register on the Git host;
-- `git_token_host` records which host a write-only token was saved for
-- (NULL means no token).
ALTER TABLE compose_projects
    ADD COLUMN deploy_public_key TEXT,
    ADD COLUMN git_token_host TEXT;
