use crate::auth::AuthUser;
use crate::db;
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;
use axum::extract::{Path, State};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use panel_core::adapters::{CreateDatabaseParams, DeleteDatabaseParams};
use panel_core::types::{DatabaseEngine, DatabaseInfo};
use rand::Rng;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", post(create_database))
        .route("/site/:site_id", get(list_for_site))
        .route("/:id", delete(delete_database))
}

#[derive(Deserialize)]
struct CreateDatabaseBody {
    site_id: Option<Uuid>,
    engine: DatabaseEngine,
    name: String,
    username: String,
}

#[derive(Serialize)]
struct CreateDatabaseResponse {
    #[serde(flatten)]
    info: DatabaseInfo,
    /// Returned exactly once, at creation time; never persisted in plaintext.
    password: String,
}

fn generate_password() -> String {
    let charset: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let mut rng = rand::thread_rng();
    (0..24)
        .map(|_| charset[rng.gen_range(0..charset.len())] as char)
        .collect()
}

async fn create_database(
    AuthUser(user): AuthUser,
    State(state): State<AppState>,
    Json(body): Json<CreateDatabaseBody>,
) -> ApiResult<Json<CreateDatabaseResponse>> {
    if !panel_core::types::validate::is_valid_identifier(&body.name)
        || !panel_core::types::validate::is_valid_identifier(&body.username)
    {
        return Err(ApiError::BadRequest(
            "invalid database/user identifier".into(),
        ));
    }
    if let Some(site_id) = body.site_id {
        let site = db::sites::find_by_id(&state.db, site_id)
            .await?
            .ok_or_else(|| ApiError::BadRequest("site not found".into()))?;
        if !user.is_admin && site.owner_user_id != user.id {
            return Err(ApiError::Forbidden);
        }
    } else if !user.is_admin {
        return Err(ApiError::Forbidden);
    }

    let password = generate_password();
    state
        .agent
        .create_database(CreateDatabaseParams {
            engine: body.engine,
            name: body.name.clone(),
            username: body.username.clone(),
            password: password.clone(),
        })
        .await?;

    let info = db::databases::create(
        &state.db,
        body.site_id,
        body.engine,
        &body.name,
        &body.username,
    )
    .await?;

    db::audit::record(
        &state.db,
        Some(user.id),
        "user",
        "create_database",
        Some(&info.name),
        serde_json::json!({ "engine": body.engine.to_string() }),
    )
    .await
    .ok();

    Ok(Json(CreateDatabaseResponse { info, password }))
}

async fn list_for_site(
    AuthUser(user): AuthUser,
    State(state): State<AppState>,
    Path(site_id): Path<Uuid>,
) -> ApiResult<Json<Vec<DatabaseInfo>>> {
    let site = db::sites::find_by_id(&state.db, site_id)
        .await?
        .ok_or_else(|| ApiError::BadRequest("site not found".into()))?;
    if !user.is_admin && site.owner_user_id != user.id {
        return Err(ApiError::Forbidden);
    }
    Ok(Json(
        db::databases::list_for_site(&state.db, site_id).await?,
    ))
}

async fn delete_database(
    AuthUser(user): AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<serde_json::Value>> {
    if !user.is_admin {
        return Err(ApiError::Forbidden);
    }
    let dbs = sqlx::query!(
        "SELECT engine, name, username FROM databases WHERE id = $1",
        id
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| ApiError::BadRequest("database not found".into()))?;

    let engine: DatabaseEngine = dbs.engine.parse().map_err(ApiError::BadRequest)?;
    state
        .agent
        .delete_database(DeleteDatabaseParams {
            engine,
            name: dbs.name,
            username: dbs.username,
        })
        .await?;
    db::databases::delete(&state.db, id).await?;
    Ok(Json(serde_json::json!({ "deleted": true })))
}
