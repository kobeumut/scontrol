//! The vetted template catalogue (`/api/templates`).
//!
//! Templates ship with the panel (`panel_core::templates`). Installing one
//! creates a raw-Compose project: random passwords and salts are generated
//! here and stored only in the project's write-only environment layer. The
//! Compose file keeps `${VAR}` references, so no secret is ever part of the
//! stored source. The agent applies the same Compose validator as for any
//! user-supplied file when it deploys.

use crate::auth::{Authenticated, Principal};
use crate::db;
use crate::db::projects::EnvScope;
use crate::error::{ApiError, ApiResult};
use crate::services::env_layers;
use crate::state::AppState;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use panel_core::rbac::{Permission, Target};
use panel_core::templates::{self, Generator, SuggestedDomain, Template};
use panel_core::types::{validate, ComposeProject, ComposeSource, DeploymentInfo};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use uuid::Uuid;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(list))
        .route("/:id", get(show))
        .route("/:id/install", post(install))
}

#[derive(Serialize)]
struct InputField {
    name: String,
    label: String,
    default: Option<String>,
}

#[derive(Serialize)]
struct TemplateView {
    id: String,
    name: String,
    description: String,
    category: String,
    suggested_domain: Option<SuggestedDomain>,
    /// Values the user may set; everything else is generated at install.
    inputs: Vec<InputField>,
    /// Names of variables generated at install (values are never shown).
    generated: Vec<String>,
}

fn view(template: &Template) -> TemplateView {
    let meta = &template.meta;
    TemplateView {
        id: meta.id.clone(),
        name: meta.name.clone(),
        description: meta.description.clone(),
        category: meta.category.clone(),
        suggested_domain: meta.suggested_domain.clone(),
        inputs: meta
            .variables
            .iter()
            .filter(|v| v.generator == Generator::Input)
            .map(|v| InputField {
                name: v.name.clone(),
                label: if v.label.is_empty() {
                    v.name.clone()
                } else {
                    v.label.clone()
                },
                default: v.default.clone(),
            })
            .collect(),
        generated: meta
            .variables
            .iter()
            .filter(|v| v.generator != Generator::Input)
            .map(|v| v.name.clone())
            .collect(),
    }
}

async fn list(Authenticated(_): Authenticated) -> Json<Vec<TemplateView>> {
    Json(templates::catalogue().iter().map(view).collect())
}

async fn show(
    Authenticated(_): Authenticated,
    Path(id): Path<String>,
) -> ApiResult<Json<TemplateView>> {
    let template =
        templates::find(&id).ok_or_else(|| ApiError::NotFound("template not found".into()))?;
    Ok(Json(view(&template)))
}

#[derive(Deserialize)]
struct InstallBody {
    name: String,
    slug: String,
    #[serde(default)]
    environment_id: Option<Uuid>,
    /// Values for the template's input variables.
    #[serde(default)]
    inputs: BTreeMap<String, String>,
    #[serde(default = "default_true")]
    deploy: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Serialize)]
struct Installed {
    project: ComposeProject,
    deployment: Option<DeploymentInfo>,
    /// Domain binding to add next, if the template suggests one.
    suggested_domain: Option<SuggestedDomain>,
}

// Installing always creates a service and writes generated variables.
fn require_install(user: &Principal, target: Target, deploy: bool) -> ApiResult<()> {
    user.require(Permission::TemplatesInstall, target)?;
    user.require(Permission::ServicesCreate, target)?;
    user.require(Permission::VariablesUpdate, target)?;
    if deploy {
        user.require(Permission::ServicesDeploy, target)?;
    }
    Ok(())
}

async fn install(
    Authenticated(user): Authenticated,
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<InstallBody>,
) -> ApiResult<(StatusCode, Json<Installed>)> {
    let template =
        templates::find(&id).ok_or_else(|| ApiError::NotFound("template not found".into()))?;
    if body.name.trim().is_empty() || body.name.len() > 100 {
        return Err(ApiError::BadRequest(
            "project name must be 1..=100 characters".into(),
        ));
    }
    if !validate::is_valid_project_slug(&body.slug) {
        return Err(ApiError::BadRequest("invalid project slug".into()));
    }
    let target = match body.environment_id {
        Some(id) => {
            let env = db::projects::find_environment(&state.db, id)
                .await?
                .ok_or_else(|| ApiError::BadRequest("environment not found".into()))?;
            Target::Environment {
                project: env.project_id,
                environment: env.id,
            }
        }
        None => Target::Global,
    };
    require_install(&user, target, body.deploy)?;
    let environment =
        templates::generate_variables(&template, &body.inputs).map_err(ApiError::BadRequest)?;
    env_layers::validate_pairs(&environment)?;

    let source = ComposeSource::Raw {
        compose_yaml: template.compose_yaml.clone(),
    };
    let server_id = super::compose_projects::place(&state, None, false, &source).await?;
    let environment_id = match body.environment_id {
        Some(id) => id,
        None => db::projects::default_environment(&state.db, user.id).await?,
    };
    let mut tx = state.db.begin().await?;
    let mut project = db::compose_projects::create_in(
        &mut tx,
        Uuid::new_v4(),
        &body.name,
        &body.slug,
        user.id,
        environment_id,
        &source,
    )
    .await
    .map_err(|e| {
        let unique = e
            .downcast_ref::<sqlx::Error>()
            .and_then(|e| e.as_database_error())
            .is_some_and(|e| e.is_unique_violation());
        if unique {
            ApiError::Conflict("a project with this slug already exists".into())
        } else {
            ApiError::Internal(e)
        }
    })?;
    sqlx::query("UPDATE compose_projects SET server_id = $1 WHERE id = $2")
        .bind(server_id)
        .bind(project.id)
        .execute(&mut *tx)
        .await?;
    project.server_id = server_id;
    // No worker may see a partially initialized service on the wrong host.
    env_layers::replace_in(
        &mut tx,
        &state.secret_box,
        EnvScope::Service(project.id),
        &environment,
    )
    .await?;
    tx.commit().await?;
    let deployment = if body.deploy {
        Some(
            state
                .deploy_queue
                .enqueue(&state.db, project.id, &project.slug, true, "template")
                .await?,
        )
    } else {
        None
    };
    db::audit::record(
        &state.db,
        Some(user.id),
        user.actor_kind(),
        "install_template",
        Some(&project.slug),
        serde_json::json!({ "project_id": project.id, "template": template.meta.id }),
    )
    .await
    .ok();
    let project = db::compose_projects::find_by_id(&state.db, project.id)
        .await?
        .ok_or_else(|| ApiError::NotFound("Compose project not found".into()))?;
    Ok((
        StatusCode::ACCEPTED,
        Json(Installed {
            project,
            deployment,
            suggested_domain: template.meta.suggested_domain.clone(),
        }),
    ))
}
