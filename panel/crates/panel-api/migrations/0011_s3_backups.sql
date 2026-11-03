-- S3-compatible backup destinations and the history of backup/restore runs.
--
-- The access key and secret key are sealed together (JSON, AES-256-GCM via
-- `services/secret_box.rs`, AAD = destination id), so a database dump does
-- not reveal them. They are decrypted only to hand to the agent for a run.
-- Database passwords are never stored: a run's caller supplies them.
CREATE TABLE backup_destinations (
    id UUID PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    endpoint TEXT NOT NULL,
    region TEXT NOT NULL,
    bucket TEXT NOT NULL,
    -- Key prefix inside the bucket, e.g. `scontrol/server1`.
    prefix TEXT NOT NULL,
    -- Backups kept per database or volume; 0 keeps everything.
    retention_count INTEGER NOT NULL DEFAULT 7 CHECK (retention_count >= 0),
    credential_ciphertext BYTEA NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE backup_runs (
    id UUID PRIMARY KEY,
    destination_id UUID NOT NULL REFERENCES backup_destinations(id) ON DELETE CASCADE,
    -- database_backup | database_restore | volume_backup | volume_restore
    kind TEXT NOT NULL,
    -- Database name, or `<project>/<volume>`.
    source TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'running',
    object_key TEXT,
    size_bytes BIGINT,
    pruned INTEGER NOT NULL DEFAULT 0,
    error TEXT,
    started_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    finished_at TIMESTAMPTZ
);

CREATE INDEX backup_runs_destination_started ON backup_runs (destination_id, started_at DESC);
