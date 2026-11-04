pub mod api_keys;
pub mod password;
pub mod session;
pub mod totp;

use crate::db::users::UserRow;
use crate::error::ApiError;
use crate::state::AppState;
use axum::async_trait;
use axum::extract::{FromRef, FromRequestParts, RawPathParams};
use axum::http::request::Parts;
use axum_extra::extract::CookieJar;
use panel_core::rbac::{Authority, GrantSet, Permission, Target};
use std::marker::PhantomData;
use uuid::Uuid;

/// Who a request acts for: a session user, or an API key acting for its
/// owner. Checks go through [`Authority`], which narrows a key to its own
/// grants. Dereferences to the user (the key's owner for an API key), whose
/// id is the audit actor.
#[derive(Clone)]
pub struct Principal {
    pub user: UserRow,
    pub api_key_id: Option<Uuid>,
    pub authority: Authority,
}

impl std::ops::Deref for Principal {
    type Target = UserRow;

    fn deref(&self) -> &UserRow {
        &self.user
    }
}

impl Principal {
    /// `audit_log.actor_kind` for this principal.
    pub fn actor_kind(&self) -> &'static str {
        if self.api_key_id.is_some() {
            "api_key"
        } else {
            "user"
        }
    }

    pub fn allows(&self, permission: Permission, target: Target) -> bool {
        self.authority.allows(permission, target)
    }

    /// Fail-closed check: `403` unless a grant covers `target`.
    pub fn require(&self, permission: Permission, target: Target) -> Result<(), ApiError> {
        if self.allows(permission, target) {
            Ok(())
        } else {
            Err(ApiError::Forbidden)
        }
    }

    async fn resolve(parts: &Parts, state: &AppState) -> Result<Principal, ApiError> {
        // A request that presents a bearer token is judged by it alone and
        // never falls back to a cookie.
        if let Some(header) = parts.headers.get(axum::http::header::AUTHORIZATION) {
            let token = header
                .to_str()
                .ok()
                .and_then(|v| v.strip_prefix("Bearer "))
                .ok_or(ApiError::Unauthorized)?;
            return Self::from_api_key(token.trim(), state).await;
        }
        let jar = CookieJar::from_headers(&parts.headers);
        let cookie = jar
            .get(session::SESSION_COOKIE_NAME)
            .ok_or(ApiError::Unauthorized)?;
        let token = state
            .cookie_cipher
            .decrypt(cookie.value())
            .map_err(|_| ApiError::Unauthorized)?;
        let user = crate::db::users::find_session_user(&state.db, &session::hash_token(&token))
            .await?
            .ok_or(ApiError::Unauthorized)?;
        let authority = Authority {
            user: user_grants(state, &user).await?,
            key: None,
        };
        Ok(Principal {
            user,
            api_key_id: None,
            authority,
        })
    }

    async fn from_api_key(token: &str, state: &AppState) -> Result<Principal, ApiError> {
        let (prefix, secret) = api_keys::parse(token).ok_or(ApiError::Unauthorized)?;
        let key = crate::db::api_keys::find_active(&state.db, prefix)
            .await?
            .ok_or(ApiError::Unauthorized)?;
        if !state
            .api_keys
            .verify(key.id, secret, &key.secret_hash)
            .await
        {
            return Err(ApiError::Unauthorized);
        }
        let user = crate::db::users::find_by_id(&state.db, key.user_id)
            .await?
            .ok_or(ApiError::Unauthorized)?;
        crate::db::api_keys::touch(&state.db, key.id).await.ok();
        let authority = Authority {
            user: user_grants(state, &user).await?,
            key: Some(GrantSet::new(key.grants)),
        };
        Ok(Principal {
            user,
            api_key_id: Some(key.id),
            authority,
        })
    }
}

async fn user_grants(state: &AppState, user: &UserRow) -> Result<GrantSet, ApiError> {
    if user.is_admin {
        return Ok(GrantSet::superuser());
    }
    Ok(GrantSet::new(
        crate::db::rbac::user_grants(&state.db, user.id).await?,
    ))
}

/// Any authenticated principal (session or API key). Resolved once per
/// request and shared by every extractor below.
pub struct Authenticated(pub Principal);

#[async_trait]
impl<S> FromRequestParts<S> for Authenticated
where
    AppState: FromRef<S>,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        if let Some(principal) = parts.extensions.get::<Principal>() {
            return Ok(Authenticated(principal.clone()));
        }
        let principal = Principal::resolve(parts, &AppState::from_ref(state)).await?;
        parts.extensions.insert(principal.clone());
        Ok(Authenticated(principal))
    }
}

impl Authenticated {
    /// `principal` with its user's grants read again, after a change to
    /// that user's bindings in the same request.
    pub async fn refresh(state: &AppState, principal: Principal) -> Result<Principal, ApiError> {
        let user = user_grants(state, &principal.user).await?;
        Ok(Principal {
            authority: Authority {
                user,
                key: principal.authority.key.clone(),
            },
            ..principal
        })
    }
}

/// A session user (no API key): for managing keys and access, which must
/// not be reachable from a key, and for the older owner-scoped routes.
pub struct SessionUser(pub Principal);

#[async_trait]
impl<S> FromRequestParts<S> for SessionUser
where
    AppState: FromRef<S>,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let Authenticated(principal) = Authenticated::from_request_parts(parts, state).await?;
        if principal.api_key_id.is_some() {
            return Err(ApiError::Forbidden);
        }
        Ok(SessionUser(principal))
    }
}

/// Extractor for a valid, authenticated session. Any route accepting this
/// as a parameter automatically requires authentication.
pub struct AuthUser(pub UserRow);

#[async_trait]
impl<S> FromRequestParts<S> for AuthUser
where
    AppState: FromRef<S>,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let SessionUser(principal) = SessionUser::from_request_parts(parts, state).await?;
        Ok(AuthUser(principal.user))
    }
}

/// Server-wide administration: a session user holding `system:admin`
/// globally (the installation admin, or a global `admin` binding). API keys
/// never pass it.
pub struct AdminUser(pub UserRow);

#[async_trait]
impl<S> FromRequestParts<S> for AdminUser
where
    AppState: FromRef<S>,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let SessionUser(principal) = SessionUser::from_request_parts(parts, state).await?;
        principal.require(Permission::SystemAdmin, Target::Global)?;
        Ok(AdminUser(principal.user))
    }
}

/// A permission named at the type level, so a handler's signature states
/// what it needs (`ServiceAccess<perm::ServicesDeploy>`).
pub trait PermissionMarker: Send + Sync + 'static {
    const PERMISSION: Permission;
}

pub mod perm {
    use super::PermissionMarker;
    use panel_core::rbac::Permission;

    macro_rules! markers {
        ($($name:ident),* $(,)?) => {
            $(
                pub struct $name;
                impl PermissionMarker for $name {
                    const PERMISSION: Permission = Permission::$name;
                }
            )*
        };
    }

    markers!(
        ProjectsCreate,
        ProjectsUpdate,
        ProjectsDelete,
        ServicesRead,
        ServicesUpdate,
        ServicesDelete,
        ServicesDeploy,
        VariablesRead,
        VariablesUpdate,
        ServersRead,
        ServersManage,
        AccessManage,
    );
}

/// A permission held at global scope.
pub struct Require<P: PermissionMarker> {
    pub principal: Principal,
    _permission: PhantomData<P>,
}

#[async_trait]
impl<S, P> FromRequestParts<S> for Require<P>
where
    AppState: FromRef<S>,
    S: Send + Sync,
    P: PermissionMarker,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let Authenticated(principal) = Authenticated::from_request_parts(parts, state).await?;
        principal.require(P::PERMISSION, Target::Global)?;
        Ok(Require {
            principal,
            _permission: PhantomData,
        })
    }
}

/// The UUID path parameter `name`, if the route has one.
async fn path_uuid<S: Send + Sync>(
    parts: &mut Parts,
    state: &S,
    name: &str,
) -> Result<Option<Uuid>, ApiError> {
    let params = RawPathParams::from_request_parts(parts, state)
        .await
        .map_err(|_| ApiError::BadRequest("invalid path".into()))?;
    let Some((_, value)) = params.iter().find(|(key, _)| *key == name) else {
        return Ok(None);
    };
    value
        .parse()
        .map(Some)
        .map_err(|_| ApiError::BadRequest(format!("invalid {name}")))
}

/// A permission on the project in the `:id` path parameter, or on the
/// environment in `:environment_id` when the route has one (it must belong
/// to the project). `404` when either does not exist.
pub struct ProjectAccess<P: PermissionMarker> {
    pub principal: Principal,
    _permission: PhantomData<P>,
}

#[async_trait]
impl<S, P> FromRequestParts<S> for ProjectAccess<P>
where
    AppState: FromRef<S>,
    S: Send + Sync,
    P: PermissionMarker,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let Authenticated(principal) = Authenticated::from_request_parts(parts, state).await?;
        let project_id = path_uuid(parts, state, "id")
            .await?
            .ok_or_else(|| ApiError::BadRequest("missing project id".into()))?;
        let environment_id = path_uuid(parts, state, "environment_id").await?;
        let app = AppState::from_ref(state);
        let target = match environment_id {
            Some(environment_id) => {
                crate::db::projects::find_environment(&app.db, environment_id)
                    .await?
                    .filter(|e| e.project_id == project_id)
                    .ok_or_else(|| ApiError::NotFound("environment not found".into()))?;
                Target::Environment {
                    project: project_id,
                    environment: environment_id,
                }
            }
            None => {
                crate::db::projects::find_by_id(&app.db, project_id)
                    .await?
                    .ok_or_else(|| ApiError::NotFound("project not found".into()))?;
                Target::Project(project_id)
            }
        };
        principal.require(P::PERMISSION, target)?;
        Ok(ProjectAccess {
            principal,
            _permission: PhantomData,
        })
    }
}

/// Where a service sits: its environment, project, and server.
#[derive(Debug, Clone, Copy)]
pub struct ServiceRef {
    pub project_id: Uuid,
    pub environment_id: Uuid,
    pub server_id: Uuid,
}

impl ServiceRef {
    pub fn target(&self) -> Target {
        Target::Environment {
            project: self.project_id,
            environment: self.environment_id,
        }
    }
}

pub async fn find_service_ref(db: &sqlx::PgPool, id: Uuid) -> Result<Option<ServiceRef>, ApiError> {
    let row: Option<(Uuid, Uuid, Uuid)> = sqlx::query_as(
        "SELECT e.project_id, c.environment_id, c.server_id
         FROM compose_projects c JOIN environments e ON e.id = c.environment_id
         WHERE c.id = $1",
    )
    .bind(id)
    .fetch_optional(db)
    .await?;
    Ok(
        row.map(|(project_id, environment_id, server_id)| ServiceRef {
            project_id,
            environment_id,
            server_id,
        }),
    )
}

/// A permission on the service (Compose project) in the `:id` path
/// parameter, checked against its environment. `404` when it does not exist.
pub struct ServiceAccess<P: PermissionMarker> {
    pub principal: Principal,
    pub service: ServiceRef,
    _permission: PhantomData<P>,
}

#[async_trait]
impl<S, P> FromRequestParts<S> for ServiceAccess<P>
where
    AppState: FromRef<S>,
    S: Send + Sync,
    P: PermissionMarker,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let Authenticated(principal) = Authenticated::from_request_parts(parts, state).await?;
        let id = path_uuid(parts, state, "id")
            .await?
            .ok_or_else(|| ApiError::BadRequest("missing service id".into()))?;
        let service = find_service_ref(&AppState::from_ref(state).db, id)
            .await?
            .ok_or_else(|| ApiError::NotFound("compose project not found".into()))?;
        principal.require(P::PERMISSION, service.target())?;
        Ok(ServiceAccess {
            principal,
            service,
            _permission: PhantomData,
        })
    }
}
