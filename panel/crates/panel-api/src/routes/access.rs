//! Role-based access control: the permission catalog, roles, and role
//! bindings (`/api/access`).
//!
//! Managing roles and bindings needs `access:manage` at global scope, which
//! no API key can hold. Nobody can hand out more than they hold: a custom
//! role may only contain permissions its author holds globally, and a
//! binding only a role whose every permission the author holds at the
//! binding's scope.

use crate::auth::{perm, Authenticated, Principal, Require};
use crate::db;
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{delete, get, put};
use axum::{Json, Router};
use panel_core::rbac::{self, Grant, Permission, Scope, Target};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/permissions", get(permissions))
        .route("/me", get(me))
        .route("/users", get(users))
        .route("/roles", get(roles).post(create_role))
        .route("/roles/:id", put(update_role).delete(remove_role))
        .route("/bindings", get(bindings).post(create_binding))
        .route("/bindings/:id", delete(remove_binding))
}

#[derive(Serialize)]
struct PermissionInfo {
    key: Permission,
    description: &'static str,
    /// Grantable in a project or environment binding.
    scopable: bool,
    /// Grantable to an API key.
    api_keys: bool,
}

async fn permissions(_: Authenticated) -> Json<Vec<PermissionInfo>> {
    Json(
        Permission::ALL
            .into_iter()
            .map(|key| PermissionInfo {
                key,
                description: key.description(),
                scopable: key.is_scopable(),
                api_keys: key.allowed_for_api_keys(),
            })
            .collect(),
    )
}

#[derive(Serialize)]
struct Me {
    user_id: Uuid,
    username: String,
    superuser: bool,
    api_key: bool,
    grants: Vec<Grant>,
}

/// The caller's own grants, so the UI can hide what would be refused. The
/// server still checks every request.
async fn me(Authenticated(principal): Authenticated) -> Json<Me> {
    let grants = match &principal.authority.key {
        Some(key) => key.grants.clone(),
        None => principal.authority.user.grants.clone(),
    };
    Json(Me {
        user_id: principal.id,
        username: principal.username.clone(),
        superuser: principal.authority.user.superuser && principal.api_key_id.is_none(),
        api_key: principal.api_key_id.is_some(),
        grants,
    })
}

#[derive(Serialize, sqlx::FromRow)]
struct UserSummary {
    id: Uuid,
    username: String,
    is_admin: bool,
}

async fn users(
    _: Require<perm::AccessManage>,
    State(state): State<AppState>,
) -> ApiResult<Json<Vec<UserSummary>>> {
    Ok(Json(
        sqlx::query_as("SELECT id, username, is_admin FROM users ORDER BY username")
            .fetch_all(&state.db)
            .await?,
    ))
}

async fn roles(
    _: Authenticated,
    State(state): State<AppState>,
) -> ApiResult<Json<Vec<db::rbac::Role>>> {
    Ok(Json(db::rbac::list_roles(&state.db).await?))
}

#[derive(Deserialize)]
struct RoleBody {
    name: String,
    #[serde(default)]
    description: String,
    permissions: Vec<Permission>,
}

impl RoleBody {
    fn check(&mut self, principal: &Principal) -> ApiResult<()> {
        if !rbac::is_valid_role_name(&self.name) {
            return Err(ApiError::BadRequest(
                "role names are 2–40 lowercase letters, digits, and dashes".into(),
            ));
        }
        if self.description.len() > 200 {
            return Err(ApiError::BadRequest("description is too long".into()));
        }
        self.permissions.sort();
        self.permissions.dedup();
        if self.permissions.is_empty() {
            return Err(ApiError::BadRequest("a role needs a permission".into()));
        }
        if let Some(p) = self
            .permissions
            .iter()
            .find(|p| !principal.allows(**p, Target::Global))
        {
            return Err(ApiError::BadRequest(format!(
                "you cannot put {p} in a role: you do not hold it globally"
            )));
        }
        Ok(())
    }
}

async fn create_role(
    Require { principal, .. }: Require<perm::AccessManage>,
    State(state): State<AppState>,
    Json(mut body): Json<RoleBody>,
) -> ApiResult<(StatusCode, Json<db::rbac::Role>)> {
    body.check(&principal)?;
    let id = db::rbac::save_role(
        &state.db,
        None,
        &body.name,
        &body.description,
        &body.permissions,
    )
    .await
    .map_err(|e| conflict_on_unique(e, "a role with this name exists"))?
    .ok_or_else(|| ApiError::Internal(anyhow::anyhow!("role not created")))?;
    audit(
        &state,
        &principal,
        "create_role",
        &body.name,
        serde_json::json!({ "role_id": id, "permissions": body.permissions }),
    )
    .await;
    let role = db::rbac::find_role(&state.db, id)
        .await?
        .ok_or_else(|| ApiError::NotFound("role not found".into()))?;
    Ok((StatusCode::CREATED, Json(role)))
}

async fn update_role(
    Require { principal, .. }: Require<perm::AccessManage>,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(mut body): Json<RoleBody>,
) -> ApiResult<Json<db::rbac::Role>> {
    body.check(&principal)?;
    let existing = db::rbac::find_role(&state.db, id)
        .await?
        .ok_or_else(|| ApiError::NotFound("role not found".into()))?;
    if existing.builtin {
        return Err(ApiError::Conflict(
            "built-in roles cannot be changed".into(),
        ));
    }
    db::rbac::save_role(
        &state.db,
        Some(id),
        &body.name,
        &body.description,
        &body.permissions,
    )
    .await
    .map_err(|e| conflict_on_unique(e, "a role with this name exists"))?
    .ok_or_else(|| ApiError::NotFound("role not found".into()))?;
    audit(
        &state,
        &principal,
        "update_role",
        &body.name,
        serde_json::json!({ "role_id": id, "permissions": body.permissions }),
    )
    .await;
    let role = db::rbac::find_role(&state.db, id)
        .await?
        .ok_or_else(|| ApiError::NotFound("role not found".into()))?;
    Ok(Json(role))
}

async fn remove_role(
    Require { principal, .. }: Require<perm::AccessManage>,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    let role = db::rbac::find_role(&state.db, id)
        .await?
        .ok_or_else(|| ApiError::NotFound("role not found".into()))?;
    if role.builtin {
        return Err(ApiError::Conflict(
            "built-in roles cannot be deleted".into(),
        ));
    }
    db::rbac::delete_role(&state.db, id).await?;
    audit(
        &state,
        &principal,
        "delete_role",
        &role.name,
        serde_json::json!({ "role_id": id }),
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct BindingQuery {
    user_id: Option<Uuid>,
}

async fn bindings(
    _: Require<perm::AccessManage>,
    State(state): State<AppState>,
    Query(query): Query<BindingQuery>,
) -> ApiResult<Json<Vec<db::rbac::Binding>>> {
    Ok(Json(
        db::rbac::list_bindings(&state.db, query.user_id).await?,
    ))
}

#[derive(Deserialize)]
struct BindingBody {
    user_id: Uuid,
    role_id: Uuid,
    scope: Scope,
}

/// Checks that a scope names an existing project (and an environment of
/// it), and returns the target it covers.
pub(crate) async fn scope_target(state: &AppState, scope: Scope) -> ApiResult<Target> {
    match scope {
        Scope::Global => Ok(Target::Global),
        Scope::Project { project_id } => {
            db::projects::find_by_id(&state.db, project_id)
                .await?
                .ok_or_else(|| ApiError::BadRequest("project not found".into()))?;
            Ok(Target::Project(project_id))
        }
        Scope::Environment {
            project_id,
            environment_id,
        } => {
            db::projects::find_environment(&state.db, environment_id)
                .await?
                .filter(|e| e.project_id == project_id)
                .ok_or_else(|| ApiError::BadRequest("environment not found in project".into()))?;
            Ok(Target::Environment {
                project: project_id,
                environment: environment_id,
            })
        }
    }
}

async fn create_binding(
    Require { principal, .. }: Require<perm::AccessManage>,
    State(state): State<AppState>,
    Json(body): Json<BindingBody>,
) -> ApiResult<(StatusCode, Json<db::rbac::Binding>)> {
    let user = db::users::find_by_id(&state.db, body.user_id)
        .await?
        .ok_or_else(|| ApiError::BadRequest("user not found".into()))?;
    let role = db::rbac::find_role(&state.db, body.role_id)
        .await?
        .ok_or_else(|| ApiError::BadRequest("role not found".into()))?;
    let target = scope_target(&state, body.scope).await?;
    if let Some(p) = role
        .permissions
        .iter()
        .find(|p| !principal.allows(**p, target))
    {
        return Err(ApiError::BadRequest(format!(
            "you cannot bind {}: you do not hold {p} at this scope",
            role.name
        )));
    }
    let id = db::rbac::create_binding(&state.db, user.id, role.id, body.scope, Some(principal.id))
        .await
        .map_err(|e| conflict_on_unique(e, "this user already has this role here"))?;
    audit(
        &state,
        &principal,
        "create_role_binding",
        &user.username,
        serde_json::json!({ "binding_id": id, "role": role.name, "scope": body.scope }),
    )
    .await;
    let binding = db::rbac::find_binding(&state.db, id)
        .await?
        .ok_or_else(|| ApiError::NotFound("binding not found".into()))?;
    Ok((StatusCode::CREATED, Json(binding)))
}

async fn remove_binding(
    Require { principal, .. }: Require<perm::AccessManage>,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    let binding = db::rbac::find_binding(&state.db, id)
        .await?
        .ok_or_else(|| ApiError::NotFound("binding not found".into()))?;
    db::rbac::delete_binding(&state.db, id).await?;
    audit(
        &state,
        &principal,
        "delete_role_binding",
        &binding.username,
        serde_json::json!({ "binding_id": id, "role": binding.role_name }),
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}

pub(crate) async fn audit(
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

pub(crate) fn conflict_on_unique(error: sqlx::Error, message: &str) -> ApiError {
    match &error {
        sqlx::Error::Database(db) if db.is_unique_violation() => ApiError::Conflict(message.into()),
        _ => error.into(),
    }
}
