//! Dokploy-inspired Docker Compose project API.
//!
//! Environment values are accepted only by deploy requests and forwarded to
//! the agent; they are never persisted, included in responses, audit JSON,
//! or logs owned by this API.

use crate::auth::AdminUser;
use crate::db;
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;
use axum::extract::{Path, Query, State};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use panel_core::adapters::{
    ComposeProjectActionParams, ComposeProjectLogsParams, DeployComposeProjectParams,
};
use panel_core::types::{
    validate, ComposeProject, ComposeServiceInfo, ComposeSource, DeploymentInfo, DeploymentStatus,
};
use serde::Deserialize;
use uuid::Uuid;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(list).post(create_and_deploy))
        .route("/:id", delete(remove))
        .route("/:id/deploy", post(deploy))
        .route("/:id/stop", post(stop))
        .route("/:id/services", get(services))
        .route("/:id/deployments", get(deployments))
        .route("/:id/logs", get(logs))
}

async fn list(
    AdminUser(user): AdminUser,
    State(state): State<AppState>,
) -> ApiResult<Json<Vec<ComposeProject>>> {
    // Admin is currently the only Docker-project actor; still keep this
    // owner-aware branch to retain the correct model for future RBAC.
    let projects = if user.is_admin {
        db::compose_projects::list_all(&state.db).await?
    } else {
        db::compose_projects::list_for_owner(&state.db, user.id).await?
    };
    Ok(Json(projects))
}

#[derive(Deserialize)]
struct CreateProjectBody {
    name: String,
    slug: String,
    source: ComposeSource,
    /// Write-only values; never returned or persisted in PostgreSQL.
    #[serde(default)]
    environment: Vec<(String, String)>,
    #[serde(default)]
    pull_images: bool,
}

async fn create_and_deploy(
    AdminUser(user): AdminUser,
    State(state): State<AppState>,
    Json(body): Json<CreateProjectBody>,
) -> ApiResult<Json<ComposeProject>> {
    validate_project_input(&body.name, &body.slug, &body.environment)?;
    let id = Uuid::new_v4();
    let project =
        db::compose_projects::create(&state.db, id, &body.name, &body.slug, user.id, &body.source)
            .await?;

    match deploy_project(&state, &project, body.environment, body.pull_images).await {
        Ok(_) => {
            db::audit::record(
                &state.db, Some(user.id), "user", "create_compose_project", Some(&project.slug),
                serde_json::json!({ "project_id": project.id, "source_kind": source_kind(&project.source) }),
            ).await.ok();
            let mut deployed = project;
            deployed.status = DeploymentStatus::Succeeded;
            Ok(Json(deployed))
        }
        Err(error) => {
            // Preserve the failed project/deployment so operators can inspect
            // its status and agent logs, but never store raw env values.
            db::compose_projects::set_status(&state.db, id, DeploymentStatus::Failed)
                .await
                .ok();
            Err(error)
        }
    }
}

#[derive(Deserialize)]
struct DeployProjectBody {
    #[serde(default)]
    environment: Vec<(String, String)>,
    #[serde(default)]
    pull_images: bool,
}

async fn deploy(
    AdminUser(_): AdminUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(body): Json<DeployProjectBody>,
) -> ApiResult<Json<Vec<ComposeServiceInfo>>> {
    validate_environment(&body.environment)?;
    let project = find_project(&state, id).await?;
    Ok(Json(
        deploy_project(&state, &project, body.environment, body.pull_images).await?,
    ))
}

async fn stop(
    AdminUser(_): AdminUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<serde_json::Value>> {
    let project = find_project(&state, id).await?;
    state
        .agent
        .stop_compose_project(ComposeProjectActionParams {
            project_slug: project.slug,
        })
        .await?;
    db::compose_projects::set_status(&state.db, id, DeploymentStatus::Stopped).await?;
    Ok(Json(serde_json::json!({ "stopped": true })))
}

async fn remove(
    AdminUser(_): AdminUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<serde_json::Value>> {
    let project = find_project(&state, id).await?;
    state
        .agent
        .remove_compose_project(ComposeProjectActionParams {
            project_slug: project.slug,
        })
        .await?;
    db::compose_projects::delete(&state.db, id).await?;
    Ok(Json(serde_json::json!({ "deleted": true })))
}

async fn services(
    AdminUser(_): AdminUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Vec<ComposeServiceInfo>>> {
    let project = find_project(&state, id).await?;
    Ok(Json(
        state
            .agent
            .list_compose_project_services(ComposeProjectActionParams {
                project_slug: project.slug,
            })
            .await?,
    ))
}

async fn deployments(
    AdminUser(_): AdminUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Vec<DeploymentInfo>>> {
    let _ = find_project(&state, id).await?;
    Ok(Json(
        db::compose_projects::list_deployments(&state.db, id).await?,
    ))
}

#[derive(Deserialize)]
struct LogQuery {
    tail: Option<u16>,
}

async fn logs(
    AdminUser(_): AdminUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Query(query): Query<LogQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let project = find_project(&state, id).await?;
    let logs = state
        .agent
        .compose_project_logs(ComposeProjectLogsParams {
            project_slug: project.slug,
            tail_lines: query.tail.unwrap_or(200),
        })
        .await?;
    Ok(Json(serde_json::json!({ "logs": logs })))
}

async fn find_project(state: &AppState, id: Uuid) -> ApiResult<ComposeProject> {
    db::compose_projects::find_by_id(&state.db, id)
        .await?
        .ok_or_else(|| ApiError::BadRequest("Compose project not found".into()))
}

async fn deploy_project(
    state: &AppState,
    project: &ComposeProject,
    environment: Vec<(String, String)>,
    pull_images: bool,
) -> ApiResult<Vec<ComposeServiceInfo>> {
    let deployment_id = Uuid::new_v4();
    let log_path = format!("agent://compose/{}/logs/latest", project.slug);
    db::compose_projects::create_deployment(&state.db, deployment_id, project.id, &log_path)
        .await?;
    db::compose_projects::set_status(&state.db, project.id, DeploymentStatus::Running).await?;

    let result = state
        .agent
        .deploy_compose_project(DeployComposeProjectParams {
            project_id: project.id,
            project_slug: project.slug.clone(),
            source: project.source.clone(),
            environment,
            pull_images,
        })
        .await;

    match result {
        Ok(services) => {
            db::compose_projects::finish_deployment(
                &state.db,
                deployment_id,
                DeploymentStatus::Succeeded,
                None,
            )
            .await?;
            db::compose_projects::set_status(&state.db, project.id, DeploymentStatus::Succeeded)
                .await?;
            Ok(services)
        }
        Err(error) => {
            // Error text is bounded; don't let an external tool's complete
            // output turn the database into a log sink.
            let message = error.to_string();
            let bounded = &message[..message.len().min(1_024)];
            db::compose_projects::finish_deployment(
                &state.db,
                deployment_id,
                DeploymentStatus::Failed,
                Some(bounded),
            )
            .await
            .ok();
            db::compose_projects::set_status(&state.db, project.id, DeploymentStatus::Failed)
                .await
                .ok();
            Err(error.into())
        }
    }
}

fn validate_project_input(
    name: &str,
    slug: &str,
    environment: &[(String, String)],
) -> ApiResult<()> {
    if name.trim().is_empty() || name.len() > 100 {
        return Err(ApiError::BadRequest(
            "project name must be 1..=100 characters".into(),
        ));
    }
    if !validate::is_valid_project_slug(slug) {
        return Err(ApiError::BadRequest("invalid project slug".into()));
    }
    validate_environment(environment)
}

fn validate_environment(environment: &[(String, String)]) -> ApiResult<()> {
    if environment.len() > 256
        || environment.iter().any(|(key, value)| {
            !validate::is_valid_env_key(key)
                || value.contains('\0')
                || value.contains('\n')
                || value.len() > 16 * 1024
        })
    {
        return Err(ApiError::BadRequest("invalid project environment".into()));
    }
    Ok(())
}

fn source_kind(source: &ComposeSource) -> &'static str {
    match source {
        ComposeSource::Git { .. } => "git",
        ComposeSource::Raw { .. } => "raw",
    }
}
