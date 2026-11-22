//! Admin-only user management routes.

use crate::auth::{password, AdminUser};
use crate::db::{self, users};
use crate::error::{ApiError, ApiResult};
use crate::services::user_purge;
use crate::state::AppState;
use axum::extract::{Path, Query, State};
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

#[derive(Deserialize, Default)]
struct DeleteUserQuery {
    /// Remove the user's sites and services from their hosts first.
    #[serde(default)]
    delete_resources: bool,
}

/// Deletes a panel user. Sessions, API keys, and role bindings go with the
/// user (`ON DELETE CASCADE`); audit entries keep the action with no actor.
///
/// A user who owns sites or services is refused (409) unless
/// `?delete_resources=true`: then each of them is torn down on its host
/// first (`services::user_purge`), so nothing is left running unmanaged.
/// Also refused: the caller's own account (400), the last installation
/// admin (409), and an installation admin when the caller is not one (403).
/// All of these are checked before anything is removed.
async fn delete_user(
    AdminUser(actor): AdminUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    query: Option<Query<DeleteUserQuery>>,
) -> ApiResult<Json<serde_json::Value>> {
    let Query(query) = query.unwrap_or_default();
    if id == actor.id {
        return Err(ApiError::BadRequest(
            "you cannot delete your own account".into(),
        ));
    }
    let target = users::find_by_id(&state.db, id)
        .await?
        .ok_or_else(|| ApiError::NotFound("user not found".into()))?;
    if target.is_admin && !actor.is_admin {
        return Err(ApiError::Forbidden);
    }
    if target.is_admin {
        let admins: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM users WHERE is_admin")
            .fetch_all(&state.db)
            .await?;
        if is_last_admin(&admins, id) {
            return Err(ApiError::Conflict(
                "cannot delete the last installation admin".into(),
            ));
        }
    }
    let owned = user_purge::owned(&state, id).await?;
    let purged = if owned.is_empty() {
        None
    } else if query.delete_resources {
        Some(user_purge::purge(&state, id, actor.id).await?)
    } else {
        return Err(ApiError::Conflict(format!(
            "user owns {} site(s) and {} service(s); delete them with the user (delete_resources=true) or first",
            owned.sites, owned.services
        )));
    };

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
            "user got {} site(s) and {} service(s) while being deleted; try again",
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
        serde_json::json!({ "username": target.username, "is_admin": target.is_admin, "removed": purged }),
    )
    .await
    .ok();
    Ok(Json(
        serde_json::json!({ "deleted": true, "removed": purged }),
    ))
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
