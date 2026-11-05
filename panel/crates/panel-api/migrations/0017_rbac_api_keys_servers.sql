-- D6: remote agents (servers and placement), role-based access control, and
-- API keys.

-- Servers. The panel's own host is the fixed `local` row (nil UUID) and is
-- reached over the agent's Unix socket. A remote server is reached either
-- over mutual TLS (`mtls`, address `host:port`) or over a Unix socket that
-- SSH forwards from the remote host (`ssh_socket`, a path under
-- /run/scontrol/remote/).
CREATE TABLE servers (
    id UUID PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    transport TEXT NOT NULL CHECK (transport IN ('local', 'mtls', 'ssh_socket')),
    address TEXT NOT NULL DEFAULT '',
    -- The certificate the panel signed for this agent. Only fingerprints are
    -- pinned; the agent's key never leaves its host.
    cert_pem TEXT,
    cert_fingerprint TEXT,
    cert_serial TEXT,
    cert_expires_at TIMESTAMPTZ,
    -- During a rotation the previous certificate stays accepted until the
    -- agent presents the new one, or until this time.
    previous_fingerprint TEXT,
    previous_valid_until TIMESTAMPTZ,
    -- Placement: draining servers take no new services; `max_services`
    -- bounds how many services may be placed (NULL = no bound).
    draining BOOLEAN NOT NULL DEFAULT FALSE,
    max_services INTEGER CHECK (max_services IS NULL OR max_services >= 0),
    status TEXT NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'online', 'offline')),
    docker BOOLEAN NOT NULL DEFAULT FALSE,
    agent_version TEXT,
    last_seen_at TIMESTAMPTZ,
    last_error TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

INSERT INTO servers (id, name, transport, status, docker)
VALUES ('00000000-0000-0000-0000-000000000000', 'local', 'local', 'online', TRUE);

-- Every existing service stays on the panel's host.
ALTER TABLE compose_projects
    ADD COLUMN server_id UUID NOT NULL DEFAULT '00000000-0000-0000-0000-000000000000'
        REFERENCES servers(id) ON DELETE RESTRICT;
CREATE INDEX idx_compose_projects_server_id ON compose_projects(server_id);

-- The panel PKI: one CA and the current panel client certificate, keys
-- sealed with SecretBox (AAD = the row's `name`).
CREATE TABLE pki_material (
    name TEXT PRIMARY KEY CHECK (name IN ('ca', 'panel_client')),
    cert_pem TEXT NOT NULL,
    key_sealed BYTEA NOT NULL,
    expires_at TIMESTAMPTZ NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- RBAC. `permissions` is the catalog (kept in step with
-- `panel_core::rbac::Permission` at startup); roles hold permissions;
-- bindings give a user a role globally, in a project, or in one environment.
CREATE TABLE permissions (
    key TEXT PRIMARY KEY,
    description TEXT NOT NULL
);

CREATE TABLE roles (
    id UUID PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    description TEXT NOT NULL DEFAULT '',
    builtin BOOLEAN NOT NULL DEFAULT FALSE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE role_permissions (
    role_id UUID NOT NULL REFERENCES roles(id) ON DELETE CASCADE,
    permission TEXT NOT NULL REFERENCES permissions(key) ON DELETE CASCADE,
    PRIMARY KEY (role_id, permission)
);

CREATE TABLE role_bindings (
    id UUID PRIMARY KEY,
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    role_id UUID NOT NULL REFERENCES roles(id) ON DELETE CASCADE,
    project_id UUID REFERENCES projects(id) ON DELETE CASCADE,
    environment_id UUID REFERENCES environments(id) ON DELETE CASCADE,
    created_by UUID REFERENCES users(id) ON DELETE SET NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- An environment binding names its project too.
    CHECK (environment_id IS NULL OR project_id IS NOT NULL)
);

CREATE UNIQUE INDEX uq_role_bindings_scope ON role_bindings (
    user_id, role_id,
    COALESCE(project_id, '00000000-0000-0000-0000-000000000000'),
    COALESCE(environment_id, '00000000-0000-0000-0000-000000000000')
);
CREATE INDEX idx_role_bindings_user_id ON role_bindings(user_id);

-- API keys. The token is `sck_<prefix>_<secret>`; only the prefix (for
-- lookup) and an Argon2id hash of the secret are stored. `grants` holds the
-- key's own `panel_core::rbac::Grant`s; a request is allowed only when both
-- the key and its owner hold the permission.
CREATE TABLE api_keys (
    id UUID PRIMARY KEY,
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    prefix TEXT NOT NULL UNIQUE,
    secret_hash TEXT NOT NULL,
    grants JSONB NOT NULL,
    expires_at TIMESTAMPTZ,
    last_used_at TIMESTAMPTZ,
    revoked_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX idx_api_keys_user_id ON api_keys(user_id);

-- Per-server observability. Existing rows belong to the local server.
-- Host metrics and alert state are kept per server; container metrics are
-- already per service (and a service is on one server).
ALTER TABLE server_metrics
    ADD COLUMN server_id UUID NOT NULL DEFAULT '00000000-0000-0000-0000-000000000000'
        REFERENCES servers(id) ON DELETE CASCADE;
ALTER TABLE server_metrics DROP CONSTRAINT server_metrics_pkey;
ALTER TABLE server_metrics ADD PRIMARY KEY (server_id, resolution_secs, bucket);

ALTER TABLE metric_alert_state
    ADD COLUMN server_id UUID NOT NULL DEFAULT '00000000-0000-0000-0000-000000000000'
        REFERENCES servers(id) ON DELETE CASCADE;
ALTER TABLE metric_alert_state DROP CONSTRAINT metric_alert_state_pkey;
ALTER TABLE metric_alert_state ADD PRIMARY KEY (server_id, key);

-- One Nginx access-log cursor per server (request statistics share one
-- table: a domain is served by one server).
ALTER TABLE nginx_log_cursor
    ADD COLUMN server_id UUID NOT NULL DEFAULT '00000000-0000-0000-0000-000000000000'
        REFERENCES servers(id) ON DELETE CASCADE;
ALTER TABLE nginx_log_cursor DROP COLUMN id;
ALTER TABLE nginx_log_cursor ADD PRIMARY KEY (server_id);

-- Docker cleanups run on every server; one at a time per server.
ALTER TABLE docker_cleanup_runs
    ADD COLUMN server_id UUID NOT NULL DEFAULT '00000000-0000-0000-0000-000000000000'
        REFERENCES servers(id) ON DELETE CASCADE;
DROP INDEX idx_docker_cleanup_one_running;
CREATE UNIQUE INDEX idx_docker_cleanup_one_running ON docker_cleanup_runs (server_id)
    WHERE status = 'running';
