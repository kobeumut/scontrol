//! Registered GitHub Apps. The private key column is sealed and is read
//! only by [`find_sealed`], for minting installation tokens.

use crate::services::github_app::SealedApp;
use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::PgPool;
use uuid::Uuid;

/// What the API shows of an App: never its key.
#[derive(Debug, Clone, Serialize)]
pub struct GitHubAppRow {
    pub id: Uuid,
    pub name: String,
    pub app_id: i64,
    pub created_at: DateTime<Utc>,
}

pub async fn create(
    pool: &PgPool,
    id: Uuid,
    name: &str,
    app_id: i64,
    private_key_sealed: &[u8],
    created_by: Uuid,
) -> sqlx::Result<GitHubAppRow> {
    sqlx::query_as!(
        GitHubAppRow,
        r#"INSERT INTO github_apps (id, name, app_id, private_key_sealed, created_by)
           VALUES ($1, $2, $3, $4, $5)
           RETURNING id, name, app_id, created_at"#,
        id,
        name,
        app_id,
        private_key_sealed,
        created_by
    )
    .fetch_one(pool)
    .await
}

pub async fn list(pool: &PgPool) -> sqlx::Result<Vec<GitHubAppRow>> {
    sqlx::query_as!(
        GitHubAppRow,
        "SELECT id, name, app_id, created_at FROM github_apps ORDER BY name"
    )
    .fetch_all(pool)
    .await
}

pub async fn find(pool: &PgPool, id: Uuid) -> sqlx::Result<Option<GitHubAppRow>> {
    sqlx::query_as!(
        GitHubAppRow,
        "SELECT id, name, app_id, created_at FROM github_apps WHERE id = $1",
        id
    )
    .fetch_optional(pool)
    .await
}

pub async fn find_sealed(pool: &PgPool, id: Uuid) -> sqlx::Result<Option<SealedApp>> {
    sqlx::query_as!(
        SealedApp,
        "SELECT id, app_id, private_key_sealed FROM github_apps WHERE id = $1",
        id
    )
    .fetch_optional(pool)
    .await
}

/// Slugs of the projects linked to the App.
pub async fn linked_projects(pool: &PgPool, id: Uuid) -> sqlx::Result<Vec<String>> {
    sqlx::query_scalar!(
        "SELECT slug FROM compose_projects WHERE github_app_id = $1 ORDER BY slug",
        id
    )
    .fetch_all(pool)
    .await
}

pub async fn delete(pool: &PgPool, id: Uuid) -> sqlx::Result<bool> {
    let result = sqlx::query!("DELETE FROM github_apps WHERE id = $1", id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}
