//! Projects and their environments (`projects → environments → services`),
//! and the write-only variables of the project and environment layers.
//!
//! Variable values are accepted here and stored sealed; responses, audit
//! entries, and errors carry key names only. Services (Compose projects)
//! inherit project → environment → service at deploy time.

use crate::auth::{perm, Authenticated, Principal, ProjectAccess, Require};
use crate::db;
use crate::db::projects::EnvScope;
use crate::error::{ApiError, ApiResult};
use crate::routes::compose_projects::VariablesBody;
use crate::services::env_layers;
use crate::state::AppState;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use panel_core::rbac::{Permission, Scope, Target};
use panel_core::types::{validate, EnvKeyInfo, EnvLayer, EnvLayerKeys, Environment, Project};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(list).post(create))
        .route("/:id", get(show).patch(rename).delete(remove))
        .route(
            "/:id/variables",
            get(project_variables).put(update_project_variables),
        )
        .route("/:id/environments", post(create_environment))
        .route(
            "/:id/environments/:environment_id",
            delete(remove_environment),
        )
        .route(
            "/:id/environments/:environment_id/variables",
            get(environment_variables).put(update_environment_variables),
        )
}

#[derive(Serialize)]
struct EnvironmentView {
    #[serde(flatten)]
    environment: Environment,
    service_count: i64,
}

#[derive(Serialize)]
struct ProjectView {
    #[serde(flatten)]
    project: Project,
    environments: Vec<EnvironmentView>,
}

/// Keys of one or two layers and how they combine.
#[derive(Serialize)]
struct LayerVariables {
    layers: Vec<EnvLayerKeys>,
    variables: Vec<EnvKeyInfo>,
}

/// The project with the environments `principal` can see.
async fn view(state: &AppState, principal: &Principal, project: Project) -> ApiResult<ProjectView> {
    let counts = db::projects::service_counts(&state.db, project.id).await?;
    let environments = db::projects::list_environments(&state.db, project.id)
        .await?
        .into_iter()
        .filter(|environment| {
            principal.allows(
                Permission::ProjectsRead,
                Target::Environment {
                    project: project.id,
                    environment: environment.id,
                },
            )
        })
        .map(|environment| EnvironmentView {
            service_count: counts
                .iter()
                .find(|(id, _)| *id == environment.id)
                .map_or(0, |(_, n)| *n),
            environment,
        })
        .collect();
    Ok(ProjectView {
        project,
        environments,
    })
}

async fn list(
    Authenticated(principal): Authenticated,
    State(state): State<AppState>,
) -> ApiResult<Json<Vec<ProjectView>>> {
    // A project shows when the principal can read it or one of its
    // environments.
    let projects = db::projects::list_all(&state.db).await?;
    let mut views = Vec::with_capacity(projects.len());
    for project in projects {
        if principal
            .authority
            .allows_within(Permission::ProjectsRead, project.id)
        {
            views.push(view(&state, &principal, project).await?);
        }
    }
    Ok(Json(views))
}

async fn show(
    Authenticated(principal): Authenticated,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<ProjectView>> {
    let project = find_project(&state, id).await?;
    if !principal
        .authority
        .allows_within(Permission::ProjectsRead, project.id)
    {
        return Err(ApiError::Forbidden);
    }
    Ok(Json(view(&state, &principal, project).await?))
}

#[derive(Deserialize)]
struct CreateProjectBody {
    name: String,
    slug: String,
    /// Name of the first environment (default `production`).
    #[serde(default)]
    environment_name: Option<String>,
}

async fn create(
    Require {
        principal: user, ..
    }: Require<perm::ProjectsCreate>,
    State(state): State<AppState>,
    Json(body): Json<CreateProjectBody>,
) -> ApiResult<(StatusCode, Json<ProjectView>)> {
    validate_name_and_slug(&body.name, &body.slug)?;
    let environment_name = body
        .environment_name
        .unwrap_or_else(|| db::projects::DEFAULT_ENVIRONMENT_SLUG.to_string());
    let environment_slug = slugify(&environment_name)?;
    let (project, _) = db::projects::create(
        &state.db,
        body.name.trim(),
        &body.slug,
        user.id,
        environment_name.trim(),
        &environment_slug,
    )
    .await
    .map_err(|e| conflict_on_unique(e, "a project with this slug already exists"))?;
    // A creator who is not an installation admin manages what they create.
    if !user.is_admin {
        if let Some(role) = db::rbac::role_id_by_name(&state.db, "project-admin").await? {
            db::rbac::create_binding(
                &state.db,
                user.id,
                role,
                Scope::Project {
                    project_id: project.id,
                },
                Some(user.id),
            )
            .await?;
        }
    }
    let user = Authenticated::refresh(&state, user).await?;
    audit(
        &state,
        &user,
        "create_project",
        &project.slug,
        serde_json::json!({ "project_id": project.id }),
    )
    .await;
    Ok((
        StatusCode::CREATED,
        Json(view(&state, &user, project).await?),
    ))
}

#[derive(Deserialize)]
struct RenameBody {
    name: String,
}

async fn rename(
    ProjectAccess {
        principal: user, ..
    }: ProjectAccess<perm::ProjectsUpdate>,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(body): Json<RenameBody>,
) -> ApiResult<Json<ProjectView>> {
    let project = find_project(&state, id).await?;
    validate_name(&body.name)?;
    db::projects::rename(&state.db, id, body.name.trim()).await?;
    audit(
        &state,
        &user,
        "rename_project",
        &project.slug,
        serde_json::json!({ "project_id": id }),
    )
    .await;
    let project = find_project(&state, id).await?;
    Ok(Json(view(&state, &user, project).await?))
}

async fn remove(
    ProjectAccess {
        principal: user, ..
    }: ProjectAccess<perm::ProjectsDelete>,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    let project = find_project(&state, id).await?;
    db::projects::delete(&state.db, id)
        .await
        .map_err(|e| conflict_on_foreign_key(e, "delete or move this project's services first"))?;
    audit(
        &state,
        &user,
        "delete_project",
        &project.slug,
        serde_json::json!({ "project_id": id }),
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct CreateEnvironmentBody {
    name: String,
    /// Derived from `name` when omitted.
    #[serde(default)]
    slug: Option<String>,
}

async fn create_environment(
    ProjectAccess {
        principal: user, ..
    }: ProjectAccess<perm::ProjectsUpdate>,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(body): Json<CreateEnvironmentBody>,
) -> ApiResult<(StatusCode, Json<Environment>)> {
    let project = find_project(&state, id).await?;
    validate_name(&body.name)?;
    let slug = match body.slug {
        Some(slug) if validate::is_valid_project_slug(&slug) => slug,
        Some(_) => return Err(ApiError::BadRequest("invalid environment slug".into())),
        None => slugify(&body.name)?,
    };
    let environment = db::projects::create_environment(&state.db, id, body.name.trim(), &slug)
        .await
        .map_err(|e| conflict_on_unique(e, "an environment with this slug already exists"))?;
    audit(
        &state,
        &user,
        "create_environment",
        &project.slug,
        serde_json::json!({ "project_id": id, "environment_id": environment.id }),
    )
    .await;
    Ok((StatusCode::CREATED, Json(environment)))
}

async fn remove_environment(
    ProjectAccess {
        principal: user, ..
    }: ProjectAccess<perm::ProjectsDelete>,
    State(state): State<AppState>,
    Path((id, environment_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<StatusCode> {
    let project = find_project(&state, id).await?;
    find_environment(&state, id, environment_id).await?;
    db::projects::delete_environment(&state.db, id, environment_id)
        .await
        .map_err(|e| conflict_on_foreign_key(e, "delete this environment's services first"))?;
    audit(
        &state,
        &user,
        "delete_environment",
        &project.slug,
        serde_json::json!({ "project_id": id, "environment_id": environment_id }),
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}

async fn project_variables(
    _: ProjectAccess<perm::VariablesRead>,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<LayerVariables>> {
    find_project(&state, id).await?;
    Ok(Json(project_layer(&state, id).await?))
}

async fn project_layer(state: &AppState, id: Uuid) -> ApiResult<LayerVariables> {
    let layers = vec![EnvLayerKeys {
        layer: EnvLayer::Project,
        keys: env_layers::keys(&state.db, EnvScope::Project(id)).await?,
    }];
    let variables = panel_core::types::describe_env_layers(&layers);
    Ok(LayerVariables { layers, variables })
}

/// Replaces the project layer. An entry without `value` keeps the stored
/// value; keys left out are removed. Applies on each service's next
/// deployment.
async fn update_project_variables(
    ProjectAccess {
        principal: user, ..
    }: ProjectAccess<perm::VariablesUpdate>,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(body): Json<VariablesBody>,
) -> ApiResult<Json<LayerVariables>> {
    let project = find_project(&state, id).await?;
    let keys = env_layers::update(
        &state.db,
        &state.secret_box,
        EnvScope::Project(id),
        body.variables,
    )
    .await?;
    audit(
        &state,
        &user,
        "update_project_variables",
        &project.slug,
        serde_json::json!({ "project_id": id, "keys": keys }),
    )
    .await;
    Ok(Json(project_layer(&state, id).await?))
}

async fn environment_variables(
    _: ProjectAccess<perm::VariablesRead>,
    State(state): State<AppState>,
    Path((id, environment_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<Json<LayerVariables>> {
    find_environment(&state, id, environment_id).await?;
    let (layers, variables) =
        env_layers::environment_variables(&state.db, id, environment_id).await?;
    Ok(Json(LayerVariables { layers, variables }))
}

async fn update_environment_variables(
    ProjectAccess {
        principal: user, ..
    }: ProjectAccess<perm::VariablesUpdate>,
    State(state): State<AppState>,
    Path((id, environment_id)): Path<(Uuid, Uuid)>,
    Json(body): Json<VariablesBody>,
) -> ApiResult<Json<LayerVariables>> {
    let project = find_project(&state, id).await?;
    find_environment(&state, id, environment_id).await?;
    let keys = env_layers::update(
        &state.db,
        &state.secret_box,
        EnvScope::Environment(environment_id),
        body.variables,
    )
    .await?;
    audit(
        &state,
        &user,
        "update_environment_variables",
        &project.slug,
        serde_json::json!({ "project_id": id, "environment_id": environment_id, "keys": keys }),
    )
    .await;
    let (layers, variables) =
        env_layers::environment_variables(&state.db, id, environment_id).await?;
    Ok(Json(LayerVariables { layers, variables }))
}

async fn find_project(state: &AppState, id: Uuid) -> ApiResult<Project> {
    db::projects::find_by_id(&state.db, id)
        .await?
        .ok_or_else(|| ApiError::NotFound("project not found".into()))
}

async fn find_environment(state: &AppState, project_id: Uuid, id: Uuid) -> ApiResult<Environment> {
    db::projects::find_environment(&state.db, id)
        .await?
        .filter(|e| e.project_id == project_id)
        .ok_or_else(|| ApiError::NotFound("environment not found".into()))
}

async fn audit(
    state: &AppState,
    principal: &Principal,
    action: &str,
    target: &str,
    metadata: serde_json::Value,
) {
    db::audit::record(
        &state.db,
        Some(principal.id),
        principal.actor_kind(),
        action,
        Some(target),
        metadata,
    )
    .await
    .ok();
}

fn validate_name(name: &str) -> ApiResult<()> {
    if name.trim().is_empty() || name.len() > 100 {
        return Err(ApiError::BadRequest(
            "name must be 1..=100 characters".into(),
        ));
    }
    Ok(())
}

fn validate_name_and_slug(name: &str, slug: &str) -> ApiResult<()> {
    validate_name(name)?;
    if !validate::is_valid_project_slug(slug) {
        return Err(ApiError::BadRequest("invalid project slug".into()));
    }
    Ok(())
}

/// `Staging EU` → `staging-eu`, checked against the slug rules.
fn slugify(name: &str) -> ApiResult<String> {
    let mut slug = String::new();
    for c in name.trim().chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
        } else if !slug.ends_with('-') && !slug.is_empty() {
            slug.push('-');
        }
    }
    let slug = slug.trim_end_matches('-').to_string();
    if validate::is_valid_project_slug(&slug) {
        Ok(slug)
    } else {
        Err(ApiError::BadRequest(
            "environment name must contain letters or digits".into(),
        ))
    }
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

fn conflict_on_foreign_key(error: sqlx::Error, message: &str) -> ApiError {
    if error
        .as_database_error()
        .is_some_and(|e| e.is_foreign_key_violation())
    {
        ApiError::Conflict(message.into())
    } else {
        ApiError::Db(error)
    }
}

#[cfg(test)]
mod tests {
    use super::slugify;
    use crate::test_support;
    use axum::body::Body;
    use axum::http::{Method, Request, StatusCode};
    use panel_core::adapters::mock::MockAdapter;
    use serde_json::{json, Value};
    use std::sync::Arc;
    use tower::ServiceExt;

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

    #[test]
    fn slugs_are_derived_from_names() {
        assert_eq!(slugify("Staging EU").unwrap(), "staging-eu");
        assert_eq!(slugify(" production ").unwrap(), "production");
        assert!(slugify("!!!").is_err());
    }

    #[tokio::test]
    async fn inherited_secrets_never_reach_responses_or_audit() {
        let pool = test_support::test_pool().await;
        let state = test_support::app_state(pool.clone(), Arc::new(MockAdapter::new()));
        let admin = test_support::user(&pool, true).await;
        let cookie = test_support::session_cookie(&state, admin.id).await;
        let router = crate::routes::build_router(state);
        let secrets = [
            format!("project-{}", uuid::Uuid::new_v4()),
            format!("staging-{}", uuid::Uuid::new_v4()),
            format!("service-{}", uuid::Uuid::new_v4()),
        ];
        let mut bodies = Vec::new();

        let slug = format!("g{}", &uuid::Uuid::new_v4().simple().to_string()[..12]);
        let (status, body) = call(
            &router,
            &cookie,
            Method::POST,
            "/api/projects",
            Some(json!({ "name": "Shop", "slug": slug })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let project: Value = serde_json::from_str(&body).unwrap();
        let project_id = project["id"].as_str().unwrap().to_string();
        assert_eq!(project["environments"][0]["slug"], "production");
        let base = format!("/api/projects/{project_id}");

        let (status, body) = call(
            &router,
            &cookie,
            Method::POST,
            &format!("{base}/environments"),
            Some(json!({ "name": "Staging" })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let staging: Value = serde_json::from_str(&body).unwrap();
        let staging_id = staging["id"].as_str().unwrap().to_string();
        assert_eq!(staging["slug"], "staging");

        let (status, body) = call(
            &router,
            &cookie,
            Method::PUT,
            &format!("{base}/variables"),
            Some(json!({ "variables": [
                { "key": "DB_URL", "value": secrets[0] },
                { "key": "REGION", "value": "eu-central" },
            ] })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        bodies.push(body);
        let (status, body) = call(
            &router,
            &cookie,
            Method::PUT,
            &format!("{base}/environments/{staging_id}/variables"),
            Some(json!({ "variables": [{ "key": "DB_URL", "value": secrets[1] }] })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let env_view: Value = serde_json::from_str(&body).unwrap();
        let db_url = env_view["variables"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["key"] == "DB_URL")
            .unwrap();
        assert_eq!(db_url["source"], "environment");
        assert_eq!(db_url["overrides"], json!(["project"]));
        bodies.push(body);

        let service_slug = format!("p{}", &uuid::Uuid::new_v4().simple().to_string()[..12]);
        let (status, body) = call(
            &router,
            &cookie,
            Method::POST,
            "/api/compose-projects",
            Some(json!({
                "name": "Web",
                "slug": service_slug,
                "environment_id": staging_id,
                "source": { "kind": "raw", "compose_yaml": "services:\n  web:\n    image: nginx\n" },
                "environment": [["API_TOKEN", secrets[2]]],
                "deploy": false,
            })),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        let service: Value = serde_json::from_str(&body).unwrap();
        let service_id = service["project"]["id"].as_str().unwrap().to_string();
        assert_eq!(service["project"]["environment_id"], staging_id.as_str());
        bodies.push(body);

        // Keep API_TOKEN as stored, override REGION on the service.
        let (status, body) = call(
            &router,
            &cookie,
            Method::PUT,
            &format!("/api/compose-projects/{service_id}/variables"),
            Some(json!({ "variables": [
                { "key": "API_TOKEN" },
                { "key": "REGION", "value": "us-east" },
            ] })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        bodies.push(body);
        let (status, body) = call(
            &router,
            &cookie,
            Method::GET,
            &format!("/api/compose-projects/{service_id}/variables"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let view: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(view["layered"], true);
        let sources: Vec<(String, String)> = view["variables"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| {
                (
                    v["key"].as_str().unwrap().to_string(),
                    v["source"].as_str().unwrap().to_string(),
                )
            })
            .collect();
        assert_eq!(
            sources,
            vec![
                ("API_TOKEN".into(), "service".into()),
                ("DB_URL".into(), "environment".into()),
                ("REGION".into(), "service".into()),
            ]
        );
        bodies.push(body);

        for path in [
            "/api/projects".to_string(),
            base.clone(),
            format!("{base}/variables"),
            format!("{base}/environments/{staging_id}/variables"),
            format!("/api/compose-projects/{service_id}"),
        ] {
            let (status, body) = call(&router, &cookie, Method::GET, &path, None).await;
            assert_eq!(status, StatusCode::OK, "{path}: {body}");
            bodies.push(body);
        }
        // Bad input errors must not echo values either.
        let (status, body) = call(
            &router,
            &cookie,
            Method::PUT,
            &format!("{base}/variables"),
            Some(
                json!({ "variables": [{ "key": "DB_URL", "value": format!("{}\n", secrets[0]) }] }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        bodies.push(body);

        // A non-empty environment or project cannot be deleted.
        let (status, _) = call(
            &router,
            &cookie,
            Method::DELETE,
            &format!("{base}/environments/{staging_id}"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        let (status, _) = call(&router, &cookie, Method::DELETE, &base, None).await;
        assert_eq!(status, StatusCode::CONFLICT);

        let audit: Vec<String> = sqlx::query_scalar(
            "SELECT metadata::text FROM audit_log WHERE metadata->>'project_id' IN ($1, $2)",
        )
        .bind(&project_id)
        .bind(&service_id)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert!(audit.len() >= 4, "{audit:?}");
        for text in bodies.iter().chain(&audit) {
            for secret in &secrets {
                assert!(!text.contains(secret.as_str()), "secret leaked into {text}");
            }
        }
    }
}
