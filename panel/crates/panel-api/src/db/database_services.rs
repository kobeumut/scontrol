use sqlx::PgPool;
use uuid::Uuid;

/// What only database services have. `credentials_ciphertext` never leaves
/// panel-api except through the audited credentials endpoint and the
/// deployment request.
#[derive(Debug, Clone)]
pub struct DatabaseServiceRow {
    pub project_id: Uuid,
    pub loopback_port: i32,
    pub credentials_ciphertext: Vec<u8>,
    pub public_port: Option<i32>,
    pub allow_from: Vec<String>,
}

/// Adds the row with the lowest free loopback port in 25000..=29999, or
/// returns `None` when every port is taken.
pub async fn create(
    pool: &PgPool,
    project_id: Uuid,
    credentials_ciphertext: &[u8],
) -> sqlx::Result<Option<DatabaseServiceRow>> {
    sqlx::query_as!(
        DatabaseServiceRow,
        r#"INSERT INTO database_services (project_id, loopback_port, credentials_ciphertext)
           SELECT $1, port, $2
           FROM generate_series(25000, 29999) AS port
           WHERE NOT EXISTS (SELECT 1 FROM database_services WHERE loopback_port = port)
           ORDER BY port
           LIMIT 1
           RETURNING project_id, loopback_port, credentials_ciphertext, public_port,
                     allow_from"#,
        project_id,
        credentials_ciphertext,
    )
    .fetch_optional(pool)
    .await
}

pub async fn find(pool: &PgPool, project_id: Uuid) -> sqlx::Result<Option<DatabaseServiceRow>> {
    sqlx::query_as!(
        DatabaseServiceRow,
        r#"SELECT project_id, loopback_port, credentials_ciphertext, public_port, allow_from
           FROM database_services WHERE project_id = $1"#,
        project_id,
    )
    .fetch_optional(pool)
    .await
}

pub async fn list(pool: &PgPool) -> sqlx::Result<Vec<DatabaseServiceRow>> {
    sqlx::query_as!(
        DatabaseServiceRow,
        r#"SELECT project_id, loopback_port, credentials_ciphertext, public_port, allow_from
           FROM database_services ORDER BY created_at DESC"#,
    )
    .fetch_all(pool)
    .await
}

/// Records the applied exposure (`None` = closed). A public port already
/// used by another database service fails with a unique violation.
pub async fn set_exposure(
    pool: &PgPool,
    project_id: Uuid,
    public_port: Option<i32>,
    allow_from: &[String],
) -> sqlx::Result<()> {
    sqlx::query!(
        r#"UPDATE database_services
           SET public_port = $2, allow_from = $3, updated_at = now()
           WHERE project_id = $1"#,
        project_id,
        public_port,
        allow_from,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Whether another database service already listens on `public_port`.
pub async fn public_port_taken(
    pool: &PgPool,
    public_port: i32,
    except: Uuid,
) -> sqlx::Result<bool> {
    let taken = sqlx::query_scalar!(
        r#"SELECT EXISTS (
               SELECT 1 FROM database_services WHERE public_port = $1 AND project_id <> $2
           ) AS "taken!""#,
        public_port,
        except,
    )
    .fetch_one(pool)
    .await?;
    Ok(taken)
}
