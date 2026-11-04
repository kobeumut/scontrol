//! Notification channel management. Secrets are write-only: a request may
//! set or replace a channel's secret, and responses only report whether one
//! is stored (`has_secret`).

use crate::auth::AdminUser;
use crate::db;
use crate::db::notifications::{ChannelInput, ChannelRow};
use crate::error::{ApiError, ApiResult};
use crate::services::notifications::{validate_events, ChannelConfig, Notification, Notifier};
use crate::state::AppState;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post, put};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(list).post(create))
        .route("/events", get(events))
        .route("/:id", put(update).delete(remove))
        .route("/:id/test", post(test))
}

#[derive(Serialize)]
struct ChannelView {
    id: Uuid,
    name: String,
    #[serde(flatten)]
    channel: ChannelConfig,
    has_secret: bool,
    events: Vec<String>,
    enabled: bool,
    last_sent_at: Option<DateTime<Utc>>,
    last_error: Option<String>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl TryFrom<ChannelRow> for ChannelView {
    type Error = ApiError;

    fn try_from(row: ChannelRow) -> ApiResult<Self> {
        Ok(Self {
            channel: serde_json::from_value(row.config)
                .map_err(|e| ApiError::Internal(anyhow::anyhow!("stored channel config: {e}")))?,
            id: row.id,
            name: row.name,
            has_secret: row.secret_sealed.is_some(),
            events: row.events,
            enabled: row.enabled,
            last_sent_at: row.last_sent_at,
            last_error: row.last_error,
            created_at: row.created_at,
            updated_at: row.updated_at,
        })
    }
}

#[derive(Deserialize)]
struct ChannelBody {
    name: String,
    #[serde(flatten)]
    channel: ChannelConfig,
    events: Vec<String>,
    #[serde(default = "default_true")]
    enabled: bool,
    /// Omit to keep the stored secret (same kind only); `""` clears it.
    #[serde(default)]
    secret: Option<String>,
}

fn default_true() -> bool {
    true
}

async fn events(AdminUser(_): AdminUser) -> Json<&'static [&'static str]> {
    Json(crate::services::notifications::EVENTS)
}

async fn list(
    AdminUser(_): AdminUser,
    State(state): State<AppState>,
) -> ApiResult<Json<Vec<ChannelView>>> {
    let rows = db::notifications::list(&state.db).await?;
    Ok(Json(
        rows.into_iter()
            .map(ChannelView::try_from)
            .collect::<ApiResult<_>>()?,
    ))
}

/// Validates the body and seals the secret that should be stored: the new
/// one, or (when omitted on an update of the same kind) the existing one.
fn prepare(
    state: &AppState,
    id: Uuid,
    body: &ChannelBody,
    existing: Option<&ChannelRow>,
) -> ApiResult<Option<Vec<u8>>> {
    let name = body.name.trim();
    if name.is_empty() || name.len() > 100 {
        return Err(ApiError::BadRequest("name must be 1–100 characters".into()));
    }
    validate_events(&body.events).map_err(ApiError::BadRequest)?;
    let kind = body.channel.kind();
    let aad = Notifier::secret_aad(id, kind);
    let secrets = state.notifier.secrets();
    let (plain, sealed) = match body.secret.as_deref() {
        Some("") => (None, None),
        Some(secret) => (
            Some(secret.to_string()),
            Some(secrets.seal(secret.as_bytes(), &aad)),
        ),
        None => match existing.filter(|row| row.kind == kind) {
            Some(ChannelRow {
                secret_sealed: Some(sealed),
                ..
            }) => {
                let plain = secrets
                    .open(sealed, &aad)
                    .map_err(|e| ApiError::BadRequest(e.to_string()))?;
                let plain = String::from_utf8(plain)
                    .map_err(|_| ApiError::BadRequest("stored secret is corrupt".into()))?;
                (Some(plain), Some(sealed.clone()))
            }
            _ => (None, None),
        },
    };
    body.channel
        .validate(plain.as_deref())
        .map_err(ApiError::BadRequest)?;
    Ok(sealed)
}

async fn create(
    AdminUser(user): AdminUser,
    State(state): State<AppState>,
    Json(body): Json<ChannelBody>,
) -> ApiResult<(StatusCode, Json<ChannelView>)> {
    let id = Uuid::new_v4();
    let secret_sealed = prepare(&state, id, &body, None)?;
    let row = db::notifications::create(
        &state.db,
        id,
        ChannelInput {
            name: body.name.trim(),
            kind: body.channel.kind(),
            config: serde_json::to_value(&body.channel).map_err(anyhow::Error::from)?,
            secret_sealed,
            events: &body.events,
            enabled: body.enabled,
        },
    )
    .await?;
    audit(&state, user.id, "create_notification_channel", &row).await;
    Ok((StatusCode::CREATED, Json(row.try_into()?)))
}

async fn update(
    AdminUser(user): AdminUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(body): Json<ChannelBody>,
) -> ApiResult<Json<ChannelView>> {
    let existing = find(&state, id).await?;
    let secret_sealed = prepare(&state, id, &body, Some(&existing))?;
    let row = db::notifications::update(
        &state.db,
        id,
        ChannelInput {
            name: body.name.trim(),
            kind: body.channel.kind(),
            config: serde_json::to_value(&body.channel).map_err(anyhow::Error::from)?,
            secret_sealed,
            events: &body.events,
            enabled: body.enabled,
        },
    )
    .await?
    .ok_or_else(not_found)?;
    audit(&state, user.id, "update_notification_channel", &row).await;
    Ok(Json(row.try_into()?))
}

async fn remove(
    AdminUser(user): AdminUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    let existing = find(&state, id).await?;
    db::notifications::delete(&state.db, id).await?;
    audit(&state, user.id, "delete_notification_channel", &existing).await;
    Ok(StatusCode::NO_CONTENT)
}

/// Sends a test message now and reports the outcome directly.
async fn test(
    AdminUser(_): AdminUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<serde_json::Value>> {
    let channel = find(&state, id).await?;
    let result = state
        .notifier
        .deliver_and_record(&state.db, &channel, &Notification::test())
        .await;
    Ok(Json(match result {
        Ok(()) => json!({ "delivered": true }),
        Err(error) => json!({ "delivered": false, "error": error }),
    }))
}

async fn find(state: &AppState, id: Uuid) -> ApiResult<ChannelRow> {
    db::notifications::find(&state.db, id)
        .await?
        .ok_or_else(not_found)
}

fn not_found() -> ApiError {
    ApiError::NotFound("notification channel not found".into())
}

async fn audit(state: &AppState, user_id: Uuid, action: &str, row: &ChannelRow) {
    db::audit::record(
        &state.db,
        Some(user_id),
        "user",
        action,
        Some(&row.name),
        json!({ "channel_id": row.id, "kind": row.kind, "events": row.events }),
    )
    .await
    .ok();
}

#[cfg(test)]
mod tests {
    use crate::services::notifications::sign;
    use crate::test_support;
    use axum::body::Body;
    use axum::http::{Method, Request, StatusCode};
    use panel_core::adapters::mock::MockAdapter;
    use serde_json::{json, Value};
    use std::sync::Arc;
    use tower::ServiceExt;

    async fn receiver() -> (String, tokio::sync::mpsc::Receiver<(String, String)>) {
        test_support::http_receiver().await
    }

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
    async fn webhook_channel_keeps_its_secret_sealed_and_delivers_signed_events() {
        let _serial = test_support::QUEUE_TESTS.lock().await;
        let pool = test_support::test_pool().await;
        let adapter = Arc::new(MockAdapter::new());
        let state = test_support::app_state(pool.clone(), adapter.clone());
        let admin = test_support::user(&pool, true).await;
        let cookie = test_support::session_cookie(&state, admin.id).await;
        let notifier = state.notifier.clone();
        let router = crate::routes::build_router(state);
        let (url, mut received) = receiver().await;
        let secret = format!("sign-{}", uuid::Uuid::new_v4());

        let (status, body) = call(
            &router,
            &cookie,
            Method::POST,
            "/api/notifications",
            Some(json!({
                "name": "Ops hook", "kind": "webhook", "url": url,
                "events": ["deploy_succeeded"], "secret": secret,
            })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let created: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(created["has_secret"], true);
        let id = created["id"].as_str().unwrap().to_string();

        // Updating without a secret keeps it; changing to a kind whose
        // secret is required, without giving one, is refused.
        let (status, body) = call(&router, &cookie, Method::PUT, &format!("/api/notifications/{id}"), Some(json!({
            "name": "Ops hook 2", "kind": "webhook", "url": url, "events": ["deploy_succeeded", "deploy_failed"],
        }))).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            serde_json::from_str::<Value>(&body).unwrap()["has_secret"],
            true
        );
        let (status, _) = call(
            &router,
            &cookie,
            Method::PUT,
            &format!("/api/notifications/{id}"),
            Some(json!({
                "name": "x", "kind": "slack", "events": ["deploy_failed"],
            })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        let (status, body) = call(
            &router,
            &cookie,
            Method::POST,
            &format!("/api/notifications/{id}/test"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            serde_json::from_str::<Value>(&body).unwrap()["delivered"],
            true,
            "{body}"
        );
        let (head, payload) = received.recv().await.unwrap();
        assert!(head.contains("x-scontrol-event: test"), "{head}");
        assert!(
            head.contains(&format!(
                "x-scontrol-signature: {}",
                sign(&secret, payload.as_bytes())
            )),
            "{head}"
        );

        // A finished deployment notifies the channel.
        let project = crate::db::compose_projects::create(
            &pool,
            uuid::Uuid::new_v4(),
            "Notify test",
            &format!("p{}", &uuid::Uuid::new_v4().simple().to_string()[..12]),
            admin.id,
            None,
            &panel_core::types::ComposeSource::Raw {
                compose_yaml: "services: {}".into(),
            },
        )
        .await
        .unwrap();
        let deployment = crate::db::compose_projects::enqueue_deployment(
            &pool,
            uuid::Uuid::new_v4(),
            project.id,
            "agent://test",
            false,
            "manual",
            None,
        )
        .await
        .unwrap();
        crate::db::compose_projects::finish_deployment(
            &pool,
            deployment.id,
            panel_core::types::DeploymentStatus::Succeeded,
            None,
            Some("0123456789abcdef"),
        )
        .await
        .unwrap();
        notifier.deployment_finished(pool.clone(), project.id, deployment.id);
        let (head, payload) =
            tokio::time::timeout(std::time::Duration::from_secs(5), received.recv())
                .await
                .unwrap()
                .unwrap();
        assert!(
            head.contains("x-scontrol-event: deploy_succeeded"),
            "{head}"
        );
        let payload: Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(payload["project"]["slug"], project.slug);
        assert_eq!(payload["deployment"]["commit_sha"], "0123456789abcdef");

        // The secret is in no response, audit row, or plaintext column.
        let (_, listed) = call(&router, &cookie, Method::GET, "/api/notifications", None).await;
        let stored: Vec<String> = sqlx::query_scalar(
            "SELECT to_jsonb(c)::text FROM notification_channels c WHERE id = $1::uuid
             UNION ALL SELECT metadata::text FROM audit_log WHERE metadata->>'channel_id' = $1",
        )
        .bind(&id)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert!(stored.len() >= 3);
        for text in stored.iter().chain([&listed, &created.to_string()]) {
            assert!(!text.contains(&secret), "secret leaked into {text}");
        }
        let sealed: Vec<u8> = sqlx::query_scalar(
            "SELECT secret_sealed FROM notification_channels WHERE id = $1::uuid",
        )
        .bind(&id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(!sealed.windows(secret.len()).any(|w| w == secret.as_bytes()));

        let (status, _) = call(
            &router,
            &cookie,
            Method::DELETE,
            &format!("/api/notifications/{id}"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
    }
}
