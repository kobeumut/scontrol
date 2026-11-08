use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

/// A stored destination. `credential_ciphertext` never leaves panel-api.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct DestinationRow {
    pub id: Uuid,
    pub name: String,
    pub endpoint: String,
    pub region: String,
    pub bucket: String,
    pub prefix: String,
    pub retention_count: i32,
    pub credential_ciphertext: Vec<u8>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct RunRow {
    pub id: Uuid,
    pub destination_id: Uuid,
    pub kind: String,
    pub source: String,
    pub status: String,
    pub object_key: Option<String>,
    pub size_bytes: Option<i64>,
    pub pruned: i32,
    pub error: Option<String>,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
}

const DEST_COLUMNS: &str = "id, name, endpoint, region, bucket, prefix, retention_count, \
     credential_ciphertext, created_at, updated_at";
const RUN_COLUMNS: &str = "id, destination_id, kind, source, status, object_key, size_bytes, \
     pruned, error, started_at, finished_at";

pub struct DestinationFields<'a> {
    pub name: &'a str,
    pub endpoint: &'a str,
    pub region: &'a str,
    pub bucket: &'a str,
    pub prefix: &'a str,
    pub retention_count: i32,
}

pub async fn list_destinations(pool: &PgPool) -> sqlx::Result<Vec<DestinationRow>> {
    sqlx::query_as(&format!(
        "SELECT {DEST_COLUMNS} FROM backup_destinations ORDER BY name"
    ))
    .fetch_all(pool)
    .await
}

pub async fn find_destination(pool: &PgPool, id: Uuid) -> sqlx::Result<Option<DestinationRow>> {
    sqlx::query_as(&format!(
        "SELECT {DEST_COLUMNS} FROM backup_destinations WHERE id = $1"
    ))
    .bind(id)
    .fetch_optional(pool)
    .await
}

pub async fn create_destination(
    pool: &PgPool,
    id: Uuid,
    f: &DestinationFields<'_>,
    ciphertext: &[u8],
) -> sqlx::Result<DestinationRow> {
    sqlx::query_as(&format!(
        "INSERT INTO backup_destinations
           (id, name, endpoint, region, bucket, prefix, retention_count, credential_ciphertext)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8) RETURNING {DEST_COLUMNS}"
    ))
    .bind(id)
    .bind(f.name)
    .bind(f.endpoint)
    .bind(f.region)
    .bind(f.bucket)
    .bind(f.prefix)
    .bind(f.retention_count)
    .bind(ciphertext)
    .fetch_one(pool)
    .await
}

pub async fn update_destination(
    pool: &PgPool,
    id: Uuid,
    f: &DestinationFields<'_>,
    ciphertext: &[u8],
) -> sqlx::Result<Option<DestinationRow>> {
    sqlx::query_as(&format!(
        "UPDATE backup_destinations
           SET name = $2, endpoint = $3, region = $4, bucket = $5, prefix = $6,
               retention_count = $7, credential_ciphertext = $8, updated_at = now()
         WHERE id = $1 RETURNING {DEST_COLUMNS}"
    ))
    .bind(id)
    .bind(f.name)
    .bind(f.endpoint)
    .bind(f.region)
    .bind(f.bucket)
    .bind(f.prefix)
    .bind(f.retention_count)
    .bind(ciphertext)
    .fetch_optional(pool)
    .await
}

pub async fn delete_destination(pool: &PgPool, id: Uuid) -> sqlx::Result<bool> {
    let n = sqlx::query("DELETE FROM backup_destinations WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?
        .rows_affected();
    Ok(n == 1)
}

pub async fn start_run(
    pool: &PgPool,
    destination_id: Uuid,
    kind: &str,
    source: &str,
) -> sqlx::Result<RunRow> {
    sqlx::query_as(&format!(
        "INSERT INTO backup_runs (id, destination_id, kind, source)
         VALUES ($1, $2, $3, $4) RETURNING {RUN_COLUMNS}"
    ))
    .bind(Uuid::new_v4())
    .bind(destination_id)
    .bind(kind)
    .bind(source)
    .fetch_one(pool)
    .await
}

pub async fn finish_run(
    pool: &PgPool,
    id: Uuid,
    result: Result<(Option<String>, Option<i64>, i32), String>,
) -> sqlx::Result<()> {
    match result {
        Ok((key, size, pruned)) => {
            sqlx::query(
                "UPDATE backup_runs SET status = 'succeeded', object_key = $2, size_bytes = $3,
                   pruned = $4, finished_at = now() WHERE id = $1",
            )
            .bind(id)
            .bind(key)
            .bind(size)
            .bind(pruned)
            .execute(pool)
            .await?;
        }
        Err(error) => {
            sqlx::query(
                "UPDATE backup_runs SET status = 'failed', error = $2, finished_at = now()
                 WHERE id = $1",
            )
            .bind(id)
            .bind(error)
            .execute(pool)
            .await?;
        }
    }
    Ok(())
}

/// A panel restart leaves `running` rows no task will finish.
pub async fn fail_interrupted_runs(pool: &PgPool) -> sqlx::Result<u64> {
    Ok(sqlx::query(
        "UPDATE backup_runs SET status = 'failed', error = 'interrupted by a panel restart',
           finished_at = now() WHERE status = 'running'",
    )
    .execute(pool)
    .await?
    .rows_affected())
}

pub async fn list_runs(pool: &PgPool, destination_id: Option<Uuid>) -> sqlx::Result<Vec<RunRow>> {
    sqlx::query_as(&format!(
        "SELECT {RUN_COLUMNS} FROM backup_runs
         WHERE ($1::uuid IS NULL OR destination_id = $1)
         ORDER BY started_at DESC LIMIT 100"
    ))
    .bind(destination_id)
    .fetch_all(pool)
    .await
}

pub async fn find_run(pool: &PgPool, id: Uuid) -> sqlx::Result<Option<RunRow>> {
    sqlx::query_as(&format!(
        "SELECT {RUN_COLUMNS} FROM backup_runs WHERE id = $1"
    ))
    .bind(id)
    .fetch_optional(pool)
    .await
}

/// The server a Compose service runs on, by slug.
pub async fn service_server(pool: &PgPool, slug: &str) -> sqlx::Result<Option<Uuid>> {
    sqlx::query_scalar("SELECT server_id FROM compose_projects WHERE slug = $1")
        .bind(slug)
        .fetch_optional(pool)
        .await
}

/// The slug of the managed database service published on `loopback_port`
/// of `server_id`, if any (loopback ports are unique panel-wide).
pub async fn database_service_on_port(
    pool: &PgPool,
    server_id: Uuid,
    loopback_port: i32,
) -> sqlx::Result<Option<String>> {
    sqlx::query_scalar(
        "SELECT p.slug FROM database_services d
         JOIN compose_projects p ON p.id = d.project_id
         WHERE d.loopback_port = $1 AND p.server_id = $2",
    )
    .bind(loopback_port)
    .bind(server_id)
    .fetch_optional(pool)
    .await
}
