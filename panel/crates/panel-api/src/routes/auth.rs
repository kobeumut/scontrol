use crate::auth::{password, session, totp, AuthUser};
use crate::db::users;
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;
use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use axum_extra::extract::cookie::{Cookie, SameSite};
use axum_extra::extract::CookieJar;
use serde::{Deserialize, Serialize};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/bootstrap-admin", post(bootstrap_admin))
        .route("/login", post(login))
        .route("/logout", post(logout))
        .route("/totp/enroll", post(totp_enroll))
        .route("/totp/verify", post(totp_verify))
        .route("/me", axum::routing::get(me))
}

#[derive(Deserialize)]
struct BootstrapAdminRequest {
    username: String,
    email: String,
    password: String,
}

#[derive(Serialize)]
struct UserResponse {
    id: uuid::Uuid,
    username: String,
    email: String,
    is_admin: bool,
    totp_enabled: bool,
}

impl From<users::UserRow> for UserResponse {
    fn from(u: users::UserRow) -> Self {
        Self {
            id: u.id,
            username: u.username,
            email: u.email,
            is_admin: u.is_admin,
            totp_enabled: u.totp_enabled,
        }
    }
}

/// Creates the first admin user. Only works while the `users` table is
/// empty; after that, use the CLI (`panel-cli user create`) or the admin UI.
async fn bootstrap_admin(
    State(state): State<AppState>,
    Json(req): Json<BootstrapAdminRequest>,
) -> ApiResult<Json<UserResponse>> {
    if !panel_core::types::validate::is_valid_username(&req.username) {
        return Err(ApiError::BadRequest("invalid username".into()));
    }
    if req.password.len() < 12 {
        return Err(ApiError::BadRequest(
            "password must be at least 12 characters".into(),
        ));
    }
    let existing = users::count_users(&state.db).await?;
    if existing > 0 {
        return Err(ApiError::Forbidden);
    }
    let hash = password::hash_password(&req.password)
        .map_err(|e| ApiError::Internal(anyhow::anyhow!(e)))?;
    let user = users::create_user(&state.db, &req.username, &req.email, &hash, true).await?;
    crate::db::audit::record(
        &state.db,
        Some(user.id),
        "user",
        "bootstrap_admin",
        Some(&user.username),
        serde_json::json!({}),
    )
    .await
    .ok();
    Ok(Json(user.into()))
}

#[derive(Deserialize)]
struct LoginRequest {
    username: String,
    password: String,
    totp_code: Option<String>,
}

async fn login(
    State(state): State<AppState>,
    jar: CookieJar,
    Json(req): Json<LoginRequest>,
) -> ApiResult<(CookieJar, Json<UserResponse>)> {
    let user = users::find_by_username(&state.db, &req.username)
        .await?
        .ok_or(ApiError::Unauthorized)?;

    if !password::verify_password(&req.password, &user.password_hash) {
        return Err(ApiError::Unauthorized);
    }

    if user.totp_enabled {
        let secret = user.totp_secret.as_deref().ok_or(ApiError::Unauthorized)?;
        let code = req.totp_code.as_deref().ok_or(ApiError::Unauthorized)?;
        if !totp::verify_code(secret, &user.username, code) {
            return Err(ApiError::Unauthorized);
        }
    }

    let (token, token_hash) = session::generate_token();
    let expires_at = chrono::Utc::now() + session::session_ttl(state.session_ttl_hours);
    users::create_session(&state.db, user.id, &token_hash, expires_at).await?;

    let cookie_value = state
        .cookie_cipher
        .encrypt(&token)
        .map_err(ApiError::Internal)?;
    let cookie = Cookie::build((session::SESSION_COOKIE_NAME, cookie_value))
        .http_only(true)
        .secure(true)
        .same_site(SameSite::Strict)
        .path("/")
        .max_age(time::Duration::hours(state.session_ttl_hours))
        .build();

    crate::db::audit::record(
        &state.db,
        Some(user.id),
        "user",
        "login",
        Some(&user.username),
        serde_json::json!({}),
    )
    .await
    .ok();

    Ok((jar.add(cookie), Json(user.into())))
}

async fn logout(State(state): State<AppState>, jar: CookieJar) -> ApiResult<CookieJar> {
    if let Some(cookie) = jar.get(session::SESSION_COOKIE_NAME) {
        if let Ok(token) = state.cookie_cipher.decrypt(cookie.value()) {
            let hash = session::hash_token(&token);
            users::delete_session(&state.db, &hash).await.ok();
        }
    }
    Ok(jar.remove(Cookie::from(session::SESSION_COOKIE_NAME)))
}

async fn me(AuthUser(user): AuthUser) -> Json<UserResponse> {
    Json(user.into())
}

#[derive(Serialize)]
struct TotpEnrollResponse {
    secret: String,
    otpauth_url: String,
}

async fn totp_enroll(
    AuthUser(user): AuthUser,
    State(state): State<AppState>,
) -> ApiResult<Json<TotpEnrollResponse>> {
    let enrollment = totp::generate_enrollment(&user.username).map_err(ApiError::Internal)?;
    users::set_totp_secret(&state.db, user.id, &enrollment.secret_base32).await?;
    Ok(Json(TotpEnrollResponse {
        secret: enrollment.secret_base32,
        otpauth_url: enrollment.otpauth_url,
    }))
}

#[derive(Deserialize)]
struct TotpVerifyRequest {
    code: String,
}

async fn totp_verify(
    AuthUser(user): AuthUser,
    State(state): State<AppState>,
    Json(req): Json<TotpVerifyRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    let secret = user
        .totp_secret
        .as_deref()
        .ok_or_else(|| ApiError::BadRequest("call /totp/enroll first".into()))?;
    if !totp::verify_code(secret, &user.username, &req.code) {
        return Err(ApiError::BadRequest("invalid code".into()));
    }
    users::enable_totp(&state.db, user.id).await?;
    Ok(Json(serde_json::json!({ "totp_enabled": true })))
}
