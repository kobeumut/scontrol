//! Request statistics and Docker cleanup storage. Runtime-checked queries
//! (no offline metadata), like the backup tables.

use chrono::{DateTime, Utc};
use panel_core::observability::RequestBucket;
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgPool};
use uuid::Uuid;

#[derive(Debug, Clone, FromRow)]
pub struct Cursor {
    pub file_id: Option<i64>,
    pub byte_offset: i64,
}

/// The log cursor of `server`, created at offset 0 on first use.
pub async fn cursor(pool: &PgPool, server: Uuid) -> sqlx::Result<Cursor> {
    sqlx::query("INSERT INTO nginx_log_cursor (server_id) VALUES ($1) ON CONFLICT DO NOTHING")
        .bind(server)
        .execute(pool)
        .await?;
    sqlx::query_as("SELECT file_id, byte_offset FROM nginx_log_cursor WHERE server_id = $1")
        .bind(server)
        .fetch_one(pool)
        .await
}

/// Adds the buckets and moves the cursor in one transaction.
pub async fn apply_chunk(
    pool: &PgPool,
    server: Uuid,
    buckets: &[RequestBucket],
    file_id: Option<u64>,
    next_offset: u64,
) -> sqlx::Result<()> {
    let mut tx = pool.begin().await?;
    for b in buckets {
        sqlx::query(
            "INSERT INTO request_stats_minute
                (minute, host, requests, status_2xx, status_3xx, status_4xx, status_5xx,
                 status_other, bytes_sent, request_time_ms_sum)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
             ON CONFLICT (minute, host) DO UPDATE SET
                requests = request_stats_minute.requests + EXCLUDED.requests,
                status_2xx = request_stats_minute.status_2xx + EXCLUDED.status_2xx,
                status_3xx = request_stats_minute.status_3xx + EXCLUDED.status_3xx,
                status_4xx = request_stats_minute.status_4xx + EXCLUDED.status_4xx,
                status_5xx = request_stats_minute.status_5xx + EXCLUDED.status_5xx,
                status_other = request_stats_minute.status_other + EXCLUDED.status_other,
                bytes_sent = request_stats_minute.bytes_sent + EXCLUDED.bytes_sent,
                request_time_ms_sum =
                    request_stats_minute.request_time_ms_sum + EXCLUDED.request_time_ms_sum",
        )
        .bind(b.minute)
        .bind(&b.host)
        .bind(b.requests as i64)
        .bind(b.status_2xx as i64)
        .bind(b.status_3xx as i64)
        .bind(b.status_4xx as i64)
        .bind(b.status_5xx as i64)
        .bind(b.status_other as i64)
        .bind(b.bytes_sent as i64)
        .bind(b.request_time_ms_sum as i64)
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query(
        "UPDATE nginx_log_cursor
         SET file_id = $1, byte_offset = $2, last_read_at = now(), last_error = NULL
         WHERE server_id = $3",
    )
    .bind(file_id.map(|v| v as i64))
    .bind(next_offset as i64)
    .bind(server)
    .execute(&mut *tx)
    .await?;
    tx.commit().await
}

pub async fn set_cursor_error(pool: &PgPool, server: Uuid, error: &str) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO nginx_log_cursor (server_id, last_error) VALUES ($2, $1)
         ON CONFLICT (server_id) DO UPDATE SET last_error = EXCLUDED.last_error",
    )
    .bind(error)
    .bind(server)
    .execute(pool)
    .await?;
    Ok(())
}

#[derive(Debug, Clone, Serialize, FromRow)]
pub struct CursorStatus {
    pub server_id: Uuid,
    pub server_name: String,
    pub last_read_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
}

/// How reading each server's log went (servers never read are absent).
pub async fn cursor_statuses(pool: &PgPool) -> sqlx::Result<Vec<CursorStatus>> {
    sqlx::query_as(
        "SELECT c.server_id, s.name AS server_name, c.last_read_at, c.last_error
         FROM nginx_log_cursor c JOIN servers s ON s.id = c.server_id
         ORDER BY (c.server_id <> '00000000-0000-0000-0000-000000000000'), s.name",
    )
    .fetch_all(pool)
    .await
}

pub async fn prune_stats(pool: &PgPool, keep_days: i32) -> sqlx::Result<u64> {
    Ok(sqlx::query(
        "DELETE FROM request_stats_minute WHERE minute < now() - make_interval(days => $1)",
    )
    .bind(keep_days)
    .execute(pool)
    .await?
    .rows_affected())
}

#[derive(Debug, Clone, Serialize, FromRow)]
pub struct HostTotals {
    pub host: String,
    pub requests: i64,
    pub status_2xx: i64,
    pub status_3xx: i64,
    pub status_4xx: i64,
    pub status_5xx: i64,
    pub status_other: i64,
    pub bytes_sent: i64,
    pub avg_latency_ms: f64,
}

pub async fn host_totals(pool: &PgPool, since: DateTime<Utc>) -> sqlx::Result<Vec<HostTotals>> {
    sqlx::query_as(
        "SELECT host,
                SUM(requests)::BIGINT AS requests,
                SUM(status_2xx)::BIGINT AS status_2xx,
                SUM(status_3xx)::BIGINT AS status_3xx,
                SUM(status_4xx)::BIGINT AS status_4xx,
                SUM(status_5xx)::BIGINT AS status_5xx,
                SUM(status_other)::BIGINT AS status_other,
                SUM(bytes_sent)::BIGINT AS bytes_sent,
                COALESCE(SUM(request_time_ms_sum)::FLOAT8 / NULLIF(SUM(requests), 0), 0)::FLOAT8
                    AS avg_latency_ms
         FROM request_stats_minute WHERE minute >= $1
         GROUP BY host ORDER BY SUM(requests) DESC, host LIMIT 500",
    )
    .bind(since)
    .fetch_all(pool)
    .await
}

#[derive(Debug, Clone, Serialize, FromRow)]
pub struct SeriesPoint {
    pub minute: DateTime<Utc>,
    pub requests: i64,
    pub status_2xx: i64,
    pub status_3xx: i64,
    pub status_4xx: i64,
    pub status_5xx: i64,
    pub bytes_sent: i64,
    pub avg_latency_ms: f64,
}

/// Points of `step_minutes` width, for one host or (with `None`) all hosts.
pub async fn series(
    pool: &PgPool,
    since: DateTime<Utc>,
    step_minutes: i32,
    host: Option<&str>,
) -> sqlx::Result<Vec<SeriesPoint>> {
    sqlx::query_as(
        "SELECT date_bin(make_interval(mins => $2), minute, TIMESTAMPTZ '2000-01-01') AS minute,
                SUM(requests)::BIGINT AS requests,
                SUM(status_2xx)::BIGINT AS status_2xx,
                SUM(status_3xx)::BIGINT AS status_3xx,
                SUM(status_4xx)::BIGINT AS status_4xx,
                SUM(status_5xx)::BIGINT AS status_5xx,
                SUM(bytes_sent)::BIGINT AS bytes_sent,
                COALESCE(SUM(request_time_ms_sum)::FLOAT8 / NULLIF(SUM(requests), 0), 0)::FLOAT8
                    AS avg_latency_ms
         FROM request_stats_minute
         WHERE minute >= $1 AND ($3::TEXT IS NULL OR host = $3)
         GROUP BY 1 ORDER BY 1",
    )
    .bind(since)
    .bind(step_minutes)
    .bind(host)
    .fetch_all(pool)
    .await
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct CleanupSettings {
    pub enabled: bool,
    pub interval_hours: i32,
    pub older_than_hours: i32,
}

pub async fn cleanup_settings(pool: &PgPool) -> sqlx::Result<CleanupSettings> {
    sqlx::query_as(
        "SELECT enabled, interval_hours, older_than_hours FROM docker_cleanup_settings WHERE id = 1",
    )
    .fetch_one(pool)
    .await
}

pub async fn save_cleanup_settings(
    pool: &PgPool,
    s: &CleanupSettings,
) -> sqlx::Result<CleanupSettings> {
    sqlx::query_as(
        "UPDATE docker_cleanup_settings
         SET enabled = $1, interval_hours = $2, older_than_hours = $3, updated_at = now()
         WHERE id = 1
         RETURNING enabled, interval_hours, older_than_hours",
    )
    .bind(s.enabled)
    .bind(s.interval_hours)
    .bind(s.older_than_hours)
    .fetch_one(pool)
    .await
}

#[derive(Debug, Clone, Serialize, FromRow)]
pub struct CleanupRun {
    pub id: Uuid,
    pub server_id: Uuid,
    pub trigger: String,
    pub status: String,
    pub older_than_hours: i32,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub images_freed_bytes: i64,
    pub builder_freed_bytes: i64,
    pub error_message: Option<String>,
}

const RUN_COLUMNS: &str =
    "id, server_id, trigger, status, older_than_hours, started_at, finished_at,
    images_freed_bytes, builder_freed_bytes, error_message";

pub async fn recent_runs(pool: &PgPool, limit: i64) -> sqlx::Result<Vec<CleanupRun>> {
    sqlx::query_as(&format!(
        "SELECT {RUN_COLUMNS} FROM docker_cleanup_runs ORDER BY started_at DESC LIMIT $1"
    ))
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// A `running` cleanup older than this is treated as lost: it no longer
/// holds back deployments, and the next start on its server marks it
/// failed. Well above `services::observability::CLEANUP_TIMEOUT`, so only
/// a run whose panel-api died or whose result could not be stored is hit.
pub const CLEANUP_STALE_SECS: i32 = 60 * 60;

/// What [`start_run`] did.
#[derive(Debug)]
pub enum StartRun {
    Started(CleanupRun),
    /// Another cleanup is running on the server.
    CleanupRunning,
    /// Deployments are queued or running on the server.
    DeploymentsActive(i64),
}

/// Starts a run row unless the server is busy. Runs under the deployment
/// claim lock (`db::compose_projects::lock_deploy_claims`): a claim refuses
/// a server with a running cleanup, and this refuses a server with a queued
/// or running deployment, so a prune never overlaps a pull or build there.
pub async fn start_run(
    pool: &PgPool,
    server: Uuid,
    trigger: &str,
    older_than_hours: i32,
) -> sqlx::Result<StartRun> {
    let mut tx = pool.begin().await?;
    super::compose_projects::lock_deploy_claims(&mut tx).await?;
    sqlx::query(
        "UPDATE docker_cleanup_runs SET status = 'failed', finished_at = now(),
                error_message = 'no result within the time limit'
         WHERE server_id = $1 AND status = 'running'
           AND started_at < now() - $2::int * interval '1 second'",
    )
    .bind(server)
    .bind(CLEANUP_STALE_SECS)
    .execute(&mut *tx)
    .await?;
    let active: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM compose_deployments d
         JOIN compose_projects p ON p.id = d.project_id
         WHERE p.server_id = $1 AND d.status IN ('queued', 'running')",
    )
    .bind(server)
    .fetch_one(&mut *tx)
    .await?;
    if active > 0 {
        tx.commit().await?;
        return Ok(StartRun::DeploymentsActive(active));
    }
    let run: Option<CleanupRun> = sqlx::query_as(&format!(
        "INSERT INTO docker_cleanup_runs (id, trigger, status, older_than_hours, server_id)
         VALUES ($1, $2, 'running', $3, $4)
         ON CONFLICT DO NOTHING
         RETURNING {RUN_COLUMNS}"
    ))
    .bind(Uuid::new_v4())
    .bind(trigger)
    .bind(older_than_hours)
    .bind(server)
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(run.map_or(StartRun::CleanupRunning, StartRun::Started))
}

pub async fn finish_run(
    pool: &PgPool,
    id: Uuid,
    result: Result<(u64, u64), &str>,
) -> sqlx::Result<CleanupRun> {
    let (status, images, builder, error) = match result {
        Ok((i, b)) => ("succeeded", i as i64, b as i64, None),
        Err(e) => ("failed", 0, 0, Some(e)),
    };
    sqlx::query_as(&format!(
        "UPDATE docker_cleanup_runs
         SET status = $2, finished_at = now(), images_freed_bytes = $3,
             builder_freed_bytes = $4, error_message = $5
         WHERE id = $1
         RETURNING {RUN_COLUMNS}"
    ))
    .bind(id)
    .bind(status)
    .bind(images)
    .bind(builder)
    .bind(error)
    .fetch_one(pool)
    .await
}

/// Runs still `running` after a panel-api crash are failed at startup.
pub async fn fail_interrupted_runs(pool: &PgPool) -> sqlx::Result<u64> {
    Ok(sqlx::query(
        "UPDATE docker_cleanup_runs SET status = 'failed', finished_at = now(),
                error_message = 'interrupted by a panel restart'
         WHERE status = 'running'",
    )
    .execute(pool)
    .await?
    .rows_affected())
}

pub async fn last_scheduled_start(
    pool: &PgPool,
    server: Uuid,
) -> sqlx::Result<Option<DateTime<Utc>>> {
    sqlx::query_scalar(
        "SELECT MAX(started_at) FROM docker_cleanup_runs
         WHERE trigger = 'schedule' AND server_id = $1",
    )
    .bind(server)
    .fetch_one(pool)
    .await
}

/// Keeps the newest 100 completed cleanup runs, never deleting a live
/// run's coordination record.
pub async fn trim_runs(pool: &PgPool) -> sqlx::Result<()> {
    sqlx::query(
        "DELETE FROM docker_cleanup_runs WHERE id IN
         (SELECT id FROM docker_cleanup_runs WHERE status != 'running'
          ORDER BY started_at DESC OFFSET 100)",
    )
    .execute(pool)
    .await?;
    Ok(())
}
