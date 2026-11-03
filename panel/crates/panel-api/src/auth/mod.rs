pub mod password;
pub mod session;
pub mod totp;

use crate::db::users::UserRow;
use crate::error::ApiError;
use crate::state::AppState;
use axum::async_trait;
use axum::extract::{FromRef, FromRequestParts};
use axum::http::request::Parts;
use axum_extra::extract::CookieJar;

/// Extractor for a valid, authenticated session. Any route accepting this
/// as a parameter automatically requires authentication.
pub struct AuthUser(pub UserRow);

/// Extractor requiring the authenticated user to also be an admin.
pub struct AdminUser(pub UserRow);

#[async_trait]
impl<S> FromRequestParts<S> for AuthUser
where
    AppState: FromRef<S>,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let app_state = AppState::from_ref(state);
        let jar = CookieJar::from_headers(&parts.headers);
        let cookie = jar
            .get(session::SESSION_COOKIE_NAME)
            .ok_or(ApiError::Unauthorized)?;

        let token = app_state
            .cookie_cipher
            .decrypt(cookie.value())
            .map_err(|_| ApiError::Unauthorized)?;
        let token_hash = session::hash_token(&token);

        let user = crate::db::users::find_session_user(&app_state.db, &token_hash)
            .await?
            .ok_or(ApiError::Unauthorized)?;

        Ok(AuthUser(user))
    }
}

#[async_trait]
impl<S> FromRequestParts<S> for AdminUser
where
    AppState: FromRef<S>,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let AuthUser(user) = AuthUser::from_request_parts(parts, state).await?;
        if !user.is_admin {
            return Err(ApiError::Forbidden);
        }
        Ok(AdminUser(user))
    }
}
