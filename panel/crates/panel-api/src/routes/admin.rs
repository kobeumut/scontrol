//! Admin-only user management routes.

use crate::auth::{password, AdminUser};
use crate::db::users;
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
    let user =
        users::create_user(&state.db, &body.username, &body.email, &hash, body.is_admin).await?;
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
