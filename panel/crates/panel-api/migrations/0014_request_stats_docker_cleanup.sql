-- D4: Nginx request statistics and scheduled Docker cleanup.

-- One row per host and UTC minute, aggregated from the Nginx JSON access
-- log. The agent returns partial minutes across polls, so rows are added
-- to (ON CONFLICT ... + EXCLUDED), never replaced.
CREATE TABLE request_stats_minute (
    minute TIMESTAMPTZ NOT NULL,
    host TEXT NOT NULL CHECK (length(host) BETWEEN 1 AND 253),
    requests BIGINT NOT NULL DEFAULT 0,
    status_2xx BIGINT NOT NULL DEFAULT 0,
    status_3xx BIGINT NOT NULL DEFAULT 0,
    status_4xx BIGINT NOT NULL DEFAULT 0,
    status_5xx BIGINT NOT NULL DEFAULT 0,
    status_other BIGINT NOT NULL DEFAULT 0,
    bytes_sent BIGINT NOT NULL DEFAULT 0,
    request_time_ms_sum BIGINT NOT NULL DEFAULT 0,
    PRIMARY KEY (minute, host)
);

CREATE INDEX idx_request_stats_host_minute ON request_stats_minute (host, minute);

-- Where the access log was read up to. Updated in the same transaction as
-- the buckets, so a crash neither loses nor double-counts requests.
CREATE TABLE nginx_log_cursor (
    id SMALLINT PRIMARY KEY CHECK (id = 1),
    file_id BIGINT,
    byte_offset BIGINT NOT NULL DEFAULT 0 CHECK (byte_offset >= 0),
    last_read_at TIMESTAMPTZ,
    last_error TEXT
);
INSERT INTO nginx_log_cursor (id) VALUES (1);

-- Automatic cleanup settings (a single row).
CREATE TABLE docker_cleanup_settings (
    id SMALLINT PRIMARY KEY CHECK (id = 1),
    enabled BOOLEAN NOT NULL DEFAULT FALSE,
    interval_hours INTEGER NOT NULL DEFAULT 24 CHECK (interval_hours IN (6, 12, 24, 72, 168)),
    older_than_hours INTEGER NOT NULL DEFAULT 168 CHECK (older_than_hours BETWEEN 1 AND 8760),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
INSERT INTO docker_cleanup_settings (id) VALUES (1);

CREATE TABLE docker_cleanup_runs (
    id UUID PRIMARY KEY,
    trigger TEXT NOT NULL CHECK (trigger IN ('schedule', 'manual')),
    status TEXT NOT NULL CHECK (status IN ('running', 'succeeded', 'failed')),
    older_than_hours INTEGER NOT NULL,
    started_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    finished_at TIMESTAMPTZ,
    images_freed_bytes BIGINT NOT NULL DEFAULT 0,
    builder_freed_bytes BIGINT NOT NULL DEFAULT 0,
    error_message TEXT
);

CREATE INDEX idx_docker_cleanup_runs_started ON docker_cleanup_runs (started_at DESC);
-- At most one cleanup runs at a time.
CREATE UNIQUE INDEX idx_docker_cleanup_one_running ON docker_cleanup_runs ((TRUE)) WHERE status = 'running';
