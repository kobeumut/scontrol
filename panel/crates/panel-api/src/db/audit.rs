use sqlx::PgPool;
use uuid::Uuid;

pub async fn record(
    pool: &PgPool,
    actor_user_id: Option<Uuid>,
    actor_kind: &str,
    action: &str,
    target: Option<&str>,
    metadata: serde_json::Value,
) -> sqlx::Result<()> {
    sqlx::query!(
        r#"INSERT INTO audit_log (actor_user_id, actor_kind, action, target, metadata)
           VALUES ($1, $2, $3, $4, $5)"#,
        actor_user_id,
        actor_kind,
        action,
        target,
        metadata,
    )
    .execute(pool)
    .await?;
    Ok(())
}
