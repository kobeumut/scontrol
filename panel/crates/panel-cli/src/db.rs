//! Direct database access for offline/local administration (install-time
//! bootstrap, server-side `panel-cli` usage). Talks to the same schema as
//! `panel-api`; kept intentionally small.

use sqlx::PgPool;
use uuid::Uuid;

pub async fn connect() -> anyhow::Result<PgPool> {
    let _ = dotenvy::dotenv();
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgresql:///scontrol?user=scontrol".to_string());
    Ok(PgPool::connect(&url).await?)
}

pub struct UserSummary {
    pub id: Uuid,
    pub username: String,
    pub email: String,
    pub is_admin: bool,
}

pub async fn create_user(
    pool: &PgPool,
    username: &str,
    email: &str,
    password_hash: &str,
    is_admin: bool,
) -> anyhow::Result<UserSummary> {
    let row = sqlx::query!(
        r#"INSERT INTO users (username, email, password_hash, is_admin)
           VALUES ($1, $2, $3, $4) RETURNING id, username, email, is_admin"#,
        username,
        email,
        password_hash,
        is_admin,
    )
    .fetch_one(pool)
    .await?;
    Ok(UserSummary {
        id: row.id,
        username: row.username,
        email: row.email,
        is_admin: row.is_admin,
    })
}

pub async fn list_users(pool: &PgPool) -> anyhow::Result<Vec<UserSummary>> {
    let rows =
        sqlx::query!("SELECT id, username, email, is_admin FROM users ORDER BY created_at ASC")
            .fetch_all(pool)
            .await?;
    Ok(rows
        .into_iter()
        .map(|r| UserSummary {
            id: r.id,
            username: r.username,
            email: r.email,
            is_admin: r.is_admin,
        })
        .collect())
}

pub struct SiteSummary {
    pub id: Uuid,
    pub domain: String,
    pub owner_username: String,
    pub status: String,
    pub ssl_enabled: bool,
}

pub async fn list_sites(pool: &PgPool) -> anyhow::Result<Vec<SiteSummary>> {
    let rows = sqlx::query!("SELECT id, domain, owner_username, status, ssl_enabled FROM sites ORDER BY created_at DESC")
        .fetch_all(pool)
        .await?;
    Ok(rows
        .into_iter()
        .map(|r| SiteSummary {
            id: r.id,
            domain: r.domain,
            owner_username: r.owner_username,
            status: r.status,
            ssl_enabled: r.ssl_enabled,
        })
        .collect())
}

pub async fn insert_site(
    pool: &PgPool,
    id: Uuid,
    domain: &str,
    owner_user_id: Uuid,
    owner_username: &str,
    runtime_json: serde_json::Value,
    document_root: &str,
) -> anyhow::Result<()> {
    sqlx::query!(
        r#"INSERT INTO sites (id, domain, owner_user_id, owner_username, runtime, document_root, status)
           VALUES ($1, $2, $3, $4, $5, $6, 'provisioning')"#,
        id,
        domain,
        owner_user_id,
        owner_username,
        runtime_json,
        document_root,
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn set_site_status(pool: &PgPool, id: Uuid, status: &str) -> anyhow::Result<()> {
    sqlx::query!(
        "UPDATE sites SET status = $1, updated_at = now() WHERE id = $2",
        status,
        id
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn delete_site(pool: &PgPool, id: Uuid) -> anyhow::Result<()> {
    sqlx::query!("DELETE FROM sites WHERE id = $1", id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn find_site(
    pool: &PgPool,
    id: Uuid,
) -> anyhow::Result<Option<(String, Uuid, serde_json::Value)>> {
    let row = sqlx::query!(
        "SELECT domain, owner_user_id, runtime FROM sites WHERE id = $1",
        id
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| (r.domain, r.owner_user_id, r.runtime)))
}

pub async fn find_user_by_id(pool: &PgPool, id: Uuid) -> anyhow::Result<Option<String>> {
    let row = sqlx::query!("SELECT username FROM users WHERE id = $1", id)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(|r| r.username))
}
