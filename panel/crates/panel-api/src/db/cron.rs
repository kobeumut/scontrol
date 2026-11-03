use panel_core::types::CronJob;
use sqlx::PgPool;
use uuid::Uuid;

#[derive(Debug, sqlx::FromRow)]
struct CronRecord {
    id: Uuid,
    site_id: Uuid,
    schedule: String,
    command: String,
    enabled: bool,
}

impl From<CronRecord> for CronJob {
    fn from(r: CronRecord) -> Self {
        CronJob {
            id: r.id,
            site_id: r.site_id,
            schedule: r.schedule,
            command: r.command,
            enabled: r.enabled,
        }
    }
}

pub async fn create(
    pool: &PgPool,
    site_id: Uuid,
    schedule: &str,
    command: &str,
) -> sqlx::Result<CronJob> {
    let record = sqlx::query_as!(
        CronRecord,
        r#"INSERT INTO cron_jobs (site_id, schedule, command) VALUES ($1, $2, $3)
           RETURNING id, site_id, schedule, command, enabled"#,
        site_id,
        schedule,
        command,
    )
    .fetch_one(pool)
    .await?;
    Ok(record.into())
}

pub async fn list_for_site(pool: &PgPool, site_id: Uuid) -> sqlx::Result<Vec<CronJob>> {
    let records = sqlx::query_as!(
        CronRecord,
        "SELECT id, site_id, schedule, command, enabled FROM cron_jobs WHERE site_id = $1",
        site_id,
    )
    .fetch_all(pool)
    .await?;
    Ok(records.into_iter().map(Into::into).collect())
}

pub async fn set_enabled(pool: &PgPool, id: Uuid, enabled: bool) -> sqlx::Result<()> {
    sqlx::query!(
        "UPDATE cron_jobs SET enabled = $1 WHERE id = $2",
        enabled,
        id
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn delete(pool: &PgPool, id: Uuid) -> sqlx::Result<()> {
    sqlx::query!("DELETE FROM cron_jobs WHERE id = $1", id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn find_by_id(pool: &PgPool, id: Uuid) -> sqlx::Result<Option<CronJob>> {
    let record = sqlx::query_as!(
        CronRecord,
        "SELECT id, site_id, schedule, command, enabled FROM cron_jobs WHERE id = $1",
        id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(record.map(Into::into))
}
