use crate::auth::AuthUser;
use crate::db;
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;
use axum::extract::{Path, State};
use axum::routing::{delete, get};
use axum::{Json, Router};
use panel_core::adapters::{RemoveCronJobParams, UpsertCronJobParams};
use panel_core::types::CronJob;
use serde::Deserialize;
use uuid::Uuid;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/site/:site_id", get(list_for_site).post(create))
        .route("/:id", delete(remove).patch(set_enabled))
}

async fn require_site_access(
    state: &AppState,
    user: &db::users::UserRow,
    site_id: Uuid,
) -> ApiResult<()> {
    let site = db::sites::find_by_id(&state.db, site_id)
        .await?
        .ok_or_else(|| ApiError::BadRequest("site not found".into()))?;
    if !user.is_admin && site.owner_user_id != user.id {
        return Err(ApiError::Forbidden);
    }
    Ok(())
}

/// Checks what becomes one crontab line: five schedule fields made of the
/// characters cron uses, and a single-line command.
fn validate_job(schedule: &str, command: &str) -> ApiResult<()> {
    let fields: Vec<&str> = schedule.split_whitespace().collect();
    let field_ok = |field: &&str| {
        field
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '*' | '/' | ',' | '-'))
    };
    if fields.len() != 5 || !fields.iter().all(field_ok) {
        return Err(ApiError::BadRequest(
            "the schedule needs five fields: minute, hour, day of month, month, day of week".into(),
        ));
    }
    let command = command.trim();
    if command.is_empty() || command.len() > 1000 || command.chars().any(char::is_control) {
        return Err(ApiError::BadRequest(
            "the command must be a single line of at most 1000 characters".into(),
        ));
    }
    Ok(())
}

/// The system account whose crontab holds the site's jobs.
async fn site_account(state: &AppState, site_id: Uuid) -> ApiResult<String> {
    db::sites::owner_username(&state.db, site_id)
        .await?
        .ok_or_else(|| ApiError::BadRequest("site not found".into()))
}

async fn list_for_site(
    AuthUser(user): AuthUser,
    State(state): State<AppState>,
    Path(site_id): Path<Uuid>,
) -> ApiResult<Json<Vec<CronJob>>> {
    require_site_access(&state, &user, site_id).await?;
    Ok(Json(db::cron::list_for_site(&state.db, site_id).await?))
}

#[derive(Deserialize)]
struct CreateCronBody {
    schedule: String,
    command: String,
}

async fn create(
    AuthUser(user): AuthUser,
    State(state): State<AppState>,
    Path(site_id): Path<Uuid>,
    Json(body): Json<CreateCronBody>,
) -> ApiResult<Json<CronJob>> {
    require_site_access(&state, &user, site_id).await?;
    validate_job(&body.schedule, &body.command)?;
    let owner_username = site_account(&state, site_id).await?;

    let job = db::cron::create(&state.db, site_id, &body.schedule, &body.command).await?;
    let installed = state
        .agent
        .upsert_cron_job(UpsertCronJobParams {
            owner_username,
            job_id: job.id,
            schedule: job.schedule.clone(),
            command: job.command.clone(),
            enabled: job.enabled,
        })
        .await;
    if let Err(error) = installed {
        // The agent refused it (for example an invalid schedule): do not
        // keep a job in the list that is not in the crontab.
        db::cron::delete(&state.db, job.id).await.ok();
        return Err(error.into());
    }
    Ok(Json(job))
}

#[derive(Deserialize)]
struct SetEnabledBody {
    enabled: bool,
}

async fn set_enabled(
    AuthUser(user): AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(body): Json<SetEnabledBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let job = db::cron::find_by_id(&state.db, id)
        .await?
        .ok_or_else(|| ApiError::BadRequest("cron job not found".into()))?;
    require_site_access(&state, &user, job.site_id).await?;
    let owner_username = site_account(&state, job.site_id).await?;

    db::cron::set_enabled(&state.db, id, body.enabled).await?;
    state
        .agent
        .upsert_cron_job(UpsertCronJobParams {
            owner_username,
            job_id: id,
            schedule: job.schedule,
            command: job.command,
            enabled: body.enabled,
        })
        .await?;
    Ok(Json(serde_json::json!({ "enabled": body.enabled })))
}

async fn remove(
    AuthUser(user): AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<serde_json::Value>> {
    let job = db::cron::find_by_id(&state.db, id)
        .await?
        .ok_or_else(|| ApiError::BadRequest("cron job not found".into()))?;
    require_site_access(&state, &user, job.site_id).await?;
    let owner_username = site_account(&state, job.site_id).await?;

    state
        .agent
        .remove_cron_job(RemoveCronJobParams {
            owner_username,
            job_id: id,
        })
        .await?;
    db::cron::delete(&state.db, id).await?;
    Ok(Json(serde_json::json!({ "deleted": true })))
}
