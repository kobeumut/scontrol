use crate::auth::AuthUser;
use crate::db;
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;
use axum::extract::{Path, State};
use axum::routing::post;
use axum::{Json, Router};
use panel_core::adapters::IssueCertificateParams;
use serde::Deserialize;
use uuid::Uuid;

pub fn router() -> Router<AppState> {
    Router::new().route("/site/:site_id", post(issue))
}

#[derive(Deserialize)]
struct IssueCertBody {
    admin_email: String,
}

async fn issue(
    AuthUser(user): AuthUser,
    State(state): State<AppState>,
    Path(site_id): Path<Uuid>,
    Json(body): Json<IssueCertBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let site = db::sites::find_by_id(&state.db, site_id)
        .await?
        .ok_or_else(|| ApiError::BadRequest("site not found".into()))?;
    if !user.is_admin && site.owner_user_id != user.id {
        return Err(ApiError::Forbidden);
    }

    state
        .agent
        .issue_certificate(IssueCertificateParams {
            domain: site.domain.clone(),
            admin_email: body.admin_email,
        })
        .await?;
    db::sites::set_ssl_enabled(&state.db, site_id, true).await?;
    db::audit::record(
        &state.db,
        Some(user.id),
        "user",
        "issue_certificate",
        Some(&site.domain),
        serde_json::json!({}),
    )
    .await
    .ok();
    Ok(Json(serde_json::json!({ "ssl_enabled": true })))
}
