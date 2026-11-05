//! API key rows. Only the lookup prefix and an Argon2id hash of the secret
//! are stored (see `auth::api_keys`).

use chrono::{DateTime, Utc};
use panel_core::rbac::Grant;
use serde::Serialize;
use sqlx::PgPool;
use uuid::Uuid;

/// An API key as the API shows it: never the secret or its hash.
#[derive(Debug, Clone, Serialize)]
pub struct ApiKey {
    pub id: Uuid,
    pub user_id: Uuid,
    pub username: String,
    pub name: String,
    /// The public part of the token (`sck_<prefix>_…`), to recognise it.
    pub prefix: String,
    pub grants: Vec<Grant>,
    pub expires_at: Option<DateTime<Utc>>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub revoked_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

#[derive(sqlx::FromRow)]
struct ApiKeyRecord {
    id: Uuid,
    user_id: Uuid,
    username: String,
    name: String,
    prefix: String,
    grants: serde_json::Value,
    expires_at: Option<DateTime<Utc>>,
    last_used_at: Option<DateTime<Utc>>,
    revoked_at: Option<DateTime<Utc>>,
    created_at: DateTime<Utc>,
}

impl ApiKeyRecord {
    fn into_key(self) -> ApiKey {
        ApiKey {
            id: self.id,
            user_id: self.user_id,
            username: self.username,
            name: self.name,
            prefix: self.prefix,
            // A row that no longer parses grants nothing (fail-closed).
            grants: serde_json::from_value(self.grants).unwrap_or_default(),
            expires_at: self.expires_at,
            last_used_at: self.last_used_at,
            revoked_at: self.revoked_at,
            created_at: self.created_at,
        }
    }
}

/// What authenticating a token needs: the hash to verify and the grants.
pub struct ActiveKey {
    pub id: Uuid,
    pub user_id: Uuid,
    pub secret_hash: String,
    pub grants: Vec<Grant>,
}

const SELECT: &str = "SELECT k.id, k.user_id, u.username, k.name, k.prefix, k.grants,
        k.expires_at, k.last_used_at, k.revoked_at, k.created_at
     FROM api_keys k JOIN users u ON u.id = k.user_id";

#[allow(clippy::too_many_arguments)]
pub async fn insert(
    pool: &PgPool,
    id: Uuid,
    user_id: Uuid,
    name: &str,
    prefix: &str,
    secret_hash: &str,
    grants: &[Grant],
    expires_at: Option<DateTime<Utc>>,
) -> anyhow::Result<ApiKey> {
    sqlx::query(
        "INSERT INTO api_keys (id, user_id, name, prefix, secret_hash, grants, expires_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(id)
    .bind(user_id)
    .bind(name)
    .bind(prefix)
    .bind(secret_hash)
    .bind(serde_json::to_value(grants)?)
    .bind(expires_at)
    .execute(pool)
    .await?;
    find(pool, id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("API key vanished after insert"))
}

pub async fn find(pool: &PgPool, id: Uuid) -> sqlx::Result<Option<ApiKey>> {
    let record: Option<ApiKeyRecord> = sqlx::query_as(&format!("{SELECT} WHERE k.id = $1"))
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(record.map(ApiKeyRecord::into_key))
}

/// The keys of one user, or of everyone when `user_id` is `None`.
pub async fn list(pool: &PgPool, user_id: Option<Uuid>) -> sqlx::Result<Vec<ApiKey>> {
    let records: Vec<ApiKeyRecord> = sqlx::query_as(&format!(
        "{SELECT} WHERE $1::uuid IS NULL OR k.user_id = $1 ORDER BY k.created_at DESC"
    ))
    .bind(user_id)
    .fetch_all(pool)
    .await?;
    Ok(records.into_iter().map(ApiKeyRecord::into_key).collect())
}

/// A key that is neither revoked nor expired, by its prefix.
pub async fn find_active(pool: &PgPool, prefix: &str) -> sqlx::Result<Option<ActiveKey>> {
    let row: Option<(Uuid, Uuid, String, serde_json::Value)> = sqlx::query_as(
        "SELECT id, user_id, secret_hash, grants FROM api_keys
         WHERE prefix = $1 AND revoked_at IS NULL AND (expires_at IS NULL OR expires_at > now())",
    )
    .bind(prefix)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(id, user_id, secret_hash, grants)| ActiveKey {
        id,
        user_id,
        secret_hash,
        grants: serde_json::from_value(grants).unwrap_or_default(),
    }))
}

/// Records use at most once a minute, so busy keys do not write per request.
pub async fn touch(pool: &PgPool, id: Uuid) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE api_keys SET last_used_at = now()
         WHERE id = $1 AND (last_used_at IS NULL OR last_used_at < now() - interval '1 minute')",
    )
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn revoke(pool: &PgPool, id: Uuid) -> sqlx::Result<bool> {
    let result =
        sqlx::query("UPDATE api_keys SET revoked_at = now() WHERE id = $1 AND revoked_at IS NULL")
            .bind(id)
            .execute(pool)
            .await?;
    Ok(result.rows_affected() == 1)
}
