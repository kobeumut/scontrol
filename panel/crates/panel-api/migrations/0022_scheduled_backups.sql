-- Scheduled backups of managed database services and Compose volumes.
--
-- panel-api runs these itself (`services/backup_schedules.rs`) instead of a
-- systemd timer on the agent: a run needs the destination's S3 keys and the
-- database service's password, and both stay sealed in panel-api. A timer
-- on the agent would need them stored on the agent host.
--
-- Nothing secret is stored here. The database connection (engine, database,
-- user, loopback port, sealed password) and the server are read from the
-- service when a run starts, so a schedule follows its service.
CREATE TABLE backup_schedules (
    id UUID PRIMARY KEY,
    name TEXT NOT NULL CHECK (length(name) BETWEEN 1 AND 100),
    target_kind TEXT NOT NULL CHECK (target_kind IN ('database', 'volume')),
    compose_project_id UUID NOT NULL REFERENCES compose_projects(id) ON DELETE CASCADE,
    -- Volume only: the named volume, the service using it, and whether that
    -- service is stopped during the copy.
    volume_name TEXT,
    service_name TEXT,
    stop_service BOOLEAN NOT NULL DEFAULT FALSE,
    s3_destination_id UUID NOT NULL REFERENCES backup_destinations(id) ON DELETE CASCADE,
    -- Five-field cron expression or macro, read in `timezone`.
    cron_expression TEXT NOT NULL,
    timezone TEXT NOT NULL DEFAULT 'UTC',
    -- Backups of this series kept after each successful run; 0 keeps all.
    retention_count INTEGER NOT NULL CHECK (retention_count BETWEEN 0 AND 1000),
    enabled BOOLEAN NOT NULL DEFAULT TRUE,
    -- When the scheduler starts the next run; NULL while disabled.
    next_run_at TIMESTAMPTZ,
    last_run_at TIMESTAMPTZ,
    last_run_id UUID,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (
        (target_kind = 'database' AND volume_name IS NULL AND service_name IS NULL
            AND NOT stop_service)
        OR (target_kind = 'volume' AND volume_name IS NOT NULL
            AND (service_name IS NOT NULL OR NOT stop_service))
    ),
    UNIQUE (compose_project_id, name)
);

CREATE INDEX backup_schedules_due ON backup_schedules (next_run_at) WHERE enabled;

-- Who started a run: a user (`manual`) or a schedule.
ALTER TABLE backup_runs
    ADD COLUMN trigger TEXT NOT NULL DEFAULT 'manual' CHECK (trigger IN ('manual', 'schedule')),
    ADD COLUMN schedule_id UUID REFERENCES backup_schedules(id) ON DELETE SET NULL;

CREATE INDEX backup_runs_schedule_started ON backup_runs (schedule_id, started_at DESC)
    WHERE schedule_id IS NOT NULL;
