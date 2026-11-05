use chrono::{DateTime, Utc};
use panel_core::schedule::{JobRunInfo, JobRunStatus, JobSchedule, ScheduledJob};
use serde::Serialize;
use sqlx::PgPool;
use std::str::FromStr;
use uuid::Uuid;

#[derive(sqlx::FromRow)]
struct JobRecord {
    id: Uuid,
    compose_project_id: Uuid,
    name: String,
    service_name: String,
    command: Vec<String>,
    schedule_kind: String,
    schedule_expression: String,
    timezone: String,
    on_calendar: Vec<String>,
    timeout_secs: i32,
    enabled: bool,
    last_run_at: Option<DateTime<Utc>>,
    last_run_status: Option<String>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl JobRecord {
    fn into_job(self) -> anyhow::Result<ScheduledJob> {
        Ok(ScheduledJob {
            id: self.id,
            compose_project_id: self.compose_project_id,
            name: self.name,
            service_name: self.service_name,
            command: self.command,
            schedule: JobSchedule::from_parts(&self.schedule_kind, self.schedule_expression)
                .ok_or_else(|| anyhow::anyhow!("unknown schedule kind"))?,
            timezone: self.timezone,
            on_calendar: self.on_calendar,
            timeout_secs: u32::try_from(self.timeout_secs)?,
            enabled: self.enabled,
            last_run_at: self.last_run_at,
            last_run_status: self
                .last_run_status
                .map(|s| JobRunStatus::from_str(&s))
                .transpose()
                .map_err(anyhow::Error::msg)?,
            created_at: self.created_at,
            updated_at: self.updated_at,
        })
    }
}

/// A run as panel-api stores and returns it. Its output is on the agent.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct JobRunRow {
    pub id: Uuid,
    pub job_id: Uuid,
    pub trigger: String,
    pub status: String,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub exit_code: Option<i32>,
    pub error_message: Option<String>,
    pub log_bytes: i64,
    pub log_truncated: bool,
}

/// Fields of a job that create and update set.
pub struct JobInput<'a> {
    pub name: &'a str,
    pub service_name: &'a str,
    pub command: &'a [String],
    pub schedule: &'a JobSchedule,
    pub timezone: &'a str,
    pub on_calendar: &'a [String],
    pub timeout_secs: u32,
    pub enabled: bool,
}

/// A job and the slug of its Compose project, for agent requests.
pub struct JobRef {
    pub job_id: Uuid,
    pub project_slug: String,
    /// The server whose agent runs the job.
    pub server_id: Uuid,
}

pub async fn create(
    pool: &PgPool,
    id: Uuid,
    project_id: Uuid,
    input: &JobInput<'_>,
) -> anyhow::Result<ScheduledJob> {
    let record = sqlx::query_as!(
        JobRecord,
        r#"INSERT INTO job_schedules
               (id, compose_project_id, name, service_name, command, schedule_kind,
                schedule_expression, timezone, on_calendar, timeout_secs, enabled)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
           RETURNING id, compose_project_id, name, service_name, command, schedule_kind,
                     schedule_expression, timezone, on_calendar, timeout_secs, enabled,
                     last_run_at, last_run_status, created_at, updated_at"#,
        id,
        project_id,
        input.name,
        input.service_name,
        input.command,
        input.schedule.kind(),
        input.schedule.expression(),
        input.timezone,
        input.on_calendar,
        i32::try_from(input.timeout_secs)?,
        input.enabled,
    )
    .fetch_one(pool)
    .await?;
    record.into_job()
}

pub async fn update(
    pool: &PgPool,
    id: Uuid,
    input: &JobInput<'_>,
) -> anyhow::Result<Option<ScheduledJob>> {
    let record = sqlx::query_as!(
        JobRecord,
        r#"UPDATE job_schedules
           SET name = $2, service_name = $3, command = $4, schedule_kind = $5,
               schedule_expression = $6, timezone = $7, on_calendar = $8,
               timeout_secs = $9, enabled = $10, updated_at = now()
           WHERE id = $1
           RETURNING id, compose_project_id, name, service_name, command, schedule_kind,
                     schedule_expression, timezone, on_calendar, timeout_secs, enabled,
                     last_run_at, last_run_status, created_at, updated_at"#,
        id,
        input.name,
        input.service_name,
        input.command,
        input.schedule.kind(),
        input.schedule.expression(),
        input.timezone,
        input.on_calendar,
        i32::try_from(input.timeout_secs)?,
        input.enabled,
    )
    .fetch_optional(pool)
    .await?;
    record.map(JobRecord::into_job).transpose()
}

pub async fn find(
    pool: &PgPool,
    project_id: Uuid,
    id: Uuid,
) -> anyhow::Result<Option<ScheduledJob>> {
    let record = sqlx::query_as!(
        JobRecord,
        r#"SELECT id, compose_project_id, name, service_name, command, schedule_kind,
                  schedule_expression, timezone, on_calendar, timeout_secs, enabled,
                  last_run_at, last_run_status, created_at, updated_at
           FROM job_schedules WHERE id = $1 AND compose_project_id = $2"#,
        id,
        project_id,
    )
    .fetch_optional(pool)
    .await?;
    record.map(JobRecord::into_job).transpose()
}

pub async fn find_by_id(pool: &PgPool, id: Uuid) -> anyhow::Result<Option<ScheduledJob>> {
    let record = sqlx::query_as!(
        JobRecord,
        r#"SELECT id, compose_project_id, name, service_name, command, schedule_kind,
                  schedule_expression, timezone, on_calendar, timeout_secs, enabled,
                  last_run_at, last_run_status, created_at, updated_at
           FROM job_schedules WHERE id = $1"#,
        id,
    )
    .fetch_optional(pool)
    .await?;
    record.map(JobRecord::into_job).transpose()
}

pub async fn list_for_project(
    pool: &PgPool,
    project_id: Uuid,
) -> anyhow::Result<Vec<ScheduledJob>> {
    let records = sqlx::query_as!(
        JobRecord,
        r#"SELECT id, compose_project_id, name, service_name, command, schedule_kind,
                  schedule_expression, timezone, on_calendar, timeout_secs, enabled,
                  last_run_at, last_run_status, created_at, updated_at
           FROM job_schedules WHERE compose_project_id = $1 ORDER BY name"#,
        project_id,
    )
    .fetch_all(pool)
    .await?;
    records.into_iter().map(JobRecord::into_job).collect()
}

pub async fn count_for_project(pool: &PgPool, project_id: Uuid) -> sqlx::Result<i64> {
    let count = sqlx::query_scalar!(
        r#"SELECT count(*) AS "count!" FROM job_schedules WHERE compose_project_id = $1"#,
        project_id
    )
    .fetch_one(pool)
    .await?;
    Ok(count)
}

pub async fn delete(pool: &PgPool, id: Uuid) -> sqlx::Result<()> {
    sqlx::query!("DELETE FROM job_schedules WHERE id = $1", id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Every job with its project slug: what the run collector asks the agent
/// about.
pub async fn list_refs(pool: &PgPool) -> sqlx::Result<Vec<JobRef>> {
    sqlx::query_as!(
        JobRef,
        r#"SELECT j.id AS job_id, p.slug AS project_slug, p.server_id
           FROM job_schedules j JOIN compose_projects p ON p.id = j.compose_project_id"#
    )
    .fetch_all(pool)
    .await
}

/// Stores a run reported by the agent. Returns its status when the row is
/// new or this call changed it, and `None` when it was already recorded
/// (finished runs are never rewritten), so a caller acts on each finished
/// run exactly once.
pub async fn upsert_run(pool: &PgPool, run: &JobRunInfo) -> sqlx::Result<Option<JobRunStatus>> {
    let log_bytes = i64::try_from(run.log_bytes).unwrap_or(i64::MAX);
    let status = sqlx::query_scalar!(
        r#"INSERT INTO job_runs
               (id, job_id, trigger, status, started_at, finished_at, exit_code,
                error_message, log_bytes, log_truncated)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
           ON CONFLICT (id) DO UPDATE
           SET status = EXCLUDED.status, finished_at = EXCLUDED.finished_at,
               exit_code = EXCLUDED.exit_code, error_message = EXCLUDED.error_message,
               log_bytes = EXCLUDED.log_bytes, log_truncated = EXCLUDED.log_truncated
           WHERE job_runs.status = 'running'
             AND (EXCLUDED.status <> 'running' OR EXCLUDED.log_bytes <> job_runs.log_bytes)
           RETURNING status"#,
        run.id,
        run.job_id,
        run.trigger.as_str(),
        run.status.as_str(),
        run.started_at,
        run.finished_at,
        run.exit_code,
        run.error_message,
        log_bytes,
        run.log_truncated,
    )
    .fetch_optional(pool)
    .await?;
    Ok(status.and_then(|s| JobRunStatus::from_str(&s).ok()))
}

/// Records the latest run on its job (an older run never overwrites it).
pub async fn set_last_run(
    pool: &PgPool,
    job_id: Uuid,
    started_at: DateTime<Utc>,
    status: JobRunStatus,
) -> sqlx::Result<()> {
    sqlx::query!(
        r#"UPDATE job_schedules SET last_run_at = $2, last_run_status = $3
           WHERE id = $1 AND (last_run_at IS NULL OR last_run_at <= $2)"#,
        job_id,
        started_at,
        status.as_str(),
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Keeps the `keep` most recent runs of a job.
pub async fn prune_runs(pool: &PgPool, job_id: Uuid, keep: i64) -> sqlx::Result<()> {
    sqlx::query!(
        r#"DELETE FROM job_runs WHERE job_id = $1 AND id NOT IN (
               SELECT id FROM job_runs WHERE job_id = $1 ORDER BY started_at DESC LIMIT $2)"#,
        job_id,
        keep,
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn list_runs(pool: &PgPool, job_id: Uuid, limit: i64) -> sqlx::Result<Vec<JobRunRow>> {
    sqlx::query_as!(
        JobRunRow,
        r#"SELECT id, job_id, trigger, status, started_at, finished_at, exit_code,
                  error_message, log_bytes, log_truncated
           FROM job_runs WHERE job_id = $1 ORDER BY started_at DESC LIMIT $2"#,
        job_id,
        limit,
    )
    .fetch_all(pool)
    .await
}

pub async fn find_run(
    pool: &PgPool,
    job_id: Uuid,
    run_id: Uuid,
) -> sqlx::Result<Option<JobRunRow>> {
    sqlx::query_as!(
        JobRunRow,
        r#"SELECT id, job_id, trigger, status, started_at, finished_at, exit_code,
                  error_message, log_bytes, log_truncated
           FROM job_runs WHERE id = $1 AND job_id = $2"#,
        run_id,
        job_id,
    )
    .fetch_optional(pool)
    .await
}

/// Whether any run is still in progress (the collector then polls faster).
pub async fn any_running(pool: &PgPool) -> sqlx::Result<bool> {
    let running = sqlx::query_scalar!(
        r#"SELECT EXISTS (SELECT 1 FROM job_runs WHERE status = 'running') AS "running!""#
    )
    .fetch_one(pool)
    .await?;
    Ok(running)
}
