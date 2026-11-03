use chrono::{DateTime, Utc};
use panel_core::types::{Runtime, Site, SiteStatus};
use sqlx::PgPool;
use std::str::FromStr;
use uuid::Uuid;

#[derive(Debug, sqlx::FromRow)]
struct SiteRecord {
    id: Uuid,
    domain: String,
    owner_user_id: Uuid,
    runtime: serde_json::Value,
    document_root: String,
    ssl_enabled: bool,
    status: String,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl SiteRecord {
    fn into_site(self) -> anyhow::Result<Site> {
        Ok(Site {
            id: self.id,
            domain: self.domain,
            owner_user_id: self.owner_user_id,
            runtime: serde_json::from_value::<Runtime>(self.runtime)?,
            document_root: self.document_root,
            ssl_enabled: self.ssl_enabled,
            status: SiteStatus::from_str(&self.status).map_err(anyhow::Error::msg)?,
            created_at: self.created_at,
            updated_at: self.updated_at,
        })
    }
}

pub async fn create_site(
    pool: &PgPool,
    id: Uuid,
    domain: &str,
    owner_user_id: Uuid,
    owner_username: &str,
    runtime: &Runtime,
    document_root: &str,
) -> anyhow::Result<Site> {
    let runtime_json = serde_json::to_value(runtime)?;
    let record = sqlx::query_as!(
        SiteRecord,
        r#"
        INSERT INTO sites (id, domain, owner_user_id, owner_username, runtime, document_root, status)
        VALUES ($1, $2, $3, $4, $5, $6, 'provisioning')
        RETURNING id, domain, owner_user_id, runtime, document_root, ssl_enabled, status, created_at, updated_at
        "#,
        id,
        domain,
        owner_user_id,
        owner_username,
        runtime_json,
        document_root,
    )
    .fetch_one(pool)
    .await?;
    record.into_site()
}

pub async fn set_status(pool: &PgPool, id: Uuid, status: SiteStatus) -> sqlx::Result<()> {
    sqlx::query!(
        "UPDATE sites SET status = $1, updated_at = now() WHERE id = $2",
        status.to_string(),
        id,
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn set_ssl_enabled(pool: &PgPool, id: Uuid, enabled: bool) -> sqlx::Result<()> {
    sqlx::query!(
        "UPDATE sites SET ssl_enabled = $1, updated_at = now() WHERE id = $2",
        enabled,
        id,
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn find_by_id(pool: &PgPool, id: Uuid) -> anyhow::Result<Option<Site>> {
    let record = sqlx::query_as!(
        SiteRecord,
        r#"SELECT id, domain, owner_user_id, runtime, document_root, ssl_enabled, status, created_at, updated_at
           FROM sites WHERE id = $1"#,
        id,
    )
    .fetch_optional(pool)
    .await?;
    record.map(|r| r.into_site()).transpose()
}

pub async fn find_by_domain(pool: &PgPool, domain: &str) -> anyhow::Result<Option<Site>> {
    let record = sqlx::query_as!(
        SiteRecord,
        r#"SELECT id, domain, owner_user_id, runtime, document_root, ssl_enabled, status, created_at, updated_at
           FROM sites WHERE domain = $1"#,
        domain,
    )
    .fetch_optional(pool)
    .await?;
    record.map(|r| r.into_site()).transpose()
}

pub async fn list_for_owner(pool: &PgPool, owner_user_id: Uuid) -> anyhow::Result<Vec<Site>> {
    let records = sqlx::query_as!(
        SiteRecord,
        r#"SELECT id, domain, owner_user_id, runtime, document_root, ssl_enabled, status, created_at, updated_at
           FROM sites WHERE owner_user_id = $1 ORDER BY created_at DESC"#,
        owner_user_id,
    )
    .fetch_all(pool)
    .await?;
    records.into_iter().map(|r| r.into_site()).collect()
}

pub async fn list_all(pool: &PgPool) -> anyhow::Result<Vec<Site>> {
    let records = sqlx::query_as!(
        SiteRecord,
        r#"SELECT id, domain, owner_user_id, runtime, document_root, ssl_enabled, status, created_at, updated_at
           FROM sites ORDER BY created_at DESC"#,
    )
    .fetch_all(pool)
    .await?;
    records.into_iter().map(|r| r.into_site()).collect()
}

pub async fn delete(pool: &PgPool, id: Uuid) -> sqlx::Result<()> {
    sqlx::query!("DELETE FROM sites WHERE id = $1", id)
        .execute(pool)
        .await?;
    Ok(())
}
