-- Dokploy-inspired Docker deployment model. Secrets/environment values are
-- intentionally absent: they are write-only inputs materialised by the agent
-- in a 0600 per-project .env file, never stored in PostgreSQL.

CREATE TABLE compose_projects (
    id UUID PRIMARY KEY,
    name TEXT NOT NULL,
    slug TEXT NOT NULL UNIQUE,
    owner_user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    source JSONB NOT NULL,
    status TEXT NOT NULL DEFAULT 'queued',
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX idx_compose_projects_owner_user_id ON compose_projects(owner_user_id);

CREATE TABLE compose_deployments (
    id UUID PRIMARY KEY,
    project_id UUID NOT NULL REFERENCES compose_projects(id) ON DELETE CASCADE,
    status TEXT NOT NULL DEFAULT 'queued',
    log_path TEXT NOT NULL,
    started_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    finished_at TIMESTAMPTZ,
    error_message TEXT
);

CREATE INDEX idx_compose_deployments_project_started_at
    ON compose_deployments(project_id, started_at DESC);

CREATE TABLE compose_project_domains (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    project_id UUID NOT NULL REFERENCES compose_projects(id) ON DELETE CASCADE,
    domain TEXT NOT NULL UNIQUE,
    service_name TEXT NOT NULL,
    upstream_port INTEGER NOT NULL CHECK (upstream_port BETWEEN 1 AND 65535),
    ssl_enabled BOOLEAN NOT NULL DEFAULT FALSE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
