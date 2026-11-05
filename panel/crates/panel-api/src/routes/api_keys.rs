//! API keys for automation and CI (`/api/api-keys`).
//!
//! Keys are created, listed, and revoked from a session only (never with
//! another key). The token is returned once, at creation; the panel keeps an
//! Argon2id hash of its secret. A key's grants must be held by its owner
//! when it is created, and every request is checked against both the key
//! and the owner's current grants.

use crate::auth::api_keys as tokens;
use crate::auth::SessionUser;
use crate::db;
use crate::error::{ApiError, ApiResult};
use crate::routes::access::{audit, scope_target};
use crate::state::AppState;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{delete, get};
use axum::{Json, Router};
use panel_core::rbac::{self, Grant, Permission, Target};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(list).post(create))
        .route("/:id", delete(revoke))
}

#[derive(Deserialize)]
struct ListQuery {
    /// Every user's keys (needs `access:manage`).
    #[serde(default)]
    all: bool,
}

async fn list(
    SessionUser(principal): SessionUser,
    State(state): State<AppState>,
    Query(query): Query<ListQuery>,
) -> ApiResult<Json<Vec<db::api_keys::ApiKey>>> {
    let owner = if query.all {
        principal.require(Permission::AccessManage, Target::Global)?;
        None
    } else {
        Some(principal.id)
    };
    Ok(Json(db::api_keys::list(&state.db, owner).await?))
}

#[derive(Deserialize)]
struct CreateBody {
    name: String,
    grants: Vec<Grant>,
    /// Days until the key stops working; never when omitted.
    #[serde(default)]
    expires_in_days: Option<u32>,
}

#[derive(Serialize)]
struct Created {
    key: db::api_keys::ApiKey,
    /// The token, shown this once.
    token: String,
}

async fn create(
    SessionUser(principal): SessionUser,
    State(state): State<AppState>,
    Json(mut body): Json<CreateBody>,
) -> ApiResult<(StatusCode, Json<Created>)> {
    let name = body.name.trim();
    if name.is_empty() || name.len() > 100 {
        return Err(ApiError::BadRequest("name must be 1–100 characters".into()));
    }
    body.grants
        .sort_by_key(|g| (g.permission, format!("{:?}", g.scope)));
    body.grants.dedup();
    rbac::validate_key_grants(&body.grants).map_err(ApiError::BadRequest)?;
    for grant in &body.grants {
        let target = scope_target(&state, grant.scope).await?;
        if !principal.allows(grant.permission, target) {
            return Err(ApiError::BadRequest(format!(
                "you do not hold {} at that scope, so a key of yours cannot",
                grant.permission
            )));
        }
    }
    let expires_at = match body.expires_in_days {
        None => None,
        Some(days @ 1..=3650) => Some(chrono::Utc::now() + chrono::Duration::days(i64::from(days))),
        Some(_) => {
            return Err(ApiError::BadRequest(
                "expires_in_days must be between 1 and 3650".into(),
            ))
        }
    };
    let token = tokio::task::spawn_blocking(tokens::generate)
        .await
        .map_err(|e| anyhow::anyhow!(e))??;
    let key = db::api_keys::insert(
        &state.db,
        Uuid::new_v4(),
        principal.id,
        name,
        &token.prefix,
        &token.secret_hash,
        &body.grants,
        expires_at,
    )
    .await?;
    audit(
        &state,
        &principal,
        "create_api_key",
        name,
        serde_json::json!({ "api_key_id": key.id, "prefix": key.prefix, "grants": key.grants }),
    )
    .await;
    Ok((
        StatusCode::CREATED,
        Json(Created {
            key,
            token: token.token,
        }),
    ))
}

/// Revokes a key at once. Your own keys, or anyone's with `access:manage`.
async fn revoke(
    SessionUser(principal): SessionUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    let key = db::api_keys::find(&state.db, id)
        .await?
        .ok_or_else(|| ApiError::NotFound("API key not found".into()))?;
    if key.user_id != principal.id {
        principal.require(Permission::AccessManage, Target::Global)?;
    }
    db::api_keys::revoke(&state.db, id).await?;
    audit(
        &state,
        &principal,
        "revoke_api_key",
        &key.name,
        serde_json::json!({ "api_key_id": id, "prefix": key.prefix }),
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}
