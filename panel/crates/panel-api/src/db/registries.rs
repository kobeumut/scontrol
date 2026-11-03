use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

/// A stored registry. `credential_ciphertext` never leaves panel-api:
/// routes convert rows into a view without it.
#[derive(Debug, Clone)]
pub struct RegistryRow {
    pub id: Uuid,
    pub name: String,
    pub server_url: String,
    pub username: String,
    pub credential_ciphertext: Vec<u8>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

pub async fn list(pool: &PgPool) -> sqlx::Result<Vec<RegistryRow>> {
    sqlx::query_as!(
        RegistryRow,
        r#"SELECT id, name, server_url, username, credential_ciphertext, created_at, updated_at
           FROM docker_registries ORDER BY name"#
    )
    .fetch_all(pool)
    .await
}

pub async fn find(pool: &PgPool, id: Uuid) -> sqlx::Result<Option<RegistryRow>> {
    sqlx::query_as!(
        RegistryRow,
        r#"SELECT id, name, server_url, username, credential_ciphertext, created_at, updated_at
           FROM docker_registries WHERE id = $1"#,
        id
    )
    .fetch_optional(pool)
    .await
}

pub async fn create(
    pool: &PgPool,
    id: Uuid,
    name: &str,
    server_url: &str,
    username: &str,
    credential_ciphertext: &[u8],
) -> sqlx::Result<RegistryRow> {
    sqlx::query_as!(
        RegistryRow,
        r#"INSERT INTO docker_registries (id, name, server_url, username, credential_ciphertext)
           VALUES ($1, $2, $3, $4, $5)
           RETURNING id, name, server_url, username, credential_ciphertext, created_at, updated_at"#,
        id,
        name,
        server_url,
        username,
        credential_ciphertext,
    )
    .fetch_one(pool)
    .await
}

pub async fn update(
    pool: &PgPool,
    id: Uuid,
    name: &str,
    server_url: &str,
    username: &str,
    credential_ciphertext: &[u8],
) -> sqlx::Result<Option<RegistryRow>> {
    sqlx::query_as!(
        RegistryRow,
        r#"UPDATE docker_registries
           SET name = $2, server_url = $3, username = $4, credential_ciphertext = $5,
               updated_at = now()
           WHERE id = $1
           RETURNING id, name, server_url, username, credential_ciphertext, created_at, updated_at"#,
        id,
        name,
        server_url,
        username,
        credential_ciphertext,
    )
    .fetch_optional(pool)
    .await
}

pub async fn delete(pool: &PgPool, id: Uuid) -> sqlx::Result<bool> {
    let deleted = sqlx::query!("DELETE FROM docker_registries WHERE id = $1", id)
        .execute(pool)
        .await?
        .rows_affected();
    Ok(deleted == 1)
}

pub async fn project_registry(pool: &PgPool, project_id: Uuid) -> sqlx::Result<Option<Uuid>> {
    let id = sqlx::query_scalar!(
        "SELECT registry_id FROM compose_projects WHERE id = $1",
        project_id
    )
    .fetch_optional(pool)
    .await?;
    Ok(id.flatten())
}

pub async fn set_project_registry(
    pool: &PgPool,
    project_id: Uuid,
    registry_id: Option<Uuid>,
) -> sqlx::Result<()> {
    sqlx::query!(
        "UPDATE compose_projects SET registry_id = $2, updated_at = now() WHERE id = $1",
        project_id,
        registry_id
    )
    .execute(pool)
    .await?;
    Ok(())
}
