-- Private Docker registries (Docker Hub, ghcr.io, ECR, GitLab, self-hosted).
--
-- The password or access token is AES-256-GCM sealed by panel-api
-- (`services/secret_box.rs`, key derived from PANEL_SECRET_KEY) with the
-- registry id as associated data, so a database dump does not reveal it and
-- a ciphertext cannot be moved to another row. Nothing else in this table
-- is secret. It is decrypted only to hand to the agent right before a
-- deployment, which writes a 0600 `DOCKER_CONFIG/config.json`.
CREATE TABLE docker_registries (
    id UUID PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    server_url TEXT NOT NULL,
    username TEXT NOT NULL,
    credential_ciphertext BYTEA NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- The registry a project pulls its images with. Deleting a registry leaves
-- its projects in place; they pull anonymously again.
ALTER TABLE compose_projects
    ADD COLUMN registry_id UUID REFERENCES docker_registries(id) ON DELETE SET NULL;
