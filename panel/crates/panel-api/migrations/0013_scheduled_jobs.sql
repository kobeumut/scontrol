-- Scheduled jobs: an exec-form command run in a service of a Compose
-- project from a generated systemd timer (`scontrol-job-<id>.timer`).
-- Numbered 0013 so it sorts after the other D3 migrations (database
-- services, backups) developed alongside it.
--
-- The command is an argv array, never a shell string. The schedule is kept
-- as entered (cron or a systemd calendar expression) together with the
-- `OnCalendar=` values it produced, so the UI shows exactly what the timer
-- runs on.
CREATE TABLE job_schedules (
    id UUID PRIMARY KEY,
    compose_project_id UUID NOT NULL REFERENCES compose_projects(id) ON DELETE CASCADE,
    name TEXT NOT NULL CHECK (length(name) BETWEEN 1 AND 64),
    service_name TEXT NOT NULL,
    command TEXT[] NOT NULL CHECK (cardinality(command) BETWEEN 1 AND 64),
    schedule_kind TEXT NOT NULL CHECK (schedule_kind IN ('cron', 'calendar')),
    schedule_expression TEXT NOT NULL,
    timezone TEXT NOT NULL,
    on_calendar TEXT[] NOT NULL,
    timeout_secs INTEGER NOT NULL CHECK (timeout_secs BETWEEN 1 AND 86400),
    enabled BOOLEAN NOT NULL DEFAULT TRUE,
    last_run_at TIMESTAMPTZ,
    last_run_status TEXT CHECK (last_run_status IN ('running', 'succeeded', 'failed')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (compose_project_id, name)
);

-- Runs, copied from the agent's run records. The output itself stays on
-- the agent (masked, at most 5 MiB per run) and is read by offset.
CREATE TABLE job_runs (
    id UUID PRIMARY KEY,
    job_id UUID NOT NULL REFERENCES job_schedules(id) ON DELETE CASCADE,
    trigger TEXT NOT NULL CHECK (trigger IN ('schedule', 'manual')),
    status TEXT NOT NULL CHECK (status IN ('running', 'succeeded', 'failed')),
    started_at TIMESTAMPTZ NOT NULL,
    finished_at TIMESTAMPTZ,
    exit_code INTEGER,
    error_message TEXT,
    log_bytes BIGINT NOT NULL DEFAULT 0 CHECK (log_bytes >= 0),
    log_truncated BOOLEAN NOT NULL DEFAULT FALSE
);

CREATE INDEX idx_job_runs_job_started ON job_runs (job_id, started_at DESC);
CREATE INDEX idx_job_runs_running ON job_runs (job_id) WHERE status = 'running';
