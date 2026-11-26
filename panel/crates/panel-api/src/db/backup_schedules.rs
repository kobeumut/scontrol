//! Scheduled backups (`backup_schedules`). Runtime `sqlx::query_as` like
//! the rest of the backup SQL, so no offline metadata is needed.

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ScheduleRow {
    pub id: Uuid,
    pub name: String,
    /// `database` or `volume`.
    pub target_kind: String,
    pub compose_project_id: Uuid,
    pub volume_name: Option<String>,
    pub service_name: Option<String>,
    pub stop_service: bool,
    pub s3_destination_id: Uuid,
    pub cron_expression: String,
    pub timezone: String,
    pub retention_count: i32,
    pub enabled: bool,
    pub next_run_at: Option<DateTime<Utc>>,
    pub last_run_at: Option<DateTime<Utc>>,
    pub last_run_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

const COLUMNS: &str = "id, name, target_kind, compose_project_id, volume_name, service_name, \
     stop_service, s3_destination_id, cron_expression, timezone, retention_count, enabled, \
     next_run_at, last_run_at, last_run_id, created_at, updated_at";

/// Validated values of a schedule.
pub struct ScheduleFields<'a> {
    pub name: &'a str,
    pub target_kind: &'a str,
    pub compose_project_id: Uuid,
    pub volume_name: Option<&'a str>,
    pub service_name: Option<&'a str>,
    pub stop_service: bool,
    pub s3_destination_id: Uuid,
    pub cron_expression: &'a str,
    pub timezone: &'a str,
    pub retention_count: i32,
    pub enabled: bool,
    pub next_run_at: Option<DateTime<Utc>>,
}

pub async fn list(pool: &PgPool) -> sqlx::Result<Vec<ScheduleRow>> {
    sqlx::query_as(&format!(
        "SELECT {COLUMNS} FROM backup_schedules ORDER BY name, id"
    ))
    .fetch_all(pool)
    .await
}

pub async fn find(pool: &PgPool, id: Uuid) -> sqlx::Result<Option<ScheduleRow>> {
    sqlx::query_as(&format!(
        "SELECT {COLUMNS} FROM backup_schedules WHERE id = $1"
    ))
    .bind(id)
    .fetch_optional(pool)
    .await
}

pub async fn create(pool: &PgPool, f: &ScheduleFields<'_>) -> sqlx::Result<ScheduleRow> {
    sqlx::query_as(&format!(
        "INSERT INTO backup_schedules
           (id, name, target_kind, compose_project_id, volume_name, service_name, stop_service,
            s3_destination_id, cron_expression, timezone, retention_count, enabled, next_run_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)
         RETURNING {COLUMNS}"
    ))
    .bind(Uuid::new_v4())
    .bind(f.name)
    .bind(f.target_kind)
    .bind(f.compose_project_id)
    .bind(f.volume_name)
    .bind(f.service_name)
    .bind(f.stop_service)
    .bind(f.s3_destination_id)
    .bind(f.cron_expression)
    .bind(f.timezone)
    .bind(f.retention_count)
    .bind(f.enabled)
    .bind(f.next_run_at)
    .fetch_one(pool)
    .await
}

/// Updates everything but the target, which a schedule keeps for life (its
/// runs and its S3 series are about that one database or volume).
pub async fn update(
    pool: &PgPool,
    id: Uuid,
    f: &ScheduleFields<'_>,
) -> sqlx::Result<Option<ScheduleRow>> {
    sqlx::query_as(&format!(
        "UPDATE backup_schedules
           SET name = $2, service_name = $3, stop_service = $4, s3_destination_id = $5,
               cron_expression = $6, timezone = $7, retention_count = $8, enabled = $9,
               next_run_at = $10, updated_at = now()
         WHERE id = $1 RETURNING {COLUMNS}"
    ))
    .bind(id)
    .bind(f.name)
    .bind(f.service_name)
    .bind(f.stop_service)
    .bind(f.s3_destination_id)
    .bind(f.cron_expression)
    .bind(f.timezone)
    .bind(f.retention_count)
    .bind(f.enabled)
    .bind(f.next_run_at)
    .fetch_optional(pool)
    .await
}

pub async fn delete(pool: &PgPool, id: Uuid) -> sqlx::Result<bool> {
    let n = sqlx::query("DELETE FROM backup_schedules WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?
        .rows_affected();
    Ok(n == 1)
}

/// Claims the schedules due at `now`: each one's `next_run_at` moves to
/// `next(row)` in the same transaction, under `FOR UPDATE SKIP LOCKED`, so
/// two panel processes never start the same occurrence twice. A schedule
/// missed while the panel was down runs once, not once per missed time.
pub async fn claim_due(
    pool: &PgPool,
    now: DateTime<Utc>,
    limit: i64,
    next: impl Fn(&ScheduleRow) -> Option<DateTime<Utc>>,
) -> sqlx::Result<Vec<ScheduleRow>> {
    let mut tx = pool.begin().await?;
    let due: Vec<ScheduleRow> = sqlx::query_as(&format!(
        "SELECT {COLUMNS} FROM backup_schedules
         WHERE enabled AND next_run_at <= $1
         ORDER BY next_run_at LIMIT $2
         FOR UPDATE SKIP LOCKED"
    ))
    .bind(now)
    .bind(limit)
    .fetch_all(&mut *tx)
    .await?;
    for row in &due {
        sqlx::query("UPDATE backup_schedules SET next_run_at = $2 WHERE id = $1")
            .bind(row.id)
            .bind(next(row))
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(due)
}

/// Links a started run to its schedule.
pub async fn record_run(
    pool: &PgPool,
    id: Uuid,
    run_id: Uuid,
    at: DateTime<Utc>,
) -> sqlx::Result<()> {
    sqlx::query("UPDATE backup_schedules SET last_run_at = $2, last_run_id = $3 WHERE id = $1")
        .bind(id)
        .bind(at)
        .bind(run_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Whether a run of this schedule is still in progress.
pub async fn has_running_run(pool: &PgPool, id: Uuid) -> sqlx::Result<bool> {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM backup_runs WHERE schedule_id = $1 AND status = 'running')",
    )
    .bind(id)
    .fetch_one(pool)
    .await
}

/// Status of each schedule's last run, for the list view.
pub async fn last_run_statuses(pool: &PgPool) -> sqlx::Result<Vec<(Uuid, String, Option<String>)>> {
    sqlx::query_as(
        "SELECT r.id, r.status, r.error FROM backup_runs r
         JOIN backup_schedules s ON s.last_run_id = r.id",
    )
    .fetch_all(pool)
    .await
}
