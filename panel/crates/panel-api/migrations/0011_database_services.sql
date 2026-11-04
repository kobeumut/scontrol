-- Managed database services (PostgreSQL, MySQL, MariaDB, MongoDB, Redis).
--
-- A database service is a compose_projects row whose source has
-- kind = 'database'; this table holds what only database services have.
--
-- credentials_ciphertext is the generated password (and MySQL/MariaDB root
-- password) as JSON, AES-256-GCM sealed by panel-api (`SecretBox`) with the
-- project id as associated data. It is decrypted only to send with a
-- deployment of this service, or for the audited credentials endpoint.
--
-- The service publishes its port on 127.0.0.1:<loopback_port> only. Outside
-- access is opt-in: public_port/allow_from describe the host Nginx stream
-- proxy and UFW rule the agent maintains (allow_from empty = any address).
CREATE TABLE database_services (
    project_id UUID PRIMARY KEY REFERENCES compose_projects(id) ON DELETE CASCADE,
    loopback_port INTEGER NOT NULL UNIQUE CHECK (loopback_port BETWEEN 25000 AND 29999),
    credentials_ciphertext BYTEA NOT NULL,
    public_port INTEGER UNIQUE CHECK (public_port BETWEEN 1024 AND 65535),
    allow_from TEXT[] NOT NULL DEFAULT '{}',
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
