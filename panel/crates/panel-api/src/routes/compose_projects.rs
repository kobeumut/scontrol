//! Dokploy-inspired Docker Compose project API.
//!
//! Deployments are asynchronous: deploy endpoints queue a job and answer
//! `202 Accepted` with the deployment record, and clients follow progress
//! through the deployment status and its incremental log.
//!
//! Environment values are write-only. They are accepted by
//! create/deploy/environment/variables requests and stored only sealed (the
//! service layer of `services::env_layers`); they are never included in
//! responses, queue rows, audit JSON, or logs owned by this API. The queue
//! worker merges project → environment → service layers and writes the
//! result to the agent right before each deployment.
//! Git HTTPS tokens follow the same rule, and a deploy key's private half
//! never leaves the agent. A linked GitHub App installation stores only the
//! App and installation IDs; tokens are minted per deployment.

use crate::auth::AdminUser;
use crate::db;
use crate::db::projects::EnvScope;
use crate::error::{ApiError, ApiResult};
use crate::services::env_layers::{self, ServiceVariables, VariableInput};
use crate::state::AppState;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use panel_core::adapters::{
    CancelDeploymentParams, ComposeProjectActionParams, ComposeProjectLogsParams,
    DeploymentLogParams, FollowComposeLogsParams, GitHttpCredentials, WriteGitCredentialsParams,
};
use panel_core::types::validate::GitTransport;
use panel_core::types::{
    validate, ComposeDomain, ComposeProject, ComposeServiceInfo, ComposeSource, DeploymentInfo,
    DeploymentLogChunk, DeploymentStatus,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(list).post(create))
        .route("/:id", get(show).delete(remove))
        .route("/:id/deploy", post(deploy))
        .route("/:id/environment", put(replace_environment))
        .route("/:id/variables", get(variables).put(update_variables))
        .route("/:id/stop", post(stop))
        .route("/:id/services", get(services))
        .route("/:id/logs", get(logs))
        .route("/:id/logs/stream", get(logs_stream))
        .route("/:id/deployments", get(deployments))
        .route("/:id/deployments/:deployment_id", get(deployment))
        .route("/:id/deployments/:deployment_id/log", get(deployment_log))
        .route(
            "/:id/deployments/:deployment_id/cancel",
            post(cancel_deployment),
        )
        .route("/:id/domains", get(domains).post(add_domain))
        .route("/:id/domains/:domain_id", delete(remove_domain))
        .route("/:id/git-access", get(git_access))
        .route(
            "/:id/git-access/deploy-key",
            post(generate_deploy_key).delete(remove_deploy_key),
        )
        .route(
            "/:id/git-access/token",
            put(set_git_token).delete(remove_git_token),
        )
        .route(
            "/:id/registry",
            get(project_registry).put(set_project_registry),
        )
        .route(
            "/:id/git-access/github-app",
            put(link_github_app).delete(unlink_github_app),
        )
        .route(
            "/:id/webhook",
            get(webhook_status)
                .post(rotate_webhook)
                .put(update_webhook)
                .delete(disable_webhook),
        )
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

async fn show(
    AdminUser(_): AdminUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<ComposeProject>> {
    Ok(Json(find_project(&state, id).await?))
}

#[derive(Deserialize)]
struct CreateProjectBody {
    name: String,
    slug: String,
    source: ComposeSource,
    /// Environment (of a project) the service belongs to; the owner's
    /// default project's `production` environment when omitted.
    #[serde(default)]
    environment_id: Option<Uuid>,
    /// Write-only values of the service layer; stored sealed, never returned.
    #[serde(default)]
    environment: Vec<(String, String)>,
    /// Write-only HTTPS token for a private Git remote.
    #[serde(default)]
    git_credentials: Option<GitHttpCredentials>,
    /// GitHub App installation that mints a token per deployment.
    #[serde(default)]
    github_app: Option<GitHubAppLinkBody>,
    #[serde(default)]
    pull_images: bool,
    /// Private registry to pull images with (see `/api/registries`).
    #[serde(default)]
    registry_id: Option<Uuid>,
    /// Queue the first deployment right away (default true).
    #[serde(default = "default_true")]
    deploy: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Serialize)]
struct CreatedProject {
    project: ComposeProject,
    deployment: Option<DeploymentInfo>,
}

async fn create(
    AdminUser(user): AdminUser,
    State(state): State<AppState>,
    Json(body): Json<CreateProjectBody>,
) -> ApiResult<(StatusCode, Json<CreatedProject>)> {
    validate_project_input(&body.name, &body.slug, &body.environment)?;
    validate_source(&body.source)?;
    let token_host = match &body.git_credentials {
        Some(credentials) => {
            validate_git_credentials(credentials)?;
            Some(https_host(&body.source)?)
        }
        None => None,
    };
    if let Some(environment_id) = body.environment_id {
        db::projects::find_environment(&state.db, environment_id)
            .await?
            .ok_or_else(|| ApiError::BadRequest("environment not found".into()))?;
    }
    if let Some(registry_id) = body.registry_id {
        if db::registries::find(&state.db, registry_id)
            .await?
            .is_none()
        {
            return Err(ApiError::BadRequest("registry not found".into()));
        }
    }
    if let Some(link) = &body.github_app {
        check_github_app_link(&state, &body.source, link).await?;
    }
    let project = db::compose_projects::create(
        &state.db,
        Uuid::new_v4(),
        &body.name,
        &body.slug,
        user.id,
        body.environment_id,
        &body.source,
    )
    .await
    .map_err(|e| conflict_on_unique(e, "a project with this slug already exists"))?;

    // The row exists first so the slug is reserved before the agent writes
    // anything under it; if a step fails, the row is rolled back.
    let mut prepared = env_layers::replace(
        &state.db,
        &state.secret_box,
        EnvScope::Service(project.id),
        &body.environment,
    )
    .await;
    if let (Ok(()), Some(host)) = (&prepared, &token_host) {
        prepared = write_git_token(&state, &project, host, body.git_credentials).await;
    }
    if prepared.is_ok() && body.registry_id.is_some() {
        prepared = db::registries::set_project_registry(&state.db, project.id, body.registry_id)
            .await
            .map_err(ApiError::from);
    }
    if let (Ok(()), Some(link)) = (&prepared, &body.github_app) {
        prepared = db::compose_projects::set_github_app(
            &state.db,
            project.id,
            Some((link.github_app_id, link.installation_id)),
        )
        .await
        .map_err(Into::into);
    }
    if let Err(error) = prepared {
        // Removes whatever the agent already wrote under the slug.
        state
            .agent
            .remove_compose_project(ComposeProjectActionParams {
                project_slug: project.slug.clone(),
            })
            .await
            .ok();
        db::compose_projects::delete(&state.db, project.id)
            .await
            .ok();
        return Err(error);
    }
    let deployment = if body.deploy {
        Some(
            state
                .deploy_queue
                .enqueue(
                    &state.db,
                    project.id,
                    &project.slug,
                    body.pull_images,
                    "manual",
                )
                .await?,
        )
    } else {
        None
    };
    db::audit::record(
        &state.db,
        Some(user.id),
        "user",
        "create_compose_project",
        Some(&project.slug),
        serde_json::json!({ "project_id": project.id, "source_kind": source_kind(&project.source) }),
    )
    .await
    .ok();
    let project = find_project(&state, project.id).await?;
    Ok((
        StatusCode::ACCEPTED,
        Json(CreatedProject {
            project,
            deployment,
        }),
    ))
}

#[derive(Deserialize, Default)]
struct DeployProjectBody {
    /// Replaces the environment before deploying; omit to keep it.
    #[serde(default)]
    environment: Option<Vec<(String, String)>>,
    #[serde(default)]
    pull_images: bool,
}

async fn deploy(
    AdminUser(user): AdminUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    body: Option<Json<DeployProjectBody>>,
) -> ApiResult<(StatusCode, Json<DeploymentInfo>)> {
    let body = body.map(|Json(body)| body).unwrap_or_default();
    let project = find_project(&state, id).await?;
    if let Some(environment) = body.environment {
        env_layers::replace(
            &state.db,
            &state.secret_box,
            EnvScope::Service(project.id),
            &environment,
        )
        .await?;
    }
    let deployment = state
        .deploy_queue
        .enqueue(
            &state.db,
            project.id,
            &project.slug,
            body.pull_images,
            "manual",
        )
        .await?;
    db::audit::record(
        &state.db,
        Some(user.id),
        "user",
        "deploy_compose_project",
        Some(&project.slug),
        serde_json::json!({ "project_id": project.id, "deployment_id": deployment.id }),
    )
    .await
    .ok();
    Ok((StatusCode::ACCEPTED, Json(deployment)))
}

#[derive(Deserialize)]
struct EnvironmentBody {
    environment: Vec<(String, String)>,
}

/// Replaces the write-only environment; it applies on the next deployment.
async fn replace_environment(
    AdminUser(user): AdminUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(body): Json<EnvironmentBody>,
) -> ApiResult<StatusCode> {
    validate_environment(&body.environment)?;
    let project = find_project(&state, id).await?;
    let keys: Vec<&str> = body.environment.iter().map(|(k, _)| k.as_str()).collect();
    let audit = serde_json::json!({ "project_id": project.id, "keys": keys });
    env_layers::replace(
        &state.db,
        &state.secret_box,
        EnvScope::Service(project.id),
        &body.environment,
    )
    .await?;
    db::audit::record(
        &state.db,
        Some(user.id),
        "user",
        "replace_compose_environment",
        Some(&project.slug),
        audit,
    )
    .await
    .ok();
    Ok(StatusCode::NO_CONTENT)
}

/// Key names of the service's own, environment, and project variables, and
/// which layer wins per key. Values are never returned.
async fn variables(
    AdminUser(_): AdminUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<ServiceVariables>> {
    let project = find_project(&state, id).await?;
    Ok(Json(
        env_layers::service_variables(&state.db, &project).await?,
    ))
}

#[derive(Deserialize)]
pub(crate) struct VariablesBody {
    pub variables: Vec<VariableInput>,
}

/// Replaces the service layer. An entry without `value` keeps the stored
/// value; keys left out are removed. Applies on the next deployment.
async fn update_variables(
    AdminUser(user): AdminUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(body): Json<VariablesBody>,
) -> ApiResult<Json<ServiceVariables>> {
    let project = find_project(&state, id).await?;
    let keys = env_layers::update(
        &state.db,
        &state.secret_box,
        EnvScope::Service(project.id),
        body.variables,
    )
    .await?;
    db::audit::record(
        &state.db,
        Some(user.id),
        "user",
        "update_service_variables",
        Some(&project.slug),
        serde_json::json!({ "project_id": project.id, "keys": keys }),
    )
    .await
    .ok();
    let project = find_project(&state, id).await?;
    Ok(Json(
        env_layers::service_variables(&state.db, &project).await?,
    ))
}

async fn stop(
    AdminUser(_): AdminUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<serde_json::Value>> {
    let project = find_project(&state, id).await?;
    if project.status == DeploymentStatus::Running {
        return Err(ApiError::Conflict(
            "a deployment is running; wait for it to finish".into(),
        ));
    }
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
    AdminUser(user): AdminUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<serde_json::Value>> {
    let project = find_project(&state, id).await?;
    if project.status == DeploymentStatus::Running {
        return Err(ApiError::Conflict(
            "a deployment is running; wait for it to finish".into(),
        ));
    }
    match state
        .agent
        .remove_compose_project(ComposeProjectActionParams {
            project_slug: project.slug.clone(),
        })
        .await
    {
        // Never deployed (or files already gone): nothing to tear down.
        Ok(()) | Err(panel_core::error::CoreError::NotFound(_)) => {}
        Err(error) => return Err(error.into()),
    }
    db::compose_projects::delete(&state.db, id).await?;
    db::audit::record(
        &state.db,
        Some(user.id),
        "user",
        "remove_compose_project",
        Some(&project.slug),
        serde_json::json!({ "project_id": project.id }),
    )
    .await
    .ok();
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

#[derive(Deserialize)]
struct LogQuery {
    tail: Option<u16>,
}

/// Bounded tail of the running containers' logs.
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

#[derive(Deserialize)]
struct LogStreamQuery {
    service: Option<String>,
    tail: Option<u16>,
    search: Option<String>,
    #[serde(default)]
    timestamps: bool,
}

/// Live container logs as Server-Sent Events: one `message` event per
/// line, then `end` when the follower stops, or `stream-error` if it failed
/// (not `error`, which `EventSource` reserves for connection errors). The
/// agent stops `docker compose logs --follow` when this response is
/// dropped, i.e. when the browser disconnects.
async fn logs_stream(
    AdminUser(_): AdminUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Query(query): Query<LogStreamQuery>,
) -> ApiResult<impl axum::response::IntoResponse> {
    use axum::response::sse::{Event, KeepAlive, Sse};

    let project = find_project(&state, id).await?;
    let service = query.service.filter(|s| !s.is_empty());
    if service
        .as_deref()
        .is_some_and(|s| !validate::is_valid_service_name(s))
    {
        return Err(ApiError::BadRequest("invalid service name".into()));
    }
    let rx = state
        .agent
        .follow_compose_logs(FollowComposeLogsParams {
            project_slug: project.slug,
            service,
            tail_lines: query.tail.unwrap_or(200).min(2_000),
            search: query.search.filter(|s| !s.trim().is_empty()),
            timestamps: query.timestamps,
        })
        .await?;

    // SSE cannot carry a carriage return inside an event (axum panics on
    // one), and progress-bar output is full of them.
    let event = |name: Option<&str>, data: &str| {
        let event = Event::default().data(data.replace('\r', ""));
        Ok::<_, std::convert::Infallible>(match name {
            Some(name) => event.event(name),
            None => event,
        })
    };
    let stream = futures::stream::unfold(Some(rx), move |rx| async move {
        let mut rx = rx?;
        Some(match rx.recv().await {
            Some(Ok(line)) => (event(None, &line), Some(rx)),
            Some(Err(error)) => (event(Some("stream-error"), error.message()), None),
            None => (event(Some("end"), ""), None),
        })
    });
    Ok((
        // Nginx must pass events through as they arrive.
        [("x-accel-buffering", "no"), ("cache-control", "no-cache")],
        Sse::new(stream).keep_alive(KeepAlive::default()),
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

async fn deployment(
    AdminUser(_): AdminUser,
    State(state): State<AppState>,
    Path((id, deployment_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<Json<DeploymentInfo>> {
    Ok(Json(find_deployment(&state, id, deployment_id).await?))
}

#[derive(Deserialize)]
struct DeploymentLogQuery {
    #[serde(default)]
    offset: u64,
    max_bytes: Option<u32>,
}

#[derive(Serialize)]
struct DeploymentLogResponse {
    #[serde(flatten)]
    chunk: DeploymentLogChunk,
    status: DeploymentStatus,
}

/// Incremental deployment log: poll with the returned `next_offset` until
/// `eof` is true and `status` is no longer `queued` or `running`.
async fn deployment_log(
    AdminUser(_): AdminUser,
    State(state): State<AppState>,
    Path((id, deployment_id)): Path<(Uuid, Uuid)>,
    Query(query): Query<DeploymentLogQuery>,
) -> ApiResult<Json<DeploymentLogResponse>> {
    let project = find_project(&state, id).await?;
    let deployment = find_deployment(&state, id, deployment_id).await?;
    let chunk = state
        .agent
        .read_deployment_log(DeploymentLogParams {
            project_slug: project.slug,
            deployment_id,
            offset: query.offset,
            max_bytes: query.max_bytes.unwrap_or(64 * 1024),
        })
        .await?;
    Ok(Json(DeploymentLogResponse {
        chunk,
        status: deployment.status,
    }))
}

/// Cancels a queued deployment immediately, or asks the agent to stop a
/// running one. A running deployment answers `202`: the worker records the
/// `cancelled` status once the agent has stopped its process group.
async fn cancel_deployment(
    AdminUser(user): AdminUser,
    State(state): State<AppState>,
    Path((id, deployment_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<(StatusCode, Json<DeploymentInfo>)> {
    let deployment = find_deployment(&state, id, deployment_id).await?;
    let status = match deployment.status {
        DeploymentStatus::Queued
            if db::compose_projects::cancel_queued_deployment(&state.db, id, deployment_id)
                .await? =>
        {
            StatusCode::OK
        }
        DeploymentStatus::Queued | DeploymentStatus::Running => {
            let signalled = state
                .agent
                .cancel_deployment(CancelDeploymentParams { deployment_id })
                .await?;
            if !signalled {
                return Err(ApiError::Conflict(
                    "the deployment is not running in the agent; retry in a moment".into(),
                ));
            }
            StatusCode::ACCEPTED
        }
        _ => {
            return Err(ApiError::Conflict(
                "the deployment has already finished".into(),
            ))
        }
    };
    db::audit::record(
        &state.db,
        Some(user.id),
        "user",
        "cancel_compose_deployment",
        Some(&deployment_id.to_string()),
        serde_json::json!({ "project_id": id }),
    )
    .await
    .ok();
    Ok((
        status,
        Json(find_deployment(&state, id, deployment_id).await?),
    ))
}

async fn domains(
    AdminUser(_): AdminUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Vec<ComposeDomain>>> {
    let _ = find_project(&state, id).await?;
    Ok(Json(
        db::compose_projects::list_domains(&state.db, id).await?,
    ))
}

#[derive(Deserialize)]
struct AddDomainBody {
    domain: String,
    service_name: String,
    container_port: u16,
    #[serde(default)]
    https: bool,
    /// Queue a deployment that applies the binding (default true).
    #[serde(default = "default_true")]
    redeploy: bool,
}

#[derive(Serialize)]
struct DomainChange<T> {
    #[serde(flatten)]
    value: T,
    deployment: Option<DeploymentInfo>,
}

async fn add_domain(
    AdminUser(user): AdminUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(body): Json<AddDomainBody>,
) -> ApiResult<(StatusCode, Json<DomainChange<ComposeDomain>>)> {
    let project = find_project(&state, id).await?;
    let domain = body.domain.trim().to_ascii_lowercase();
    if !validate::is_valid_domain(&domain) {
        return Err(ApiError::BadRequest("invalid domain".into()));
    }
    if !validate::is_valid_service_name(&body.service_name) {
        return Err(ApiError::BadRequest("invalid service name".into()));
    }
    if body.container_port == 0 {
        return Err(ApiError::BadRequest(
            "container_port must be 1..=65535".into(),
        ));
    }
    if db::compose_projects::domain_in_use(&state.db, &domain).await? {
        return Err(ApiError::Conflict(format!("{domain} is already in use")));
    }
    let created = db::compose_projects::create_domain(
        &state.db,
        id,
        &domain,
        &body.service_name,
        body.container_port,
        body.https,
    )
    .await
    .map_err(|e| conflict_on_unique(e, "domain or port already in use; retry"))?
    .ok_or_else(|| ApiError::Conflict("no free loopback port for domain bindings".into()))?;
    db::audit::record(
        &state.db,
        Some(user.id),
        "user",
        "add_compose_domain",
        Some(&domain),
        serde_json::json!({ "project_id": id, "service": body.service_name, "https": body.https }),
    )
    .await
    .ok();
    let deployment = if body.redeploy {
        Some(
            state
                .deploy_queue
                .enqueue(&state.db, id, &project.slug, false, "domain")
                .await?,
        )
    } else {
        None
    };
    Ok((
        StatusCode::CREATED,
        Json(DomainChange {
            value: created,
            deployment,
        }),
    ))
}

#[derive(Deserialize)]
struct RemoveDomainQuery {
    redeploy: Option<bool>,
}

async fn remove_domain(
    AdminUser(user): AdminUser,
    State(state): State<AppState>,
    Path((id, domain_id)): Path<(Uuid, Uuid)>,
    Query(query): Query<RemoveDomainQuery>,
) -> ApiResult<Json<DomainChange<serde_json::Value>>> {
    let project = find_project(&state, id).await?;
    if !db::compose_projects::delete_domain(&state.db, id, domain_id).await? {
        return Err(ApiError::NotFound("domain binding not found".into()));
    }
    db::audit::record(
        &state.db,
        Some(user.id),
        "user",
        "remove_compose_domain",
        Some(&project.slug),
        serde_json::json!({ "project_id": id, "domain_id": domain_id }),
    )
    .await
    .ok();
    let deployment = if query.redeploy.unwrap_or(true) {
        Some(
            state
                .deploy_queue
                .enqueue(&state.db, id, &project.slug, false, "domain")
                .await?,
        )
    } else {
        None
    };
    Ok(Json(DomainChange {
        value: serde_json::json!({ "deleted": true }),
        deployment,
    }))
}

#[derive(Serialize)]
struct WebhookStatus {
    enabled: bool,
    /// Path to configure on the Git host, relative to the panel origin.
    path: String,
    /// Globs a push must touch to deploy; empty deploys every push.
    watch_paths: Vec<String>,
}

#[derive(Serialize)]
struct WebhookCredentials {
    path: String,
    /// Shown once, on enable/rotate; set it as the webhook secret
    /// (GitHub/Gitea) or secret token (GitLab).
    secret: String,
}

fn webhook_path(id: Uuid) -> String {
    format!("/api/webhooks/compose/{id}")
}

async fn webhook_status(
    AdminUser(_): AdminUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<WebhookStatus>> {
    let _ = find_project(&state, id).await?;
    let enabled = db::compose_projects::webhook_nonce(&state.db, id)
        .await?
        .is_some();
    Ok(Json(WebhookStatus {
        enabled,
        path: webhook_path(id),
        watch_paths: db::compose_projects::webhook_watch_paths(&state.db, id).await?,
    }))
}

#[derive(Deserialize)]
struct UpdateWebhookBody {
    watch_paths: Vec<String>,
}

/// Replaces the watch paths. Works whether or not the webhook is enabled,
/// and never changes the secret.
async fn update_webhook(
    AdminUser(user): AdminUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(body): Json<UpdateWebhookBody>,
) -> ApiResult<Json<WebhookStatus>> {
    use crate::services::webhook::{is_valid_watch_path, MAX_WATCH_PATHS};
    let project = find_project(&state, id).await?;
    let mut watch_paths: Vec<String> = body
        .watch_paths
        .iter()
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .collect();
    let mut seen = std::collections::HashSet::new();
    watch_paths.retain(|p| seen.insert(p.clone()));
    if watch_paths.len() > MAX_WATCH_PATHS {
        return Err(ApiError::BadRequest(format!(
            "at most {MAX_WATCH_PATHS} watch paths are allowed"
        )));
    }
    if let Some(bad) = watch_paths.iter().find(|p| !is_valid_watch_path(p)) {
        return Err(ApiError::BadRequest(format!(
            "invalid watch path {bad:?}: use a repository-relative glob with *, ?, or **"
        )));
    }
    db::compose_projects::set_webhook_watch_paths(&state.db, id, &watch_paths).await?;
    db::audit::record(
        &state.db,
        Some(user.id),
        "user",
        "update_compose_webhook",
        Some(&project.slug),
        serde_json::json!({ "project_id": id, "watch_paths": watch_paths }),
    )
    .await
    .ok();
    webhook_status(AdminUser(user), State(state), Path(id)).await
}

/// Enables the push webhook, or rotates its secret if already enabled. The
/// previous secret stops working immediately.
async fn rotate_webhook(
    AdminUser(user): AdminUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<WebhookCredentials>> {
    let project = find_project(&state, id).await?;
    if project.source.git_checkout().is_none() {
        return Err(ApiError::BadRequest(
            "webhooks need a Git-sourced project".into(),
        ));
    }
    let nonce = crate::services::webhook::new_nonce();
    db::compose_projects::set_webhook_nonce(&state.db, id, Some(&nonce)).await?;
    db::audit::record(
        &state.db,
        Some(user.id),
        "user",
        "rotate_compose_webhook",
        Some(&project.slug),
        serde_json::json!({ "project_id": id }),
    )
    .await
    .ok();
    Ok(Json(WebhookCredentials {
        path: webhook_path(id),
        secret: state.webhook_key.secret_for(id, &nonce),
    }))
}

async fn disable_webhook(
    AdminUser(user): AdminUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    let project = find_project(&state, id).await?;
    db::compose_projects::set_webhook_nonce(&state.db, id, None).await?;
    db::audit::record(
        &state.db,
        Some(user.id),
        "user",
        "disable_compose_webhook",
        Some(&project.slug),
        serde_json::json!({ "project_id": id }),
    )
    .await
    .ok();
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Serialize)]
struct GitAccess {
    /// `ssh` or `https`; `None` for a raw Compose project.
    transport: Option<&'static str>,
    host: Option<String>,
    /// Register this on the Git host as a read-only deploy key.
    deploy_public_key: Option<String>,
    /// Host a write-only HTTPS token was saved for. The token itself is
    /// never returned.
    token_host: Option<String>,
    /// Linked GitHub App installation, if any.
    github_app: Option<GitHubAppLink>,
}

#[derive(Serialize)]
struct GitHubAppLink {
    id: Uuid,
    name: String,
    app_id: i64,
    installation_id: i64,
}

#[derive(Deserialize)]
struct GitHubAppLinkBody {
    github_app_id: Uuid,
    /// From the installation's settings URL
    /// (`github.com/settings/installations/<id>`).
    installation_id: i64,
}

async fn git_access(
    AdminUser(_): AdminUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<GitAccess>> {
    let project = find_project(&state, id).await?;
    git_access_of(&state, &project).await.map(Json)
}

async fn git_access_of(state: &AppState, project: &ComposeProject) -> ApiResult<GitAccess> {
    let remote = project
        .source
        .git_checkout()
        .and_then(|(repository_url, _)| validate::git_remote(repository_url));
    let record = db::compose_projects::git_access(&state.db, project.id).await?;
    let github_app = match (record.github_app_id, record.github_installation_id) {
        (Some(app_id), Some(installation_id)) => db::github_apps::find(&state.db, app_id)
            .await?
            .map(|app| GitHubAppLink {
                id: app.id,
                name: app.name,
                app_id: app.app_id,
                installation_id,
            }),
        _ => None,
    };
    Ok(GitAccess {
        transport: remote.as_ref().map(|(transport, _)| match transport {
            GitTransport::Ssh => "ssh",
            GitTransport::Https => "https",
        }),
        host: remote.map(|(_, host)| host),
        deploy_public_key: record.deploy_public_key,
        token_host: record.git_token_host,
        github_app,
    })
}

/// A GitHub App link needs a registered App, a positive installation ID,
/// and an `https://github.com/<owner>/<repo>` remote.
async fn check_github_app_link(
    state: &AppState,
    source: &ComposeSource,
    link: &GitHubAppLinkBody,
) -> ApiResult<()> {
    let repository = source
        .git_checkout()
        .and_then(|(url, _)| crate::services::github_app::github_repository(url));
    if repository.is_none() {
        return Err(ApiError::BadRequest(
            "GitHub Apps need an https://github.com/<owner>/<repo> remote".into(),
        ));
    }
    if link.installation_id <= 0 {
        return Err(ApiError::BadRequest("invalid installation ID".into()));
    }
    if db::github_apps::find(&state.db, link.github_app_id)
        .await?
        .is_none()
    {
        return Err(ApiError::BadRequest("unknown GitHub App".into()));
    }
    Ok(())
}

/// Links a GitHub App installation. Deployments then mint a short-lived,
/// repository-scoped token instead of using a stored one.
async fn link_github_app(
    AdminUser(user): AdminUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(link): Json<GitHubAppLinkBody>,
) -> ApiResult<Json<GitAccess>> {
    let project = find_project(&state, id).await?;
    check_github_app_link(&state, &project.source, &link).await?;
    db::compose_projects::set_github_app(
        &state.db,
        id,
        Some((link.github_app_id, link.installation_id)),
    )
    .await?;
    db::audit::record(
        &state.db,
        Some(user.id),
        "user",
        "link_compose_github_app",
        Some(&project.slug),
        serde_json::json!({
            "project_id": id,
            "github_app_id": link.github_app_id,
            "installation_id": link.installation_id,
        }),
    )
    .await
    .ok();
    git_access_of(&state, &project).await.map(Json)
}

async fn unlink_github_app(
    AdminUser(user): AdminUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    let project = find_project(&state, id).await?;
    db::compose_projects::set_github_app(&state.db, id, None).await?;
    db::audit::record(
        &state.db,
        Some(user.id),
        "user",
        "unlink_compose_github_app",
        Some(&project.slug),
        serde_json::json!({ "project_id": id }),
    )
    .await
    .ok();
    Ok(StatusCode::NO_CONTENT)
}

/// Creates the project's SSH deploy key, or replaces it: the old key stops
/// working once it is removed from the Git host.
async fn generate_deploy_key(
    AdminUser(user): AdminUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<GitAccess>> {
    let project = find_project(&state, id).await?;
    let is_ssh = project
        .source
        .git_checkout()
        .is_some_and(|(repository_url, _)| {
            matches!(
                validate::git_remote(repository_url),
                Some((GitTransport::Ssh, _))
            )
        });
    if !is_ssh {
        return Err(ApiError::BadRequest(
            "deploy keys need an SSH remote such as git@github.com:org/repo.git".into(),
        ));
    }
    let public_key = state
        .agent
        .generate_deploy_key(ComposeProjectActionParams {
            project_slug: project.slug.clone(),
        })
        .await?;
    db::compose_projects::set_deploy_public_key(&state.db, id, Some(&public_key)).await?;
    db::audit::record(
        &state.db,
        Some(user.id),
        "user",
        "generate_compose_deploy_key",
        Some(&project.slug),
        serde_json::json!({ "project_id": id }),
    )
    .await
    .ok();
    git_access_of(&state, &project).await.map(Json)
}

async fn remove_deploy_key(
    AdminUser(user): AdminUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    let project = find_project(&state, id).await?;
    state
        .agent
        .remove_deploy_key(ComposeProjectActionParams {
            project_slug: project.slug.clone(),
        })
        .await?;
    db::compose_projects::set_deploy_public_key(&state.db, id, None).await?;
    db::audit::record(
        &state.db,
        Some(user.id),
        "user",
        "remove_compose_deploy_key",
        Some(&project.slug),
        serde_json::json!({ "project_id": id }),
    )
    .await
    .ok();
    Ok(StatusCode::NO_CONTENT)
}

/// Replaces the write-only HTTPS token. It is bound to the host of the
/// project's remote, taken from the project rather than the request.
async fn set_git_token(
    AdminUser(user): AdminUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(credentials): Json<GitHttpCredentials>,
) -> ApiResult<Json<GitAccess>> {
    validate_git_credentials(&credentials)?;
    let project = find_project(&state, id).await?;
    let host = https_host(&project.source)?;
    write_git_token(&state, &project, &host, Some(credentials)).await?;
    db::audit::record(
        &state.db,
        Some(user.id),
        "user",
        "set_compose_git_token",
        Some(&project.slug),
        serde_json::json!({ "project_id": id, "host": host }),
    )
    .await
    .ok();
    git_access_of(&state, &project).await.map(Json)
}

async fn remove_git_token(
    AdminUser(user): AdminUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    let project = find_project(&state, id).await?;
    let host = db::compose_projects::git_access(&state.db, id)
        .await?
        .git_token_host
        .unwrap_or_default();
    write_git_token(&state, &project, &host, None).await?;
    db::audit::record(
        &state.db,
        Some(user.id),
        "user",
        "remove_compose_git_token",
        Some(&project.slug),
        serde_json::json!({ "project_id": id }),
    )
    .await
    .ok();
    Ok(StatusCode::NO_CONTENT)
}

/// Sends the token to the agent (or removes it) and records only the host.
async fn write_git_token(
    state: &AppState,
    project: &ComposeProject,
    host: &str,
    credentials: Option<GitHttpCredentials>,
) -> ApiResult<()> {
    let stored_host = credentials.as_ref().map(|_| host.to_string());
    state
        .agent
        .write_git_credentials(WriteGitCredentialsParams {
            project_slug: project.slug.clone(),
            host: host.to_string(),
            credentials,
        })
        .await?;
    db::compose_projects::set_git_token_host(&state.db, project.id, stored_host.as_deref()).await?;
    Ok(())
}

/// The host of an HTTPS Git source; tokens apply to nothing else.
fn https_host(source: &ComposeSource) -> ApiResult<String> {
    match source.git_checkout() {
        Some((repository_url, _)) => match validate::git_remote(repository_url) {
            Some((GitTransport::Https, host)) => Ok(host),
            _ => Err(ApiError::BadRequest(
                "HTTPS tokens need an https:// remote; use a deploy key for SSH remotes".into(),
            )),
        },
        None => Err(ApiError::BadRequest(
            "Git credentials need a Git-sourced project".into(),
        )),
    }
}

/// Same rules the agent enforces. Errors never echo the token.
fn validate_git_credentials(credentials: &GitHttpCredentials) -> ApiResult<()> {
    let token = &credentials.token;
    if !validate::is_valid_git_username(&credentials.username)
        || token.is_empty()
        || token.len() > 4096
        || token.chars().any(|c| c.is_control() || c.is_whitespace())
    {
        return Err(ApiError::BadRequest(
            "invalid Git credentials: username and a token of 1..=4096 characters without spaces"
                .into(),
        ));
    }
    Ok(())
}

#[derive(Serialize, Deserialize)]
struct ProjectRegistry {
    /// The private registry the project pulls images with; `None` pulls
    /// anonymously.
    registry_id: Option<Uuid>,
}

async fn project_registry(
    AdminUser(_): AdminUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<ProjectRegistry>> {
    find_project(&state, id).await?;
    Ok(Json(ProjectRegistry {
        registry_id: db::registries::project_registry(&state.db, id).await?,
    }))
}

async fn set_project_registry(
    AdminUser(user): AdminUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(body): Json<ProjectRegistry>,
) -> ApiResult<Json<ProjectRegistry>> {
    let project = find_project(&state, id).await?;
    if let Some(registry_id) = body.registry_id {
        if db::registries::find(&state.db, registry_id)
            .await?
            .is_none()
        {
            return Err(ApiError::BadRequest("registry not found".into()));
        }
    }
    db::registries::set_project_registry(&state.db, id, body.registry_id).await?;
    db::audit::record(
        &state.db,
        Some(user.id),
        "user",
        "set_compose_registry",
        Some(&project.slug),
        serde_json::json!({ "project_id": id, "registry_id": body.registry_id }),
    )
    .await
    .ok();
    Ok(Json(body))
}

async fn find_project(state: &AppState, id: Uuid) -> ApiResult<ComposeProject> {
    db::compose_projects::find_by_id(&state.db, id)
        .await?
        .ok_or_else(|| ApiError::NotFound("Compose project not found".into()))
}

async fn find_deployment(
    state: &AppState,
    project_id: Uuid,
    deployment_id: Uuid,
) -> ApiResult<DeploymentInfo> {
    db::compose_projects::find_deployment(&state.db, project_id, deployment_id)
        .await?
        .ok_or_else(|| ApiError::NotFound("deployment not found".into()))
}

fn conflict_on_unique(error: anyhow::Error, message: &str) -> ApiError {
    let unique = error
        .downcast_ref::<sqlx::Error>()
        .and_then(|e| e.as_database_error())
        .is_some_and(|e| e.is_unique_violation());
    if unique {
        ApiError::Conflict(message.into())
    } else {
        ApiError::Internal(error)
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

/// Same rules the agent enforces, checked early so bad input never creates
/// a project row or a queued job.
fn validate_source(source: &ComposeSource) -> ApiResult<()> {
    let valid = match source {
        ComposeSource::Git {
            repository_url,
            branch,
            compose_path,
        } => {
            validate::is_valid_git_url(repository_url)
                && validate::is_valid_git_branch(branch)
                && validate::is_valid_compose_path(compose_path)
        }
        ComposeSource::Raw { compose_yaml } => {
            !compose_yaml.is_empty() && compose_yaml.len() <= 512 * 1024
        }
        ComposeSource::Application(spec) => {
            return spec.validate().map_err(ApiError::BadRequest);
        }
        ComposeSource::Static(spec) => {
            return spec.validate().map_err(ApiError::BadRequest);
        }
    };
    if valid {
        Ok(())
    } else {
        Err(ApiError::BadRequest("invalid project source".into()))
    }
}

fn validate_environment(environment: &[(String, String)]) -> ApiResult<()> {
    env_layers::validate_pairs(environment)
}

fn source_kind(source: &ComposeSource) -> &'static str {
    source.kind()
}

#[cfg(test)]
mod tests {
    use crate::test_support;
    use axum::body::Body;
    use axum::http::{Method, Request, StatusCode};
    use panel_core::adapters::mock::MockAdapter;
    use serde_json::{json, Value};
    use std::sync::Arc;
    use tower::ServiceExt;

    /// Sends one request through the full router and returns status and body.
    async fn call(
        router: &axum::Router,
        cookie: &str,
        method: Method,
        uri: &str,
        body: Option<Value>,
    ) -> (StatusCode, String) {
        let mut request = Request::builder()
            .method(method)
            .uri(uri)
            .header("cookie", cookie);
        if body.is_some() {
            request = request.header("content-type", "application/json");
        }
        let request = request
            .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
            .unwrap();
        let response = router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        (status, String::from_utf8(bytes.to_vec()).unwrap())
    }

    #[tokio::test]
    async fn application_sources_are_validated_and_queued_like_compose() {
        let pool = test_support::test_pool().await;
        let adapter = Arc::new(MockAdapter::new());
        let state = test_support::app_state(pool.clone(), adapter.clone());
        let admin = test_support::user(&pool, true).await;
        let cookie = test_support::session_cookie(&state, admin.id).await;
        let router = crate::routes::build_router(state);
        let slug = |p: &str| format!("{p}{}", &uuid::Uuid::new_v4().simple().to_string()[..12]);

        for bad in [
            json!({ "kind": "application", "build": { "type": "image", "image": "nginx --privileged" } }),
            json!({ "kind": "application", "build": { "type": "image", "image": "nginx" }, "service_name": "a b" }),
            json!({ "kind": "application", "build": { "type": "image", "image": "nginx" },
                    "volumes": [{ "name": "data", "target": "/data:/etc" }] }),
            json!({ "kind": "application", "build": { "type": "dockerfile",
                    "repository_url": "https://github.com/acme/app.git", "branch": "main",
                    "context_path": "../.." } }),
        ] {
            let (status, body) = call(
                &router,
                &cookie,
                Method::POST,
                "/api/compose-projects",
                Some(json!({ "name": "Bad app", "slug": slug("bad"), "source": bad })),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        }

        let (status, body) = call(
            &router,
            &cookie,
            Method::POST,
            "/api/compose-projects",
            Some(json!({
                "name": "Image app",
                "slug": slug("img"),
                "source": { "kind": "application",
                            "build": { "type": "image", "image": "ghcr.io/acme/app:1.0" },
                            "container_port": 8080, "memory_limit_mb": 256 },
            })),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        let project = &serde_json::from_str::<Value>(&body).unwrap()["project"];
        assert_eq!(project["source"]["build"]["image"], "ghcr.io/acme/app:1.0");
        assert_eq!(project["source"]["service_name"], "app");
        assert_eq!(project["source"]["restart"], "unless-stopped");
        // An image application has no Git remote, so no webhook.
        let id = project["id"].as_str().unwrap();
        let (status, _) = call(
            &router,
            &cookie,
            Method::POST,
            &format!("/api/compose-projects/{id}/webhook"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        let (status, body) = call(
            &router,
            &cookie,
            Method::POST,
            "/api/compose-projects",
            Some(json!({
                "name": "Built app",
                "slug": slug("built"),
                "source": { "kind": "application",
                            "build": { "type": "dockerfile",
                                       "repository_url": "git@github.com:acme/app.git",
                                       "branch": "main" } },
            })),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        let project = &serde_json::from_str::<Value>(&body).unwrap()["project"];
        assert_eq!(project["source"]["build"]["context_path"], ".");
        assert_eq!(project["source"]["build"]["dockerfile_path"], "Dockerfile");
        // A Dockerfile application deploys from Git: webhooks and deploy
        // keys apply to it.
        let id = project["id"].as_str().unwrap();
        let (status, body) = call(
            &router,
            &cookie,
            Method::GET,
            &format!("/api/compose-projects/{id}/git-access"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let access: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(access["transport"], "ssh");
        assert_eq!(access["host"], "github.com");
        let (status, body) = call(
            &router,
            &cookie,
            Method::POST,
            &format!("/api/compose-projects/{id}/webhook"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }

    #[tokio::test]
    async fn environment_values_never_reach_responses_audit_or_queue_rows() {
        let pool = test_support::test_pool().await;
        let adapter = Arc::new(MockAdapter::new());
        let state = test_support::app_state(pool.clone(), adapter.clone());
        let admin = test_support::user(&pool, true).await;
        let cookie = test_support::session_cookie(&state, admin.id).await;
        let router = crate::routes::build_router(state);

        let secrets = [
            format!("create-{}", uuid::Uuid::new_v4()),
            format!("replace-{}", uuid::Uuid::new_v4()),
            format!("deploy-{}", uuid::Uuid::new_v4()),
        ];
        let slug = format!("p{}", &uuid::Uuid::new_v4().simple().to_string()[..12]);
        let mut bodies = Vec::new();

        let (status, body) = call(
            &router,
            &cookie,
            Method::POST,
            "/api/compose-projects",
            Some(json!({
                "name": "Leak test",
                "slug": slug,
                "source": { "kind": "raw", "compose_yaml": "services:\n  web:\n    image: nginx\n" },
                "environment": [["API_TOKEN", secrets[0]]],
            })),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        let id = serde_json::from_str::<Value>(&body).unwrap()["project"]["id"]
            .as_str()
            .unwrap()
            .to_string();
        bodies.push(body);

        let base = format!("/api/compose-projects/{id}");
        let (status, body) = call(
            &router,
            &cookie,
            Method::PUT,
            &format!("{base}/environment"),
            Some(json!({ "environment": [["API_TOKEN", secrets[1]]] })),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
        bodies.push(body);

        let (status, body) = call(
            &router,
            &cookie,
            Method::POST,
            &format!("{base}/deploy"),
            Some(json!({ "environment": [["API_TOKEN", secrets[2]]] })),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        bodies.push(body);

        for path in [
            "/api/compose-projects".to_string(),
            base.clone(),
            format!("{base}/deployments"),
            format!("{base}/webhook"),
        ] {
            let (status, body) = call(&router, &cookie, Method::GET, &path, None).await;
            assert_eq!(status, StatusCode::OK, "{path}: {body}");
            bodies.push(body);
        }
        // Invalid input is echoed back in errors, so check that path too.
        let (status, body) = call(
            &router,
            &cookie,
            Method::PUT,
            &format!("{base}/environment"),
            Some(json!({ "environment": [["BAD KEY", secrets[1]]] })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        bodies.push(body);

        let audit: Vec<String> = sqlx::query_scalar(
            "SELECT metadata::text FROM audit_log WHERE target = $1 OR metadata->>'project_id' = $2",
        )
        .bind(&slug)
        .bind(&id)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert!(audit.len() >= 3, "expected audit rows, got {audit:?}");
        let rows: Vec<String> = sqlx::query_scalar(
            "SELECT to_jsonb(d)::text FROM compose_deployments d WHERE project_id = $1::uuid
             UNION ALL
             SELECT to_jsonb(p)::text FROM compose_projects p WHERE id = $1::uuid
             UNION ALL
             SELECT to_jsonb(v)::text FROM env_variables v WHERE service_id = $1::uuid",
        )
        .bind(&id)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert!(rows.len() >= 4);

        for text in bodies.iter().chain(&audit).chain(&rows) {
            for secret in &secrets {
                assert!(!text.contains(secret.as_str()), "secret leaked into {text}");
            }
        }
        // The service layer is stored sealed; the agent receives the merged
        // set from the queue worker at deploy time (not from handlers).
        let calls = adapter.calls.lock().unwrap().clone();
        assert!(
            !calls.iter().any(|c| c == "write_compose_environment"),
            "{calls:?}"
        );
        let stored: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM env_variables WHERE service_id = $1::uuid")
                .bind(&id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(stored, 1);
    }

    #[tokio::test]
    async fn git_tokens_stay_write_only_and_deploy_keys_need_ssh_remotes() {
        let pool = test_support::test_pool().await;
        let adapter = Arc::new(MockAdapter::new());
        let state = test_support::app_state(pool.clone(), adapter.clone());
        let admin = test_support::user(&pool, true).await;
        let cookie = test_support::session_cookie(&state, admin.id).await;
        let router = crate::routes::build_router(state);
        let tokens = [
            format!("ghp_create{}", uuid::Uuid::new_v4().simple()),
            format!("ghp_replace{}", uuid::Uuid::new_v4().simple()),
            format!("bad token {}", uuid::Uuid::new_v4().simple()),
        ];
        let slug = || format!("p{}", &uuid::Uuid::new_v4().simple().to_string()[..12]);
        let git = |url: &str| {
            json!({ "kind": "git", "repository_url": url, "branch": "main",
                    "compose_path": "docker-compose.yml" })
        };
        let mut bodies = Vec::new();

        let https_slug = slug();
        let (status, body) = call(
            &router,
            &cookie,
            Method::POST,
            "/api/compose-projects",
            Some(json!({
                "name": "Private HTTPS",
                "slug": https_slug,
                "source": git("https://GitHub.com/org/private.git"),
                "git_credentials": { "username": "x-access-token", "token": tokens[0] },
                "deploy": false,
            })),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        let https_id = serde_json::from_str::<Value>(&body).unwrap()["project"]["id"]
            .as_str()
            .unwrap()
            .to_string();
        bodies.push(body);
        let https_base = format!("/api/compose-projects/{https_id}/git-access");

        let (status, body) = call(&router, &cookie, Method::GET, &https_base, None).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let access: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(access["transport"], "https");
        assert_eq!(access["token_host"], "github.com");
        assert!(access["deploy_public_key"].is_null());
        bodies.push(body);

        let (status, body) = call(
            &router,
            &cookie,
            Method::PUT,
            &format!("{https_base}/token"),
            Some(json!({ "username": "x-access-token", "token": tokens[1] })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        bodies.push(body);
        for (method, path, body) in [
            (
                Method::PUT,
                format!("{https_base}/token"),
                Some(json!({ "username": "x-access-token", "token": tokens[2] })),
            ),
            (Method::POST, format!("{https_base}/deploy-key"), None),
        ] {
            let (status, body) = call(&router, &cookie, method, &path, body).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{path}: {body}");
            bodies.push(body);
        }

        let ssh_slug = slug();
        let (status, body) = call(
            &router,
            &cookie,
            Method::POST,
            "/api/compose-projects",
            Some(json!({
                "name": "Private SSH",
                "slug": ssh_slug,
                "source": git("git@github.com:org/private.git"),
                "deploy": false,
            })),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        let ssh_id = serde_json::from_str::<Value>(&body).unwrap()["project"]["id"]
            .as_str()
            .unwrap()
            .to_string();
        let ssh_base = format!("/api/compose-projects/{ssh_id}/git-access");
        let (status, body) = call(
            &router,
            &cookie,
            Method::POST,
            &format!("{ssh_base}/deploy-key"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let access: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(access["transport"], "ssh");
        assert!(access["deploy_public_key"]
            .as_str()
            .unwrap()
            .starts_with("ssh-ed25519 "));
        let (status, body) = call(
            &router,
            &cookie,
            Method::PUT,
            &format!("{ssh_base}/token"),
            Some(json!({ "username": "git", "token": "abc" })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        let (status, _) = call(
            &router,
            &cookie,
            Method::DELETE,
            &format!("{ssh_base}/deploy-key"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (_, body) = call(&router, &cookie, Method::GET, &ssh_base, None).await;
        assert!(serde_json::from_str::<Value>(&body).unwrap()["deploy_public_key"].is_null());

        let (status, _) = call(
            &router,
            &cookie,
            Method::DELETE,
            &format!("{https_base}/token"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (_, body) = call(&router, &cookie, Method::GET, &https_base, None).await;
        assert!(serde_json::from_str::<Value>(&body).unwrap()["token_host"].is_null());
        bodies.push(body);

        let audit: Vec<String> = sqlx::query_scalar(
            "SELECT metadata::text FROM audit_log WHERE target = $1 OR target = $2",
        )
        .bind(&https_slug)
        .bind(&ssh_slug)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert!(audit.len() >= 6, "expected audit rows, got {audit:?}");
        let rows: Vec<String> = sqlx::query_scalar(
            "SELECT to_jsonb(p)::text FROM compose_projects p WHERE id IN ($1::uuid, $2::uuid)",
        )
        .bind(&https_id)
        .bind(&ssh_id)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(rows.len(), 2);
        for text in bodies.iter().chain(&audit).chain(&rows) {
            for token in &tokens {
                assert!(!text.contains(token.as_str()), "token leaked into {text}");
            }
        }
        let calls = adapter.calls.lock().unwrap().clone();
        let count = |name: &str| calls.iter().filter(|c| *c == name).count();
        // Create, replace, remove.
        assert_eq!(count("write_git_credentials"), 3, "{calls:?}");
        assert_eq!(count("generate_deploy_key"), 1, "{calls:?}");
    }

    #[tokio::test]
    async fn github_apps_are_write_only_and_link_only_github_https_remotes() {
        let pool = test_support::test_pool().await;
        let adapter = Arc::new(MockAdapter::new());
        let state = test_support::app_state(pool.clone(), adapter.clone());
        let admin = test_support::user(&pool, true).await;
        let cookie = test_support::session_cookie(&state, admin.id).await;
        let router = crate::routes::build_router(state);
        let unique = uuid::Uuid::new_v4().simple().to_string()[..12].to_string();
        let key = crate::services::github_app::TEST_KEY_PKCS1;

        let (status, body) = call(
            &router,
            &cookie,
            Method::POST,
            "/api/github-apps",
            Some(json!({ "name": "bad", "app_id": 1, "private_key": "ghp_not_a_pem" })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        let (status, body) = call(
            &router,
            &cookie,
            Method::POST,
            "/api/github-apps",
            Some(
                json!({ "name": format!("deployer-{unique}"), "app_id": 4242, "private_key": key }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        assert!(
            !body.contains("PRIVATE KEY") && !body.contains("private_key"),
            "{body}"
        );
        let app: Value = serde_json::from_str(&body).unwrap();
        let app_id = app["id"].as_str().unwrap().to_string();
        let (_, listed) = call(&router, &cookie, Method::GET, "/api/github-apps", None).await;
        assert!(listed.contains(&app_id) && !listed.contains("PRIVATE KEY"));
        // The key is sealed at rest and appears in no other table.
        let sealed: Vec<u8> =
            sqlx::query_scalar("SELECT private_key_sealed FROM github_apps WHERE id = $1::uuid")
                .bind(&app_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(!String::from_utf8_lossy(&sealed).contains("PRIVATE KEY"));
        let audit: Vec<String> = sqlx::query_scalar("SELECT row_to_json(a)::text FROM audit_log a")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert!(audit.iter().all(|row| !row.contains("PRIVATE KEY")));

        let create = |slug: String, url: &str| {
            json!({
                "name": "Private", "slug": slug, "deploy": false,
                "source": { "kind": "git", "repository_url": url, "branch": "main",
                            "compose_path": "docker-compose.yml" },
                "github_app": { "github_app_id": app_id, "installation_id": 77 },
            })
        };
        // SSH and non-GitHub remotes cannot use an App token.
        for url in [
            "git@github.com:acme/app.git",
            "https://gitlab.com/acme/app.git",
        ] {
            let (status, body) = call(
                &router,
                &cookie,
                Method::POST,
                "/api/compose-projects",
                Some(create(format!("ghx{unique}"), url)),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{url}: {body}");
        }
        let (status, body) = call(
            &router,
            &cookie,
            Method::POST,
            "/api/compose-projects",
            Some(create(
                format!("gh{unique}"),
                "https://github.com/acme/app.git",
            )),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        let id = serde_json::from_str::<Value>(&body).unwrap()["project"]["id"]
            .as_str()
            .unwrap()
            .to_string();
        let (_, body) = call(
            &router,
            &cookie,
            Method::GET,
            &format!("/api/compose-projects/{id}/git-access"),
            None,
        )
        .await;
        let access: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(access["github_app"]["id"], app_id.as_str());
        assert_eq!(access["github_app"]["installation_id"], 77);
        assert_eq!(access["github_app"]["app_id"], 4242);

        // An App in use cannot be deleted; unlink first.
        let (status, body) = call(
            &router,
            &cookie,
            Method::DELETE,
            &format!("/api/github-apps/{app_id}"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert!(body.contains(&format!("gh{unique}")));
        let (status, _) = call(
            &router,
            &cookie,
            Method::PUT,
            &format!("/api/compose-projects/{id}/git-access/github-app"),
            Some(json!({ "github_app_id": app_id, "installation_id": 0 })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _) = call(
            &router,
            &cookie,
            Method::DELETE,
            &format!("/api/compose-projects/{id}/git-access/github-app"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (status, _) = call(
            &router,
            &cookie,
            Method::DELETE,
            &format!("/api/github-apps/{app_id}"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn static_sites_and_builder_apps_are_validated_sources() {
        let pool = test_support::test_pool().await;
        let adapter = Arc::new(MockAdapter::new());
        let state = test_support::app_state(pool.clone(), adapter.clone());
        let admin = test_support::user(&pool, true).await;
        let cookie = test_support::session_cookie(&state, admin.id).await;
        let router = crate::routes::build_router(state);
        let slug = |p: &str| format!("{p}{}", &uuid::Uuid::new_v4().simple().to_string()[..12]);
        let repo = "https://github.com/acme/site.git";

        for bad in [
            json!({ "kind": "static", "repository_url": repo, "branch": "main", "publish_path": "../.." }),
            json!({ "kind": "static", "repository_url": repo, "branch": "main", "publish_path": "/etc" }),
            json!({ "kind": "static", "repository_url": repo, "branch": "main",
                    "build": { "image": "node --privileged", "commands": [["npm", "ci"]] } }),
            json!({ "kind": "static", "repository_url": repo, "branch": "main",
                    "build": { "image": "node:22", "commands": [] } }),
            json!({ "kind": "application", "build": { "type": "builder", "builder": "nixpacks",
                    "repository_url": repo, "branch": "main", "context_path": "../x" } }),
            json!({ "kind": "application", "build": { "type": "builder", "builder": "make",
                    "repository_url": repo, "branch": "main" } }),
        ] {
            let (status, body) = call(
                &router,
                &cookie,
                Method::POST,
                "/api/compose-projects",
                Some(json!({ "name": "Bad", "slug": slug("bad"), "source": bad })),
            )
            .await;
            assert!(status.is_client_error(), "{status}: {body}");
        }

        let (status, body) = call(
            &router,
            &cookie,
            Method::POST,
            "/api/compose-projects",
            Some(json!({
                "name": "Docs", "slug": slug("docs"),
                "source": { "kind": "static", "repository_url": repo, "branch": "main",
                            "publish_path": "dist", "spa": true,
                            "build": { "image": "node:22-alpine",
                                       "commands": [["npm", "ci"], ["npm", "run", "build"]] } },
            })),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        let created: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(created["project"]["source"]["kind"], "static");
        assert_eq!(created["project"]["source"]["publish_path"], "dist");
        assert!(created["deployment"]["id"].is_string());
        // Static sites deploy from Git, so webhooks apply.
        let id = created["project"]["id"].as_str().unwrap();
        let (status, _) = call(
            &router,
            &cookie,
            Method::POST,
            &format!("/api/compose-projects/{id}/webhook"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let (status, body) = call(
            &router,
            &cookie,
            Method::POST,
            "/api/compose-projects",
            Some(json!({
                "name": "Api", "slug": slug("api"),
                "source": { "kind": "application", "container_port": 3000,
                            "build": { "type": "builder", "builder": "railpack",
                                       "repository_url": repo, "branch": "main" } },
            })),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        let created: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(created["project"]["source"]["build"]["context_path"], ".");
    }

    #[tokio::test]
    async fn live_logs_stream_as_server_sent_events() {
        let pool = test_support::test_pool().await;
        let state = test_support::app_state(pool.clone(), Arc::new(MockAdapter::new()));
        let admin = test_support::user(&pool, true).await;
        let cookie = test_support::session_cookie(&state, admin.id).await;
        let project = crate::db::compose_projects::create(
            &pool,
            uuid::Uuid::new_v4(),
            "SSE test",
            &format!("p{}", &uuid::Uuid::new_v4().simple().to_string()[..12]),
            admin.id,
            None,
            &panel_core::types::ComposeSource::Raw {
                compose_yaml: "services:\n  web:\n    image: nginx\n".into(),
            },
        )
        .await
        .unwrap();
        let router = crate::routes::build_router(state);
        let base = format!("/api/compose-projects/{}/logs/stream", project.id);

        let (status, body) = call(
            &router,
            &cookie,
            Method::GET,
            &format!("{base}?search=get"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body.contains("data: web-1  | GET / 200\n"), "{body}");
        assert!(
            !body.contains("listening"),
            "search filter not applied: {body}"
        );
        assert!(body.contains("event: end\n"), "{body}");

        let (status, _) = call(
            &router,
            &cookie,
            Method::GET,
            &format!("{base}?service=bad%20name"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _) = call(&router, "", Method::GET, &base, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn projects_are_linked_to_a_registry_at_creation_or_later() {
        let pool = test_support::test_pool().await;
        let adapter = Arc::new(MockAdapter::new());
        let state = test_support::app_state(pool.clone(), adapter.clone());
        let admin = test_support::user(&pool, true).await;
        let cookie = test_support::session_cookie(&state, admin.id).await;
        let router = crate::routes::build_router(state);
        let unique = &uuid::Uuid::new_v4().simple().to_string()[..12];

        let (status, body) = call(
            &router,
            &cookie,
            Method::POST,
            "/api/registries",
            Some(
                json!({ "name": format!("reg-{unique}"), "server_url": "ghcr.io",
                         "username": "octocat", "password": "pw" }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let registry_id = serde_json::from_str::<Value>(&body).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();

        let new_project = |slug: String, registry: Value| {
            json!({ "name": "Private app", "slug": slug, "deploy": false,
                    "source": { "kind": "application", "build": { "type": "image", "image": "ghcr.io/acme/app:1" } },
                    "registry_id": registry })
        };
        // An unknown registry is refused before anything is created.
        let (status, _) = call(
            &router,
            &cookie,
            Method::POST,
            "/api/compose-projects",
            Some(new_project(
                format!("bad{unique}"),
                json!(uuid::Uuid::new_v4()),
            )),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        let (status, body) = call(
            &router,
            &cookie,
            Method::POST,
            "/api/compose-projects",
            Some(new_project(format!("reg{unique}"), json!(registry_id))),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        let project_id = serde_json::from_str::<Value>(&body).unwrap()["project"]["id"]
            .as_str()
            .unwrap()
            .to_string();
        let uri = format!("/api/compose-projects/{project_id}/registry");
        let (_, body) = call(&router, &cookie, Method::GET, &uri, None).await;
        assert_eq!(
            serde_json::from_str::<Value>(&body).unwrap()["registry_id"],
            registry_id
        );

        // Unlinking, and linking to something that does not exist.
        let (status, _) = call(
            &router,
            &cookie,
            Method::PUT,
            &uri,
            Some(json!({ "registry_id": null })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (_, body) = call(&router, &cookie, Method::GET, &uri, None).await;
        assert!(serde_json::from_str::<Value>(&body).unwrap()["registry_id"].is_null());
        let (status, _) = call(
            &router,
            &cookie,
            Method::PUT,
            &uri,
            Some(json!({ "registry_id": uuid::Uuid::new_v4() })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        // Deleting a registry leaves its projects in place, unlinked.
        let (status, _) = call(
            &router,
            &cookie,
            Method::PUT,
            &uri,
            Some(json!({ "registry_id": registry_id })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, _) = call(
            &router,
            &cookie,
            Method::DELETE,
            &format!("/api/registries/{registry_id}"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (_, body) = call(&router, &cookie, Method::GET, &uri, None).await;
        assert!(serde_json::from_str::<Value>(&body).unwrap()["registry_id"].is_null());
    }
}
