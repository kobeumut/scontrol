-- Numbered 0015 because GENE-74 (PR #8) takes 0014.
-- Host and container metrics, sampled in-process by the agent (`/proc` and
-- cgroup v2) and stored by panel-api as a downsampled ring:
--   30 s buckets for 24 hours, 5 min averages for 7 days, 1 h averages for
--   30 days. `services::metrics` rolls up and prunes every few minutes.
CREATE TABLE server_metrics (
    resolution_secs INTEGER NOT NULL CHECK (resolution_secs IN (30, 300, 3600)),
    bucket TIMESTAMPTZ NOT NULL,
    cpu_pct REAL NOT NULL,
    mem_used_bytes BIGINT NOT NULL,
    mem_total_bytes BIGINT NOT NULL,
    load1 REAL NOT NULL,
    disk_used_bytes BIGINT NOT NULL,
    disk_total_bytes BIGINT NOT NULL,
    disk_read_bps DOUBLE PRECISION NOT NULL,
    disk_write_bps DOUBLE PRECISION NOT NULL,
    samples INTEGER NOT NULL DEFAULT 1,
    PRIMARY KEY (resolution_secs, bucket)
);

-- One row per Compose service and bucket (replicas are summed). Only
-- containers of panel-managed projects are stored.
CREATE TABLE container_metrics (
    resolution_secs INTEGER NOT NULL CHECK (resolution_secs IN (30, 300, 3600)),
    bucket TIMESTAMPTZ NOT NULL,
    compose_project_id UUID NOT NULL REFERENCES compose_projects (id) ON DELETE CASCADE,
    service TEXT NOT NULL,
    cpu_pct REAL NOT NULL,
    mem_bytes BIGINT NOT NULL,
    mem_limit_bytes BIGINT,
    io_read_bps DOUBLE PRECISION NOT NULL,
    io_write_bps DOUBLE PRECISION NOT NULL,
    samples INTEGER NOT NULL DEFAULT 1,
    PRIMARY KEY (resolution_secs, compose_project_id, service, bucket)
);

CREATE INDEX idx_container_metrics_bucket ON container_metrics (resolution_secs, bucket);

-- Threshold alert settings (a single row).
CREATE TABLE metric_alert_settings (
    id BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK (id),
    enabled BOOLEAN NOT NULL DEFAULT TRUE,
    cpu_pct REAL NOT NULL DEFAULT 85,
    memory_pct REAL NOT NULL DEFAULT 85,
    disk_pct REAL NOT NULL DEFAULT 85,
    container_memory_pct REAL NOT NULL DEFAULT 90,
    hysteresis_pct REAL NOT NULL DEFAULT 5,
    trigger_samples INTEGER NOT NULL DEFAULT 3,
    clear_samples INTEGER NOT NULL DEFAULT 3,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

INSERT INTO metric_alert_settings (id) VALUES (TRUE);

-- Edge-triggered alert state per watched metric, kept across restarts so a
-- panel-api restart neither repeats nor loses an alert.
-- Keys: `host:cpu`, `host:memory`, `disk:<mount point>`,
-- `container:<project id>:<service>:memory`.
CREATE TABLE metric_alert_state (
    key TEXT PRIMARY KEY,
    firing BOOLEAN NOT NULL DEFAULT FALSE,
    streak INTEGER NOT NULL DEFAULT 0,
    last_value REAL NOT NULL,
    changed_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
