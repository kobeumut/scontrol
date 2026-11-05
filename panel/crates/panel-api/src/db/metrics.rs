//! Metrics storage: the downsampled ring (`server_metrics`,
//! `container_metrics`), alert settings, and per-metric alert state.
//! Runtime `sqlx::query*` calls, so no offline metadata is needed.

use chrono::{DateTime, TimeZone, Utc};
use panel_core::metrics::{
    AlertTracker, ThresholdRule, COARSE_RESOLUTION_SECS, MEDIUM_RESOLUTION_SECS,
    RAW_RESOLUTION_SECS,
};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::collections::HashMap;
use uuid::Uuid;

/// How long each resolution is kept.
pub const RETENTION: [(i32, i64); 3] = [
    (RAW_RESOLUTION_SECS, 24 * 3600),
    (MEDIUM_RESOLUTION_SECS, 7 * 24 * 3600),
    (COARSE_RESOLUTION_SECS, 30 * 24 * 3600),
];

/// Start of the `resolution_secs` bucket holding `at`.
pub fn bucket_start(at: DateTime<Utc>, resolution_secs: i32) -> DateTime<Utc> {
    let step = i64::from(resolution_secs.max(1));
    let secs = at.timestamp().div_euclid(step) * step;
    Utc.timestamp_opt(secs, 0).single().unwrap_or(at)
}

#[derive(Debug, Clone, PartialEq)]
pub struct ServerSample {
    pub cpu_pct: f64,
    pub mem_used_bytes: i64,
    pub mem_total_bytes: i64,
    pub load1: f64,
    pub disk_used_bytes: i64,
    pub disk_total_bytes: i64,
    pub disk_read_bps: f64,
    pub disk_write_bps: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ContainerSample {
    pub compose_project_id: Uuid,
    pub service: String,
    pub cpu_pct: f64,
    pub mem_bytes: i64,
    pub mem_limit_bytes: Option<i64>,
    pub io_read_bps: f64,
    pub io_write_bps: f64,
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct ServerPoint {
    pub bucket: DateTime<Utc>,
    pub cpu_pct: f32,
    pub mem_used_bytes: i64,
    pub mem_total_bytes: i64,
    pub load1: f32,
    pub disk_used_bytes: i64,
    pub disk_total_bytes: i64,
    pub disk_read_bps: f64,
    pub disk_write_bps: f64,
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct ContainerPoint {
    pub bucket: DateTime<Utc>,
    pub service: String,
    pub cpu_pct: f32,
    pub mem_bytes: i64,
    pub mem_limit_bytes: Option<i64>,
    pub io_read_bps: f64,
    pub io_write_bps: f64,
}

/// Stores one raw (30 s) server sample. A second sample in the same bucket
/// replaces the first.
pub async fn insert_server_sample(
    pool: &PgPool,
    server: Uuid,
    bucket: DateTime<Utc>,
    s: &ServerSample,
) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO server_metrics (resolution_secs, bucket, cpu_pct, mem_used_bytes, \
             mem_total_bytes, load1, disk_used_bytes, disk_total_bytes, disk_read_bps, \
             disk_write_bps, samples, server_id) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, 1, $11) \
         ON CONFLICT (server_id, resolution_secs, bucket) DO UPDATE SET \
             cpu_pct = EXCLUDED.cpu_pct, mem_used_bytes = EXCLUDED.mem_used_bytes, \
             mem_total_bytes = EXCLUDED.mem_total_bytes, load1 = EXCLUDED.load1, \
             disk_used_bytes = EXCLUDED.disk_used_bytes, \
             disk_total_bytes = EXCLUDED.disk_total_bytes, \
             disk_read_bps = EXCLUDED.disk_read_bps, disk_write_bps = EXCLUDED.disk_write_bps",
    )
    .bind(RAW_RESOLUTION_SECS)
    .bind(bucket)
    .bind(s.cpu_pct as f32)
    .bind(s.mem_used_bytes)
    .bind(s.mem_total_bytes)
    .bind(s.load1 as f32)
    .bind(s.disk_used_bytes)
    .bind(s.disk_total_bytes)
    .bind(s.disk_read_bps)
    .bind(s.disk_write_bps)
    .bind(server)
    .execute(pool)
    .await?;
    Ok(())
}

/// Stores raw (30 s) per-service samples in one transaction.
pub async fn insert_container_samples(
    pool: &PgPool,
    bucket: DateTime<Utc>,
    samples: &[ContainerSample],
) -> sqlx::Result<()> {
    if samples.is_empty() {
        return Ok(());
    }
    let mut tx = pool.begin().await?;
    for s in samples {
        sqlx::query(
            "INSERT INTO container_metrics (resolution_secs, bucket, compose_project_id, \
                 service, cpu_pct, mem_bytes, mem_limit_bytes, io_read_bps, io_write_bps, \
                 samples) \
             SELECT $1, $2, $3, $4, $5, $6, $7, $8, $9, 1 \
             WHERE EXISTS (SELECT 1 FROM compose_projects WHERE id = $3) \
             ON CONFLICT (resolution_secs, compose_project_id, service, bucket) DO UPDATE SET \
                 cpu_pct = EXCLUDED.cpu_pct, mem_bytes = EXCLUDED.mem_bytes, \
                 mem_limit_bytes = EXCLUDED.mem_limit_bytes, \
                 io_read_bps = EXCLUDED.io_read_bps, io_write_bps = EXCLUDED.io_write_bps",
        )
        .bind(RAW_RESOLUTION_SECS)
        .bind(bucket)
        .bind(s.compose_project_id)
        .bind(&s.service)
        .bind(s.cpu_pct as f32)
        .bind(s.mem_bytes)
        .bind(s.mem_limit_bytes)
        .bind(s.io_read_bps)
        .bind(s.io_write_bps)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await
}

/// Averages `from`-resolution rows at or after `since` into `to`-resolution
/// buckets. Buckets are recomputed from scratch, so running it repeatedly
/// (including over a still-open bucket) is idempotent. `since` is aligned
/// down to a `to` bucket so no bucket is averaged from a partial window.
pub async fn rollup(pool: &PgPool, from: i32, to: i32, since: DateTime<Utc>) -> sqlx::Result<()> {
    let since = bucket_start(since, to);
    sqlx::query(
        "INSERT INTO server_metrics (server_id, resolution_secs, bucket, cpu_pct, mem_used_bytes, \
             mem_total_bytes, load1, disk_used_bytes, disk_total_bytes, disk_read_bps, \
             disk_write_bps, samples) \
         SELECT server_id, $2, to_timestamp((floor(extract(epoch FROM bucket) / $2) * $2)::double precision) AS b, \
             avg(cpu_pct)::real, avg(mem_used_bytes)::bigint, max(mem_total_bytes), \
             avg(load1)::real, avg(disk_used_bytes)::bigint, max(disk_total_bytes), \
             avg(disk_read_bps), avg(disk_write_bps), sum(samples)::int \
         FROM server_metrics WHERE resolution_secs = $1 AND bucket >= $3 \
         GROUP BY server_id, b \
         ON CONFLICT (server_id, resolution_secs, bucket) DO UPDATE SET \
             cpu_pct = EXCLUDED.cpu_pct, mem_used_bytes = EXCLUDED.mem_used_bytes, \
             mem_total_bytes = EXCLUDED.mem_total_bytes, load1 = EXCLUDED.load1, \
             disk_used_bytes = EXCLUDED.disk_used_bytes, \
             disk_total_bytes = EXCLUDED.disk_total_bytes, \
             disk_read_bps = EXCLUDED.disk_read_bps, disk_write_bps = EXCLUDED.disk_write_bps, \
             samples = EXCLUDED.samples",
    )
    .bind(from)
    .bind(to)
    .bind(since)
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO container_metrics (resolution_secs, bucket, compose_project_id, service, \
             cpu_pct, mem_bytes, mem_limit_bytes, io_read_bps, io_write_bps, samples) \
         SELECT $2, to_timestamp((floor(extract(epoch FROM bucket) / $2) * $2)::double precision) AS b, \
             compose_project_id, service, avg(cpu_pct)::real, avg(mem_bytes)::bigint, \
             max(mem_limit_bytes), avg(io_read_bps), avg(io_write_bps), sum(samples)::int \
         FROM container_metrics WHERE resolution_secs = $1 AND bucket >= $3 \
         GROUP BY b, compose_project_id, service \
         ON CONFLICT (resolution_secs, compose_project_id, service, bucket) DO UPDATE SET \
             cpu_pct = EXCLUDED.cpu_pct, mem_bytes = EXCLUDED.mem_bytes, \
             mem_limit_bytes = EXCLUDED.mem_limit_bytes, io_read_bps = EXCLUDED.io_read_bps, \
             io_write_bps = EXCLUDED.io_write_bps, samples = EXCLUDED.samples",
    )
    .bind(from)
    .bind(to)
    .bind(since)
    .execute(pool)
    .await?;
    Ok(())
}

/// Deletes rows older than each resolution's retention. Returns rows removed.
pub async fn prune(pool: &PgPool, now: DateTime<Utc>) -> sqlx::Result<u64> {
    let mut removed = 0;
    for (resolution, keep_secs) in RETENTION {
        let cutoff = now - chrono::Duration::seconds(keep_secs);
        for table in ["server_metrics", "container_metrics"] {
            removed += sqlx::query(&format!(
                "DELETE FROM {table} WHERE resolution_secs = $1 AND bucket < $2"
            ))
            .bind(resolution)
            .bind(cutoff)
            .execute(pool)
            .await?
            .rows_affected();
        }
    }
    Ok(removed)
}

pub async fn server_series(
    pool: &PgPool,
    server: Uuid,
    resolution: i32,
    since: DateTime<Utc>,
) -> sqlx::Result<Vec<ServerPoint>> {
    sqlx::query_as(
        "SELECT bucket, cpu_pct, mem_used_bytes, mem_total_bytes, load1, disk_used_bytes, \
             disk_total_bytes, disk_read_bps, disk_write_bps \
         FROM server_metrics WHERE resolution_secs = $1 AND bucket >= $2 AND server_id = $3 \
         ORDER BY bucket",
    )
    .bind(resolution)
    .bind(since)
    .bind(server)
    .fetch_all(pool)
    .await
}

pub async fn container_series(
    pool: &PgPool,
    compose_project_id: Uuid,
    resolution: i32,
    since: DateTime<Utc>,
) -> sqlx::Result<Vec<ContainerPoint>> {
    sqlx::query_as(
        "SELECT bucket, service, cpu_pct, mem_bytes, mem_limit_bytes, io_read_bps, io_write_bps \
         FROM container_metrics \
         WHERE compose_project_id = $1 AND resolution_secs = $2 AND bucket >= $3 \
         ORDER BY service, bucket",
    )
    .bind(compose_project_id)
    .bind(resolution)
    .bind(since)
    .fetch_all(pool)
    .await
}

/// `(id, slug, name)` of every Compose project, to map container labels.
#[cfg(test)]
pub async fn project_slugs(pool: &PgPool) -> sqlx::Result<Vec<(Uuid, String, String)>> {
    sqlx::query_as("SELECT id, slug, name FROM compose_projects")
        .fetch_all(pool)
        .await
}

/// `(id, slug, name)` of the Compose projects placed on `server`: only
/// these are mapped from that server's container labels.
pub async fn project_slugs_on(
    pool: &PgPool,
    server: Uuid,
) -> sqlx::Result<Vec<(Uuid, String, String)>> {
    sqlx::query_as("SELECT id, slug, name FROM compose_projects WHERE server_id = $1")
        .bind(server)
        .fetch_all(pool)
        .await
}

// ---------------------------------------------------------------------------
// Alerts
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, sqlx::FromRow)]
pub struct AlertSettings {
    pub enabled: bool,
    pub cpu_pct: f32,
    pub memory_pct: f32,
    pub disk_pct: f32,
    pub container_memory_pct: f32,
    pub hysteresis_pct: f32,
    pub trigger_samples: i32,
    pub clear_samples: i32,
}

impl AlertSettings {
    pub fn rule(&self, threshold: f32) -> ThresholdRule {
        ThresholdRule {
            threshold: f64::from(threshold),
            hysteresis: f64::from(self.hysteresis_pct),
            trigger_samples: self.trigger_samples.max(1) as u32,
            clear_samples: self.clear_samples.max(1) as u32,
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        for threshold in [
            self.cpu_pct,
            self.memory_pct,
            self.disk_pct,
            self.container_memory_pct,
        ] {
            self.rule(threshold).validate()?;
        }
        if !(1..=60).contains(&self.trigger_samples) || !(1..=60).contains(&self.clear_samples) {
            return Err("sample counts must be between 1 and 60".into());
        }
        Ok(())
    }
}

const SETTINGS_COLUMNS: &str = "enabled, cpu_pct, memory_pct, disk_pct, container_memory_pct, \
     hysteresis_pct, trigger_samples, clear_samples";

pub async fn alert_settings(pool: &PgPool) -> sqlx::Result<AlertSettings> {
    sqlx::query_as(&format!(
        "SELECT {SETTINGS_COLUMNS} FROM metric_alert_settings WHERE id"
    ))
    .fetch_one(pool)
    .await
}

pub async fn update_alert_settings(
    pool: &PgPool,
    s: &AlertSettings,
) -> sqlx::Result<AlertSettings> {
    sqlx::query_as(&format!(
        "UPDATE metric_alert_settings SET enabled = $1, cpu_pct = $2, memory_pct = $3, \
             disk_pct = $4, container_memory_pct = $5, hysteresis_pct = $6, \
             trigger_samples = $7, clear_samples = $8, updated_at = now() \
         WHERE id RETURNING {SETTINGS_COLUMNS}"
    ))
    .bind(s.enabled)
    .bind(s.cpu_pct)
    .bind(s.memory_pct)
    .bind(s.disk_pct)
    .bind(s.container_memory_pct)
    .bind(s.hysteresis_pct)
    .bind(s.trigger_samples)
    .bind(s.clear_samples)
    .fetch_one(pool)
    .await
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct AlertStateRow {
    pub server_id: Uuid,
    pub server_name: String,
    pub key: String,
    pub firing: bool,
    pub streak: i32,
    pub last_value: f32,
    pub changed_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl AlertStateRow {
    pub fn tracker(&self) -> AlertTracker {
        AlertTracker {
            firing: self.firing,
            streak: self.streak.max(0) as u32,
        }
    }
}

const STATE_SELECT: &str = "SELECT a.server_id, s.name AS server_name, a.key, a.firing, \
     a.streak, a.last_value, a.changed_at, a.updated_at \
     FROM metric_alert_state a JOIN servers s ON s.id = a.server_id";

/// The alert states of one server, by metric key.
pub async fn alert_states(
    pool: &PgPool,
    server: Uuid,
) -> sqlx::Result<HashMap<String, AlertStateRow>> {
    let rows: Vec<AlertStateRow> =
        sqlx::query_as(&format!("{STATE_SELECT} WHERE a.server_id = $1"))
            .bind(server)
            .fetch_all(pool)
            .await?;
    Ok(rows.into_iter().map(|row| (row.key.clone(), row)).collect())
}

/// Firing alerts of every server.
pub async fn firing_alerts(pool: &PgPool) -> sqlx::Result<Vec<AlertStateRow>> {
    sqlx::query_as(&format!(
        "{STATE_SELECT} WHERE a.firing ORDER BY a.changed_at"
    ))
    .fetch_all(pool)
    .await
}

/// Saves a metric's state. `changed` moves `changed_at` (an edge happened).
pub async fn save_alert_state(
    pool: &PgPool,
    server: Uuid,
    key: &str,
    tracker: AlertTracker,
    value: f64,
    changed: bool,
) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO metric_alert_state (key, firing, streak, last_value, server_id) \
         VALUES ($1, $2, $3, $4, $6) \
         ON CONFLICT (server_id, key) DO UPDATE SET firing = EXCLUDED.firing, streak = EXCLUDED.streak, \
             last_value = EXCLUDED.last_value, updated_at = now(), \
             changed_at = CASE WHEN $5 THEN now() ELSE metric_alert_state.changed_at END",
    )
    .bind(key)
    .bind(tracker.firing)
    .bind(tracker.streak.min(i32::MAX as u32) as i32)
    .bind(value as f32)
    .bind(changed)
    .bind(server)
    .execute(pool)
    .await?;
    Ok(())
}

/// Forgets metrics that no longer exist (an unmounted disk, a removed
/// service), so a later metric with the same key starts clean.
pub async fn delete_alert_states_except(
    pool: &PgPool,
    server: Uuid,
    keep: &[String],
) -> sqlx::Result<u64> {
    Ok(
        sqlx::query("DELETE FROM metric_alert_state WHERE server_id = $2 AND NOT (key = ANY($1))")
            .bind(keep)
            .bind(server)
            .execute(pool)
            .await?
            .rows_affected(),
    )
}

pub async fn clear_alert_states(pool: &PgPool) -> sqlx::Result<u64> {
    Ok(sqlx::query("DELETE FROM metric_alert_state")
        .execute(pool)
        .await?
        .rows_affected())
}
