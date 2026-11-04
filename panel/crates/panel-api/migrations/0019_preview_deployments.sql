-- Pull request preview deployments (D5).
--
-- A Git-sourced Compose project can opt in to previews: each open pull
-- (merge) request from the same repository gets a temporary Compose project
-- `<slug>-pr-<n>` served on `<slug>-pr-<n>.<base_domain>`. Closing or
-- merging the request, an operator, or the TTL sweeper removes it again.
--
-- `comment_token_sealed` is an optional write-only API token used to post
-- the preview link on the pull request when the project has no GitHub App
-- installation. It is sealed with SecretBox (AAD binds the project id) and
-- never returned.

CREATE TABLE preview_settings (
    project_id UUID PRIMARY KEY REFERENCES compose_projects(id) ON DELETE CASCADE,
    enabled BOOLEAN NOT NULL DEFAULT FALSE,
    base_domain TEXT NOT NULL,
    service_name TEXT NOT NULL,
    container_port INTEGER NOT NULL CHECK (container_port BETWEEN 1 AND 65535),
    https BOOLEAN NOT NULL DEFAULT FALSE,
    -- certbot lineage of a wildcard certificate (`*.base_domain`) issued
    -- out of band; NULL requests one certificate per preview host instead.
    wildcard_certificate TEXT,
    max_active INTEGER NOT NULL DEFAULT 5 CHECK (max_active BETWEEN 1 AND 50),
    ttl_hours INTEGER NOT NULL DEFAULT 48 CHECK (ttl_hours BETWEEN 1 AND 720),
    memory_mb INTEGER NOT NULL DEFAULT 512 CHECK (memory_mb BETWEEN 64 AND 65536),
    cpu_millis INTEGER NOT NULL DEFAULT 500 CHECK (cpu_millis BETWEEN 100 AND 64000),
    comment_token_sealed BYTEA,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- One row per previewed pull request. The preview's Compose project row
-- owns the row: deleting it (teardown) deletes the preview record.
CREATE TABLE preview_deployments (
    id UUID PRIMARY KEY,
    parent_project_id UUID NOT NULL REFERENCES compose_projects(id) ON DELETE CASCADE,
    preview_project_id UUID NOT NULL UNIQUE REFERENCES compose_projects(id) ON DELETE CASCADE,
    provider TEXT NOT NULL CHECK (provider IN ('github', 'gitlab', 'gitea')),
    pr_number BIGINT NOT NULL CHECK (pr_number BETWEEN 1 AND 9999999),
    head_branch TEXT NOT NULL,
    head_sha TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'active' CHECK (status IN ('active', 'closing')),
    -- closed, merged, expired, removed, disabled
    close_reason TEXT,
    -- Comment (GitHub/Gitea) or note (GitLab) holding the preview link,
    -- edited in place on every deployment.
    comment_id BIGINT,
    last_activity_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (parent_project_id, pr_number)
);

CREATE INDEX idx_preview_deployments_status ON preview_deployments (status, last_activity_at);
