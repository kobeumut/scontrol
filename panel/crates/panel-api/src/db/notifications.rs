use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

/// A stored channel. `secret_sealed` never leaves panel-api: routes convert
/// rows into a view that only says whether a secret is set.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ChannelRow {
    pub id: Uuid,
    pub name: String,
    pub kind: String,
    pub config: serde_json::Value,
    pub secret_sealed: Option<Vec<u8>>,
    pub events: Vec<String>,
    pub enabled: bool,
    pub last_sent_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

pub struct ChannelInput<'a> {
    pub name: &'a str,
    pub kind: &'a str,
    pub config: serde_json::Value,
    pub secret_sealed: Option<Vec<u8>>,
    pub events: &'a [String],
    pub enabled: bool,
}

pub async fn list(pool: &PgPool) -> sqlx::Result<Vec<ChannelRow>> {
    sqlx::query_as!(
        ChannelRow,
        r#"SELECT id, name, kind, config, secret_sealed, events, enabled, last_sent_at,
                  last_error, created_at, updated_at
           FROM notification_channels ORDER BY created_at"#
    )
    .fetch_all(pool)
    .await
}

pub async fn find(pool: &PgPool, id: Uuid) -> sqlx::Result<Option<ChannelRow>> {
    sqlx::query_as!(
        ChannelRow,
        r#"SELECT id, name, kind, config, secret_sealed, events, enabled, last_sent_at,
                  last_error, created_at, updated_at
           FROM notification_channels WHERE id = $1"#,
        id
    )
    .fetch_optional(pool)
    .await
}

/// Enabled channels subscribed to `event`.
pub async fn list_for_event(pool: &PgPool, event: &str) -> sqlx::Result<Vec<ChannelRow>> {
    sqlx::query_as!(
        ChannelRow,
        r#"SELECT id, name, kind, config, secret_sealed, events, enabled, last_sent_at,
                  last_error, created_at, updated_at
           FROM notification_channels WHERE enabled AND $1 = ANY(events)"#,
        event
    )
    .fetch_all(pool)
    .await
}

pub async fn create(pool: &PgPool, id: Uuid, input: ChannelInput<'_>) -> sqlx::Result<ChannelRow> {
    sqlx::query_as!(
        ChannelRow,
        r#"INSERT INTO notification_channels (id, name, kind, config, secret_sealed, events, enabled)
           VALUES ($1, $2, $3, $4, $5, $6, $7)
           RETURNING id, name, kind, config, secret_sealed, events, enabled, last_sent_at,
                     last_error, created_at, updated_at"#,
        id,
        input.name,
        input.kind,
        input.config,
        input.secret_sealed,
        input.events,
        input.enabled,
    )
    .fetch_one(pool)
    .await
}

pub async fn update(
    pool: &PgPool,
    id: Uuid,
    input: ChannelInput<'_>,
) -> sqlx::Result<Option<ChannelRow>> {
    sqlx::query_as!(
        ChannelRow,
        r#"UPDATE notification_channels
           SET name = $2, kind = $3, config = $4, secret_sealed = $5, events = $6, enabled = $7,
               last_error = NULL, updated_at = now()
           WHERE id = $1
           RETURNING id, name, kind, config, secret_sealed, events, enabled, last_sent_at,
                     last_error, created_at, updated_at"#,
        id,
        input.name,
        input.kind,
        input.config,
        input.secret_sealed,
        input.events,
        input.enabled,
    )
    .fetch_optional(pool)
    .await
}

pub async fn delete(pool: &PgPool, id: Uuid) -> sqlx::Result<bool> {
    let deleted = sqlx::query!("DELETE FROM notification_channels WHERE id = $1", id)
        .execute(pool)
        .await?
        .rows_affected();
    Ok(deleted == 1)
}

/// Records a delivery: success clears the error, failure keeps the last
/// success time and stores the (already sanitized) error.
pub async fn record_result(pool: &PgPool, id: Uuid, error: Option<&String>) -> sqlx::Result<()> {
    match error {
        None => {
            sqlx::query!(
                "UPDATE notification_channels SET last_sent_at = now(), last_error = NULL WHERE id = $1",
                id
            )
            .execute(pool)
            .await?;
        }
        Some(error) => {
            sqlx::query!(
                "UPDATE notification_channels SET last_error = $2 WHERE id = $1",
                id,
                error
            )
            .execute(pool)
            .await?;
        }
    }
    Ok(())
}
