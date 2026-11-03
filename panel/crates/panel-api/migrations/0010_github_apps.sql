-- GitHub Apps for private Git access through short-lived installation
-- tokens. panel-api mints a token when a deployment starts (signing a JWT
-- with the App's private key), so unlike deploy keys and HTTPS tokens the
-- private key is stored here, sealed with SecretBox (AES-256-GCM under
-- PANEL_SECRET_KEY, row-bound associated data). It is never returned.
CREATE TABLE github_apps (
    id UUID PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    app_id BIGINT NOT NULL CHECK (app_id > 0),
    private_key_sealed BYTEA NOT NULL,
    created_by UUID REFERENCES users(id) ON DELETE SET NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- A project links one App installation. An App still linked to a project
-- cannot be deleted (the API reports which projects use it).
ALTER TABLE compose_projects
    ADD COLUMN github_app_id UUID REFERENCES github_apps(id) ON DELETE RESTRICT,
    ADD COLUMN github_installation_id BIGINT CHECK (github_installation_id > 0),
    ADD CONSTRAINT compose_projects_github_app_link
        CHECK ((github_app_id IS NULL) = (github_installation_id IS NULL));
