//! GitHub App registrations for private repositories. The private key is
//! write-only: it is validated, sealed, and never returned. Projects link an
//! App installation under `/api/compose-projects/:id/git-access/github-app`.

use crate::auth::AdminUser;
use crate::db;
use crate::db::github_apps::GitHubAppRow;
use crate::error::{ApiError, ApiResult};
use crate::services::github_app::{parse_private_key, MAX_PRIVATE_KEY_LEN};
use crate::state::AppState;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{delete, get};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(list).post(create))
        .route("/:id", delete(remove))
}

async fn list(
    AdminUser(_): AdminUser,
    State(state): State<AppState>,
) -> ApiResult<Json<Vec<GitHubAppRow>>> {
    Ok(Json(db::github_apps::list(&state.db).await?))
}

#[derive(Deserialize)]
struct CreateAppBody {
    name: String,
    /// The numeric App ID from the App's settings page.
    app_id: i64,
    /// PEM private key downloaded from GitHub. Write-only.
    private_key: String,
}

async fn create(
    AdminUser(user): AdminUser,
    State(state): State<AppState>,
    Json(body): Json<CreateAppBody>,
) -> ApiResult<(StatusCode, Json<GitHubAppRow>)> {
    let name = body.name.trim();
    if name.is_empty() || name.len() > 100 {
        return Err(ApiError::BadRequest(
            "App name must be 1..=100 characters".into(),
        ));
    }
    if body.app_id <= 0 {
        return Err(ApiError::BadRequest("invalid GitHub App ID".into()));
    }
    if body.private_key.len() > MAX_PRIVATE_KEY_LEN {
        return Err(ApiError::BadRequest("private key is too large".into()));
    }
    parse_private_key(&body.private_key).map_err(ApiError::BadRequest)?;
    let id = Uuid::new_v4();
    let sealed = state
        .github_apps
        .seal_private_key(id, body.private_key.trim());
    let row = db::github_apps::create(&state.db, id, name, body.app_id, &sealed, user.id)
        .await
        .map_err(|e| match e.as_database_error() {
            Some(db) if db.is_unique_violation() => {
                ApiError::Conflict("a GitHub App with this name already exists".into())
            }
            _ => e.into(),
        })?;
    db::audit::record(
        &state.db,
        Some(user.id),
        "user",
        "create_github_app",
        Some(name),
        json!({ "github_app_id": id, "app_id": body.app_id }),
    )
    .await
    .ok();
    Ok((StatusCode::CREATED, Json(row)))
}

async fn remove(
    AdminUser(user): AdminUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    let app = db::github_apps::find(&state.db, id)
        .await?
        .ok_or_else(|| ApiError::NotFound("GitHub App not found".into()))?;
    let linked = db::github_apps::linked_projects(&state.db, id).await?;
    if !linked.is_empty() {
        return Err(ApiError::Conflict(format!(
            "the App is linked to {}; unlink it there first",
            linked.join(", ")
        )));
    }
    db::github_apps::delete(&state.db, id).await?;
    db::audit::record(
        &state.db,
        Some(user.id),
        "user",
        "remove_github_app",
        Some(&app.name),
        json!({ "github_app_id": id }),
    )
    .await
    .ok();
    Ok(StatusCode::NO_CONTENT)
}
