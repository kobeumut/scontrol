use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct UserRow {
    pub id: Uuid,
    pub username: String,
    pub email: String,
    pub password_hash: String,
    pub is_admin: bool,
    pub totp_secret: Option<String>,
    pub totp_enabled: bool,
    pub created_at: DateTime<Utc>,
    #[allow(dead_code)] // kept for audit/debugging; not yet surfaced via API
    pub updated_at: DateTime<Utc>,
}

impl UserRow {
    pub fn to_public(&self) -> panel_core::types::User {
        panel_core::types::User {
            id: self.id,
            username: self.username.clone(),
            email: self.email.clone(),
            is_admin: self.is_admin,
            totp_enabled: self.totp_enabled,
            created_at: self.created_at,
        }
    }
}

pub async fn create_user(
    pool: &PgPool,
    username: &str,
    email: &str,
    password_hash: &str,
    is_admin: bool,
) -> sqlx::Result<UserRow> {
    sqlx::query_as!(
        UserRow,
        r#"
        INSERT INTO users (username, email, password_hash, is_admin)
        VALUES ($1, $2, $3, $4)
        RETURNING id, username, email, password_hash, is_admin, totp_secret, totp_enabled, created_at, updated_at
        "#,
        username,
        email,
        password_hash,
        is_admin,
    )
    .fetch_one(pool)
    .await
}

pub async fn find_by_username(pool: &PgPool, username: &str) -> sqlx::Result<Option<UserRow>> {
    sqlx::query_as!(
        UserRow,
        r#"SELECT id, username, email, password_hash, is_admin, totp_secret, totp_enabled, created_at, updated_at
           FROM users WHERE username = $1"#,
        username,
    )
    .fetch_optional(pool)
    .await
}

pub async fn find_by_id(pool: &PgPool, id: Uuid) -> sqlx::Result<Option<UserRow>> {
    sqlx::query_as!(
        UserRow,
        r#"SELECT id, username, email, password_hash, is_admin, totp_secret, totp_enabled, created_at, updated_at
           FROM users WHERE id = $1"#,
        id,
    )
    .fetch_optional(pool)
    .await
}

pub async fn count_users(pool: &PgPool) -> sqlx::Result<i64> {
    let row = sqlx::query!("SELECT COUNT(*) as count FROM users")
        .fetch_one(pool)
        .await?;
    Ok(row.count.unwrap_or(0))
}

pub async fn set_totp_secret(pool: &PgPool, user_id: Uuid, secret: &str) -> sqlx::Result<()> {
    sqlx::query!(
        "UPDATE users SET totp_secret = $1, updated_at = now() WHERE id = $2",
        secret,
        user_id,
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn enable_totp(pool: &PgPool, user_id: Uuid) -> sqlx::Result<()> {
    sqlx::query!(
        "UPDATE users SET totp_enabled = TRUE, updated_at = now() WHERE id = $1",
        user_id,
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn disable_totp(pool: &PgPool, user_id: Uuid) -> sqlx::Result<()> {
    sqlx::query!(
        "UPDATE users SET totp_enabled = FALSE, totp_secret = NULL, updated_at = now() WHERE id = $1",
        user_id,
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn create_session(
    pool: &PgPool,
    user_id: Uuid,
    token_hash: &str,
    expires_at: DateTime<Utc>,
) -> sqlx::Result<Uuid> {
    let row = sqlx::query!(
        "INSERT INTO sessions (user_id, token_hash, expires_at) VALUES ($1, $2, $3) RETURNING id",
        user_id,
        token_hash,
        expires_at,
    )
    .fetch_one(pool)
    .await?;
    Ok(row.id)
}

pub async fn find_session_user(pool: &PgPool, token_hash: &str) -> sqlx::Result<Option<UserRow>> {
    sqlx::query_as!(
        UserRow,
        r#"
        SELECT u.id, u.username, u.email, u.password_hash, u.is_admin, u.totp_secret,
               u.totp_enabled, u.created_at, u.updated_at
        FROM sessions s
        JOIN users u ON u.id = s.user_id
        WHERE s.token_hash = $1 AND s.expires_at > now()
        "#,
        token_hash,
    )
    .fetch_optional(pool)
    .await
}

pub async fn delete_session(pool: &PgPool, token_hash: &str) -> sqlx::Result<()> {
    sqlx::query!("DELETE FROM sessions WHERE token_hash = $1", token_hash)
        .execute(pool)
        .await?;
    Ok(())
}
