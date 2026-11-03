use crate::auth::AuthUser;
use crate::db;
use crate::error::{ApiError, ApiResult};
use crate::services::site_service::{self, CreateSiteRequest};
use crate::state::AppState;
use axum::extract::{Path, State};
use axum::routing::{delete, get};
use axum::{Json, Router};
use panel_core::types::{Runtime, Site};
use serde::Deserialize;
use uuid::Uuid;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(list_sites).post(create_site))
        .route("/:id", delete(delete_site))
}

async fn list_sites(
    AuthUser(user): AuthUser,
    State(state): State<AppState>,
) -> ApiResult<Json<Vec<Site>>> {
    let sites = if user.is_admin {
        db::sites::list_all(&state.db).await?
    } else {
        db::sites::list_for_owner(&state.db, user.id).await?
    };
    Ok(Json(sites))
}

#[derive(Deserialize)]
struct CreateSiteBody {
    domain: String,
    owner_username: String,
    runtime: Runtime,
}

async fn create_site(
    AuthUser(user): AuthUser,
    State(state): State<AppState>,
    Json(body): Json<CreateSiteBody>,
) -> ApiResult<Json<Site>> {
    let site = site_service::create_site(
        &state.db,
        state.agent.as_ref(),
        CreateSiteRequest {
            domain: body.domain,
            owner_user_id: user.id,
            owner_username: body.owner_username,
            runtime: body.runtime,
        },
    )
    .await?;

    db::audit::record(
        &state.db,
        Some(user.id),
        "user",
        "create_site",
        Some(&site.domain),
        serde_json::json!({ "site_id": site.id }),
    )
    .await
    .ok();

    Ok(Json(site))
}

async fn delete_site(
    AuthUser(user): AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<serde_json::Value>> {
    let site = db::sites::find_by_id(&state.db, id)
        .await?
        .ok_or_else(|| ApiError::BadRequest("site not found".into()))?;
    if !user.is_admin && site.owner_user_id != user.id {
        return Err(ApiError::Forbidden);
    }

    site_service::delete_site(&state.db, state.agent.as_ref(), id).await?;
    db::audit::record(
        &state.db,
        Some(user.id),
        "user",
        "delete_site",
        Some(&id.to_string()),
        serde_json::json!({}),
    )
    .await
    .ok();
    Ok(Json(serde_json::json!({ "deleted": true })))
}
