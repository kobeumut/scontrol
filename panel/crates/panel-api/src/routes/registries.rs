//! Private Docker registry management. The password or token is write-only:
//! requests may set or replace it, and responses never include it (or any
//! value derived from it); they only report the non-secret metadata.

use crate::auth::AdminUser;
use crate::db;
use crate::db::registries::RegistryRow;
use crate::error::{ApiError, ApiResult};
use crate::services::registries as sealed;
use crate::state::AppState;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use panel_core::adapters::{DockerRegistryAuth, TestDockerRegistryParams};
use panel_core::error::CoreError;
use panel_core::types::validate;
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(list).post(create))
        .route("/test", post(test_new))
        .route("/:id", get(show).put(update).delete(remove))
        .route("/:id/test", post(test_saved))
}

#[derive(Serialize)]
struct RegistryView {
    id: Uuid,
    name: String,
    server_url: String,
    username: String,
    /// Always true: a registry cannot be stored without its credential.
    has_credential: bool,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl From<RegistryRow> for RegistryView {
    fn from(row: RegistryRow) -> Self {
        Self {
            id: row.id,
            name: row.name,
            server_url: row.server_url,
            username: row.username,
            has_credential: !row.credential_ciphertext.is_empty(),
            created_at: row.created_at,
            updated_at: row.updated_at,
        }
    }
}

#[derive(Deserialize)]
struct RegistryBody {
    name: String,
    server_url: String,
    username: String,
    /// Required on create. On update, omit to keep the stored one.
    #[serde(default)]
    password: Option<String>,
}

#[derive(Serialize)]
struct TestResult {
    ok: bool,
    message: String,
}

struct Checked {
    name: String,
    /// Normalized registry address (see `validate::registry_auth_key`).
    server: String,
    username: String,
}

fn check(body: &RegistryBody) -> ApiResult<Checked> {
    let name = body.name.trim();
    if name.is_empty() || name.chars().count() > 100 {
        return Err(ApiError::BadRequest("name must be 1–100 characters".into()));
    }
    let server = validate::registry_auth_key(&body.server_url).ok_or_else(|| {
        ApiError::BadRequest(
            "server must be a registry host such as ghcr.io or registry.example.com:5000".into(),
        )
    })?;
    if !validate::is_valid_git_username(&body.username) {
        return Err(ApiError::BadRequest(
            "username may contain letters, digits and . _ @ + - (up to 100 characters)".into(),
        ));
    }
    if let Some(password) = &body.password {
        if !validate::is_valid_registry_secret(password) {
            return Err(ApiError::BadRequest(
                "password must be 1–4096 printable characters without whitespace".into(),
            ));
        }
    }
    Ok(Checked {
        name: name.to_string(),
        server,
        username: body.username.clone(),
    })
}

async fn list(
    AdminUser(_): AdminUser,
    State(state): State<AppState>,
) -> ApiResult<Json<Vec<RegistryView>>> {
    let rows = db::registries::list(&state.db).await?;
    Ok(Json(rows.into_iter().map(Into::into).collect()))
}

async fn show(
    AdminUser(_): AdminUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<RegistryView>> {
    Ok(Json(find(&state, id).await?.into()))
}

fn name_taken(error: sqlx::Error) -> ApiError {
    match &error {
        sqlx::Error::Database(db) if db.is_unique_violation() => {
            ApiError::Conflict("a registry with this name already exists".into())
        }
        _ => error.into(),
    }
}

async fn create(
    AdminUser(user): AdminUser,
    State(state): State<AppState>,
    Json(body): Json<RegistryBody>,
) -> ApiResult<(StatusCode, Json<RegistryView>)> {
    let checked = check(&body)?;
    let password = body
        .password
        .as_deref()
        .ok_or_else(|| ApiError::BadRequest("password is required".into()))?;
    let id = Uuid::new_v4();
    let ciphertext = sealed::seal(state.notifier.secrets(), id, password);
    let row = db::registries::create(
        &state.db,
        id,
        &checked.name,
        &checked.server,
        &checked.username,
        &ciphertext,
    )
    .await
    .map_err(name_taken)?;
    audit(&state, user.id, "create_docker_registry", &row).await;
    Ok((StatusCode::CREATED, Json(row.into())))
}

async fn update(
    AdminUser(user): AdminUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(body): Json<RegistryBody>,
) -> ApiResult<Json<RegistryView>> {
    let checked = check(&body)?;
    let existing = find(&state, id).await?;
    let ciphertext = match body.password.as_deref() {
        Some(password) => sealed::seal(state.notifier.secrets(), id, password),
        None => {
            // Never send a stored credential to a different server or
            // account than it was saved for.
            if existing.server_url != checked.server || existing.username != checked.username {
                return Err(ApiError::BadRequest(
                    "enter the password again when the server or username changes".into(),
                ));
            }
            existing.credential_ciphertext
        }
    };
    let row = db::registries::update(
        &state.db,
        id,
        &checked.name,
        &checked.server,
        &checked.username,
        &ciphertext,
    )
    .await
    .map_err(name_taken)?
    .ok_or_else(not_found)?;
    audit(&state, user.id, "update_docker_registry", &row).await;
    Ok(Json(row.into()))
}

async fn remove(
    AdminUser(user): AdminUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    let existing = find(&state, id).await?;
    // Projects that used it go back to anonymous pulls (ON DELETE SET NULL).
    db::registries::delete(&state.db, id).await?;
    audit(&state, user.id, "delete_docker_registry", &existing).await;
    Ok(StatusCode::NO_CONTENT)
}

/// Tests the stored credential.
async fn test_saved(
    AdminUser(_): AdminUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<TestResult>> {
    let row = find(&state, id).await?;
    let auth = sealed::open(state.notifier.secrets(), &row)?;
    Ok(Json(login(&state, auth).await?))
}

/// Tests credentials before they are saved. Nothing is stored.
async fn test_new(
    AdminUser(_): AdminUser,
    State(state): State<AppState>,
    Json(body): Json<RegistryBody>,
) -> ApiResult<Json<TestResult>> {
    let checked = check(&body)?;
    let password = body
        .password
        .ok_or_else(|| ApiError::BadRequest("password is required".into()))?;
    let auth = DockerRegistryAuth {
        server: checked.server,
        username: checked.username,
        password,
    };
    Ok(Json(login(&state, auth).await?))
}

/// A rejected login is an expected outcome, reported in the body. Only
/// failures to reach the agent at all are errors.
async fn login(state: &AppState, auth: DockerRegistryAuth) -> ApiResult<TestResult> {
    match state
        .agent
        .test_docker_registry(TestDockerRegistryParams { auth })
        .await
    {
        Ok(()) => Ok(TestResult {
            ok: true,
            message: "login succeeded".into(),
        }),
        Err(CoreError::External(message)) => Ok(TestResult {
            ok: false,
            message: message.chars().take(500).collect(),
        }),
        Err(other) => Err(other.into()),
    }
}

fn not_found() -> ApiError {
    ApiError::NotFound("registry not found".into())
}

async fn find(state: &AppState, id: Uuid) -> ApiResult<RegistryRow> {
    db::registries::find(&state.db, id)
        .await?
        .ok_or_else(not_found)
}

/// Audit entries carry the registry's metadata only, never the credential.
async fn audit(state: &AppState, user_id: Uuid, action: &str, row: &RegistryRow) {
    db::audit::record(
        &state.db,
        Some(user_id),
        "user",
        action,
        Some(&row.name),
        json!({ "registry_id": row.id, "server": row.server_url, "username": row.username }),
    )
    .await
    .ok();
}

#[cfg(test)]
mod tests {
    use crate::test_support;
    use axum::body::Body;
    use axum::http::{Method, Request, StatusCode};
    use panel_core::adapters::mock::MockAdapter;
    use panel_core::error::CoreError;
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
        let response = router
            .clone()
            .oneshot(
                request
                    .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        (status, String::from_utf8(bytes.to_vec()).unwrap())
    }

    #[tokio::test]
    async fn registry_password_is_sealed_and_never_returned_or_audited() {
        let _serial = test_support::QUEUE_TESTS.lock().await;
        let pool = test_support::test_pool().await;
        let adapter = Arc::new(MockAdapter::new());
        let state = test_support::app_state(pool.clone(), adapter.clone());
        let admin = test_support::user(&pool, true).await;
        let cookie = test_support::session_cookie(&state, admin.id).await;
        let secrets = state.notifier.secrets().clone();
        let router = crate::routes::build_router(state);
        let password = format!("pw-{}", uuid::Uuid::new_v4());
        let name = format!("ghcr-{}", uuid::Uuid::new_v4());

        let (status, body) = call(
            &router,
            &cookie,
            Method::POST,
            "/api/registries",
            Some(json!({
                "name": name, "server_url": "GHCR.io", "username": "octocat",
                "password": password,
            })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        assert!(!body.contains(&password), "{body}");
        let created: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(created["server_url"], "ghcr.io");
        assert_eq!(created["has_credential"], true);
        assert!(created.get("password").is_none());
        assert!(created.get("credential_ciphertext").is_none());
        let id: uuid::Uuid = created["id"].as_str().unwrap().parse().unwrap();

        // At rest: sealed, not plaintext, and it opens only for this row.
        let row = crate::db::registries::find(&pool, id)
            .await
            .unwrap()
            .unwrap();
        assert!(!String::from_utf8_lossy(&row.credential_ciphertext).contains(&password));
        let auth = crate::services::registries::open(&secrets, &row).unwrap();
        assert_eq!(auth.password, password);
        let mut moved = row.clone();
        moved.id = uuid::Uuid::new_v4();
        assert!(crate::services::registries::open(&secrets, &moved).is_err());

        for uri in [
            "/api/registries".to_string(),
            format!("/api/registries/{id}"),
        ] {
            let (status, body) = call(&router, &cookie, Method::GET, &uri, None).await;
            assert_eq!(status, StatusCode::OK);
            assert!(!body.contains(&password), "{body}");
        }

        // Omitting the password keeps it, unless the server changes.
        let (status, body) = call(
            &router,
            &cookie,
            Method::PUT,
            &format!("/api/registries/{id}"),
            Some(json!({ "name": name, "server_url": "ghcr.io", "username": "octocat" })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let kept = crate::db::registries::find(&pool, id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(kept.credential_ciphertext, row.credential_ciphertext);
        let (status, _) = call(
            &router,
            &cookie,
            Method::PUT,
            &format!("/api/registries/{id}"),
            Some(json!({ "name": name, "server_url": "evil.example", "username": "octocat" })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        // Hostile input is refused.
        for bad in [
            json!({ "name": "x", "server_url": "ghcr.io/org", "username": "u", "password": "p" }),
            json!({ "name": "x", "server_url": "ghcr.io", "username": "-u", "password": "p" }),
            json!({ "name": "x", "server_url": "ghcr.io", "username": "u", "password": "a b" }),
            json!({ "name": "x", "server_url": "ghcr.io", "username": "u" }),
        ] {
            let (status, _) =
                call(&router, &cookie, Method::POST, "/api/registries", Some(bad)).await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
        }
        // Names are unique.
        let (status, _) = call(
            &router,
            &cookie,
            Method::POST,
            "/api/registries",
            Some(
                json!({ "name": name, "server_url": "ghcr.io", "username": "u", "password": "p" }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);

        // The audit trail names the registry but not its credential.
        let audit: Vec<serde_json::Value> = sqlx::query_scalar(
            "SELECT metadata FROM audit_log WHERE actor_user_id = $1 AND action LIKE '%docker_registry'",
        )
        .bind(admin.id)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert!(!audit.is_empty());
        assert!(audit.iter().all(|m| !m.to_string().contains(&password)));

        // Testing reports a rejected login in the body, with the agent's reason.
        let (status, body) = call(
            &router,
            &cookie,
            Method::POST,
            &format!("/api/registries/{id}/test"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(serde_json::from_str::<Value>(&body).unwrap()["ok"], true);
        adapter.fail_next_call_with(
            "test_docker_registry",
            CoreError::External("unauthorized: bad credentials".into()),
        );
        let (status, body) = call(
            &router,
            &cookie,
            Method::POST,
            &format!("/api/registries/{id}/test"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let result: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(result["ok"], false);
        assert!(result["message"].as_str().unwrap().contains("unauthorized"));

        let (status, _) = call(
            &router,
            &cookie,
            Method::DELETE,
            &format!("/api/registries/{id}"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (status, _) = call(
            &router,
            &cookie,
            Method::GET,
            &format!("/api/registries/{id}"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn requests_without_an_admin_session_are_refused() {
        let pool = test_support::test_pool().await;
        let state = test_support::app_state(pool, Arc::new(MockAdapter::new()));
        let router = crate::routes::build_router(state);
        let (status, _) = call(&router, "", Method::GET, "/api/registries", None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
}
