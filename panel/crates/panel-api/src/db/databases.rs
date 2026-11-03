use chrono::{DateTime, Utc};
use panel_core::types::{DatabaseEngine, DatabaseInfo};
use sqlx::PgPool;
use std::str::FromStr;
use uuid::Uuid;

#[derive(Debug, sqlx::FromRow)]
struct DatabaseRecord {
    id: Uuid,
    site_id: Option<Uuid>,
    engine: String,
    name: String,
    username: String,
    created_at: DateTime<Utc>,
}

impl DatabaseRecord {
    fn into_info(self) -> anyhow::Result<DatabaseInfo> {
        Ok(DatabaseInfo {
            id: self.id,
            site_id: self.site_id,
            engine: DatabaseEngine::from_str(&self.engine).map_err(anyhow::Error::msg)?,
            name: self.name,
            username: self.username,
            created_at: self.created_at,
        })
    }
}

pub async fn create(
    pool: &PgPool,
    site_id: Option<Uuid>,
    engine: DatabaseEngine,
    name: &str,
    username: &str,
) -> anyhow::Result<DatabaseInfo> {
    let engine_str = engine.to_string();
    let record = sqlx::query_as!(
        DatabaseRecord,
        r#"
        INSERT INTO databases (site_id, engine, name, username)
        VALUES ($1, $2, $3, $4)
        RETURNING id, site_id, engine, name, username, created_at
        "#,
        site_id,
        engine_str,
        name,
        username,
    )
    .fetch_one(pool)
    .await?;
    record.into_info()
}

pub async fn list_for_site(pool: &PgPool, site_id: Uuid) -> anyhow::Result<Vec<DatabaseInfo>> {
    let records = sqlx::query_as!(
        DatabaseRecord,
        "SELECT id, site_id, engine, name, username, created_at FROM databases WHERE site_id = $1",
        site_id,
    )
    .fetch_all(pool)
    .await?;
    records.into_iter().map(|r| r.into_info()).collect()
}

/// Used by callers that need to look a database up by its (engine, name)
/// unique key rather than its row id (e.g. future CLI lookups).
#[allow(dead_code)]
pub async fn find_by_name(
    pool: &PgPool,
    engine: DatabaseEngine,
    name: &str,
) -> anyhow::Result<Option<DatabaseInfo>> {
    let engine_str = engine.to_string();
    let record = sqlx::query_as!(
        DatabaseRecord,
        "SELECT id, site_id, engine, name, username, created_at FROM databases WHERE engine = $1 AND name = $2",
        engine_str,
        name,
    )
    .fetch_optional(pool)
    .await?;
    record.map(|r| r.into_info()).transpose()
}

pub async fn delete(pool: &PgPool, id: Uuid) -> sqlx::Result<()> {
    sqlx::query!("DELETE FROM databases WHERE id = $1", id)
        .execute(pool)
        .await?;
    Ok(())
}
