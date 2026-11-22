//! Admin-only user management routes.

use crate::auth::{password, AdminUser};
use crate::db::{self, users};
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;
use axum::extract::{Path, State};
use axum::routing::post;
use axum::{Json, Router};
use panel_core::types::User;
use serde::Deserialize;
use uuid::Uuid;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/users", axum::routing::get(list_users).post(create_user))
        .route("/users/:id", axum::routing::delete(delete_user))
        .route("/users/:id/disable-totp", post(disable_totp))
}

async fn list_users(
    AdminUser(_): AdminUser,
    State(state): State<AppState>,
) -> ApiResult<Json<Vec<User>>> {
    let rows = sqlx::query_as!(
        users::UserRow,
        r#"SELECT id, username, email, password_hash, is_admin, totp_secret, totp_enabled, created_at, updated_at
           FROM users ORDER BY created_at ASC"#
    )
    .fetch_all(&state.db)
    .await?;
    Ok(Json(rows.iter().map(users::UserRow::to_public).collect()))
}

#[derive(Deserialize)]
struct CreateUserBody {
    username: String,
    email: String,
    password: String,
    is_admin: bool,
}

async fn create_user(
    AdminUser(actor): AdminUser,
    State(state): State<AppState>,
    Json(body): Json<CreateUserBody>,
) -> ApiResult<Json<User>> {
    // `system:admin` through a role must not mint an installation admin,
    // who holds every permission (including `access:manage`).
    if body.is_admin && !actor.is_admin {
        return Err(ApiError::Forbidden);
    }
    if !panel_core::types::validate::is_valid_username(&body.username) {
        return Err(ApiError::BadRequest("invalid username".into()));
    }
    if body.password.len() < 12 {
        return Err(ApiError::BadRequest(
            "password must be at least 12 characters".into(),
        ));
    }
    let hash = password::hash_password(&body.password)
        .map_err(|e| ApiError::Internal(anyhow::anyhow!(e)))?;
    let user = users::create_user(&state.db, &body.username, &body.email, &hash, body.is_admin)
        .await
        .map_err(|error| match error.as_database_error() {
            Some(db) if db.is_unique_violation() => {
                ApiError::Conflict("a user with that username or email already exists".into())
            }
            _ => error.into(),
        })?;
    Ok(Json(user.to_public()))
}

async fn disable_totp(
    AdminUser(actor): AdminUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<serde_json::Value>> {
    let target = users::find_by_id(&state.db, id)
        .await?
        .ok_or_else(|| ApiError::NotFound("user not found".into()))?;
    // Only an installation admin may weaken another installation admin.
    if target.is_admin && !actor.is_admin {
        return Err(ApiError::Forbidden);
    }
    users::disable_totp(&state.db, id).await?;
    Ok(Json(serde_json::json!({ "totp_enabled": false })))
}

/// Deletes a panel user. Sessions, API keys, and role bindings go with the
/// user (`ON DELETE CASCADE`); audit entries keep the action with no actor.
///
/// Refused when the user is the caller (400), the last installation admin
/// (409), or still owns sites or Compose services (409): those rows cascade
/// too, which would leave their host resources (system users, vhosts,
/// containers) running with nothing in the panel to manage them.
async fn delete_user(
    AdminUser(actor): AdminUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<serde_json::Value>> {
    if id == actor.id {
        return Err(ApiError::BadRequest(
            "you cannot delete your own account".into(),
        ));
    }
    let mut tx = state.db.begin().await?;
    // Lock every admin row so two concurrent deletions cannot each see
    // another admin left and together remove the last one.
    let admins: Vec<Uuid> =
        sqlx::query_scalar!("SELECT id FROM users WHERE is_admin ORDER BY id FOR UPDATE")
            .fetch_all(&mut *tx)
            .await?;
    let target = sqlx::query!(
        "SELECT username, is_admin FROM users WHERE id = $1 FOR UPDATE",
        id
    )
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| ApiError::NotFound("user not found".into()))?;
    // Only an installation admin may remove another installation admin.
    if target.is_admin && !actor.is_admin {
        return Err(ApiError::Forbidden);
    }
    if target.is_admin && is_last_admin(&admins, id) {
        return Err(ApiError::Conflict(
            "cannot delete the last installation admin".into(),
        ));
    }
    let owned = sqlx::query!(
        r#"SELECT (SELECT COUNT(*) FROM sites WHERE owner_user_id = $1) AS "sites!",
                  (SELECT COUNT(*) FROM compose_projects WHERE owner_user_id = $1) AS "services!""#,
        id
    )
    .fetch_one(&mut *tx)
    .await?;
    if owned.sites > 0 || owned.services > 0 {
        return Err(ApiError::Conflict(format!(
            "user still owns {} site(s) and {} service(s); delete them first",
            owned.sites, owned.services
        )));
    }
    sqlx::query!("DELETE FROM users WHERE id = $1", id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    db::audit::record(
        &state.db,
        Some(actor.id),
        "user",
        "delete_user",
        Some(&id.to_string()),
        serde_json::json!({ "username": target.username, "is_admin": target.is_admin }),
    )
    .await
    .ok();
    Ok(Json(serde_json::json!({ "deleted": true })))
}

/// Whether removing `id` leaves no installation admin.
fn is_last_admin(admins: &[Uuid], id: Uuid) -> bool {
    admins.iter().all(|admin| *admin == id)
}

#[cfg(test)]
mod tests {
    use super::is_last_admin;
    use uuid::Uuid;

    #[test]
    fn last_admin_is_detected() {
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        assert!(is_last_admin(&[a], a));
        assert!(!is_last_admin(&[a, b], a));
        assert!(!is_last_admin(&[a, b], b));
    }
}
