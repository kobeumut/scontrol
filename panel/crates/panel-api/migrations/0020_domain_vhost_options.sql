-- Typed vhost options of a project domain (redirect, path prefix, basic auth
-- and custom certificate flags). Only flags and non-secret settings live
-- here: password hashes and certificate keys stay in 0600 files on the agent.
CREATE TABLE compose_domain_options (
    domain_id UUID PRIMARY KEY REFERENCES compose_project_domains(id) ON DELETE CASCADE,
    options JSONB NOT NULL
);
