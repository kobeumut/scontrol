-- Projects → environments → services (Compose projects), with layered
-- environment variables merged project → environment → service at deploy
-- time (the most specific layer wins).
--
-- Variable values are secrets. They are stored only sealed with SecretBox
-- (AES-256-GCM under PANEL_SECRET_KEY, row-bound associated data), never in
-- plain text, and the API reports key names only. The merged set reaches
-- the agent through `write_compose_environment` right before a deployment;
-- queue rows never carry it.

CREATE TABLE projects (
    id UUID PRIMARY KEY,
    name TEXT NOT NULL,
    slug TEXT NOT NULL,
    owner_user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (owner_user_id, slug)
);

CREATE TABLE environments (
    id UUID PRIMARY KEY,
    project_id UUID NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    slug TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (project_id, slug)
);

-- Backward compatibility: every owner of existing Compose projects gets a
-- "Default" project with a "production" environment holding them.
INSERT INTO projects (id, name, slug, owner_user_id)
SELECT gen_random_uuid(), 'Default', 'default', owner_user_id
FROM (SELECT DISTINCT owner_user_id FROM compose_projects) owners;

INSERT INTO environments (id, project_id, name, slug)
SELECT gen_random_uuid(), id, 'production', 'production' FROM projects;

ALTER TABLE compose_projects
    ADD COLUMN environment_id UUID REFERENCES environments(id) ON DELETE RESTRICT,
    -- FALSE for services created before layering: their own variables were
    -- written straight to the agent and are unknown here, so a deployment
    -- leaves their env file alone until the service environment is saved
    -- again. TRUE once the panel holds the service layer.
    ADD COLUMN env_layered BOOLEAN NOT NULL DEFAULT FALSE;

UPDATE compose_projects c
SET environment_id = e.id
FROM environments e
JOIN projects p ON p.id = e.project_id
WHERE p.owner_user_id = c.owner_user_id AND p.slug = 'default' AND e.slug = 'production';

ALTER TABLE compose_projects ALTER COLUMN environment_id SET NOT NULL;
CREATE INDEX idx_compose_projects_environment_id ON compose_projects(environment_id);

-- One row per variable of one layer. Exactly one scope column is set.
CREATE TABLE env_variables (
    id UUID PRIMARY KEY,
    project_id UUID REFERENCES projects(id) ON DELETE CASCADE,
    environment_id UUID REFERENCES environments(id) ON DELETE CASCADE,
    service_id UUID REFERENCES compose_projects(id) ON DELETE CASCADE,
    key TEXT NOT NULL,
    -- nonce || AES-256-GCM ciphertext; AAD binds scope, scope id, and key.
    value_sealed BYTEA NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (num_nonnulls(project_id, environment_id, service_id) = 1)
);

CREATE UNIQUE INDEX uq_env_variables_project ON env_variables(project_id, key)
    WHERE project_id IS NOT NULL;
CREATE UNIQUE INDEX uq_env_variables_environment ON env_variables(environment_id, key)
    WHERE environment_id IS NOT NULL;
CREATE UNIQUE INDEX uq_env_variables_service ON env_variables(service_id, key)
    WHERE service_id IS NOT NULL;
