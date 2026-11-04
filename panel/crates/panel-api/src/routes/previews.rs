//! Pull request preview settings and previews of a Compose project
//! (`/api/compose-projects/:id/previews`). The lifecycle itself is driven
//! by the project's webhook (`services::previews`).
//!
//! The comment token is write-only: it is stored sealed and the API only
//! reports whether one is set.

use crate::auth::AdminUser;
use crate::db;
use crate::db::previews::{PreviewRow, SettingsInput, SettingsRow};
use crate::error::{ApiError, ApiResult};
use crate::services::previews as preview_service;
use crate::state::AppState;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{delete, get, put};
use axum::{Json, Router};
use panel_core::previews::{self, ResourceLimits};
use panel_core::types::{validate, ComposeProject};
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

/// Routes merged into `/api/compose-projects`.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/:id/previews", get(overview).put(update_settings))
        .route(
            "/:id/previews/comment-token",
            put(set_comment_token).delete(remove_comment_token),
        )
        .route("/:id/previews/:number", delete(remove_preview))
}

#[derive(Serialize)]
struct Overview {
    /// Whether this project can have previews at all (Git source, a slug
    /// short enough for `-pr-<n>`, not a preview itself).
    available: bool,
    unavailable_reason: Option<&'static str>,
    /// Pull request events arrive on the push webhook.
    webhook_enabled: bool,
    settings: Option<SettingsRow>,
    previews: Vec<PreviewRow>,
}

fn unavailable(project: &ComposeProject, is_preview: bool) -> Option<&'static str> {
    if is_preview {
        Some("this project is itself a preview")
    } else if project.source.git_checkout().is_none() {
        Some("previews need a Git-sourced project")
    } else if !previews::parent_slug_supports_previews(&project.slug) {
        Some("the project slug is too long for the -pr-<n> suffix (37 characters at most)")
    } else {
        None
    }
}

async fn overview(
    AdminUser(_): AdminUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Overview>> {
    let project = find_project(&state, id).await?;
    let is_preview = db::previews::find_by_preview_project(&state.db, id)
        .await?
        .is_some();
    let reason = unavailable(&project, is_preview);
    Ok(Json(Overview {
        available: reason.is_none(),
        unavailable_reason: reason,
        webhook_enabled: db::compose_projects::webhook_nonce(&state.db, id)
            .await?
            .is_some(),
        settings: db::previews::settings(&state.db, id).await?,
        previews: db::previews::list_for_parent(&state.db, id).await?,
    }))
}

#[derive(Deserialize)]
struct SettingsBody {
    enabled: bool,
    /// `preview.example.com` or `*.preview.example.com`.
    base_domain: String,
    service_name: String,
    container_port: u16,
    #[serde(default)]
    https: bool,
    /// certbot lineage of an existing wildcard certificate.
    #[serde(default)]
    wildcard_certificate: Option<String>,
    #[serde(default = "default_max_active")]
    max_active: u32,
    #[serde(default = "default_ttl")]
    ttl_hours: u32,
    #[serde(default)]
    resource_limits: Option<ResourceLimits>,
}

fn default_max_active() -> u32 {
    previews::DEFAULT_MAX_ACTIVE
}

fn default_ttl() -> u32 {
    previews::DEFAULT_TTL_HOURS
}

async fn update_settings(
    AdminUser(user): AdminUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(body): Json<SettingsBody>,
) -> ApiResult<Json<SettingsRow>> {
    let project = find_project(&state, id).await?;
    let is_preview = db::previews::find_by_preview_project(&state.db, id)
        .await?
        .is_some();
    if let Some(reason) = unavailable(&project, is_preview) {
        return Err(ApiError::BadRequest(reason.into()));
    }
    let base_domain = previews::normalize_base_domain(&body.base_domain)
        .ok_or_else(|| ApiError::BadRequest("invalid preview base domain".into()))?;
    // The longest preview host must still be a valid domain.
    let longest = format!("{}-pr-{}", project.slug, previews::MAX_PR_NUMBER);
    if previews::preview_host(&longest, &base_domain).is_none() {
        return Err(ApiError::BadRequest(
            "the preview base domain is too long".into(),
        ));
    }
    if !validate::is_valid_service_name(&body.service_name) {
        return Err(ApiError::BadRequest("invalid service name".into()));
    }
    if body.container_port == 0 {
        return Err(ApiError::BadRequest(
            "container_port must be 1..=65535".into(),
        ));
    }
    let certificate = body
        .wildcard_certificate
        .as_deref()
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .map(|c| c.to_ascii_lowercase());
    if let Some(certificate) = &certificate {
        if !validate::is_valid_domain(certificate) {
            return Err(ApiError::BadRequest(
                "the wildcard certificate must name a certbot lineage such as preview.example.com"
                    .into(),
            ));
        }
        if !body.https {
            return Err(ApiError::BadRequest(
                "a wildcard certificate needs HTTPS enabled".into(),
            ));
        }
    }
    previews::validate_limits(body.max_active, body.ttl_hours).map_err(ApiError::BadRequest)?;
    let limits = body.resource_limits.unwrap_or_default();
    limits.validate().map_err(ApiError::BadRequest)?;

    let was_enabled = db::previews::settings(&state.db, id)
        .await?
        .is_some_and(|s| s.enabled);
    let settings = db::previews::upsert_settings(
        &state.db,
        id,
        &SettingsInput {
            enabled: body.enabled,
            base_domain: &base_domain,
            service_name: &body.service_name,
            container_port: body.container_port,
            https: body.https,
            wildcard_certificate: certificate.as_deref(),
            max_active: body.max_active,
            ttl_hours: body.ttl_hours,
            memory_mb: limits.memory_mb,
            cpu_millis: limits.cpu_millis,
        },
    )
    .await?;
    db::audit::record(
        &state.db,
        Some(user.id),
        "user",
        "update_preview_settings",
        Some(&project.slug),
        json!({
            "project_id": id,
            "enabled": body.enabled,
            "base_domain": base_domain,
            "max_active": body.max_active,
            "ttl_hours": body.ttl_hours,
            "memory_mb": limits.memory_mb,
            "cpu_millis": limits.cpu_millis,
        }),
    )
    .await
    .ok();
    // Disabling removes every preview of the project (in the background:
    // each teardown is a `docker compose down`).
    if was_enabled && !body.enabled {
        let (db, agent, previews) = (
            state.db.clone(),
            state.agent.clone(),
            state.previews.clone(),
        );
        tokio::spawn(async move {
            if let Err(error) =
                preview_service::close_all(&db, agent.as_ref(), &previews, id, "disabled").await
            {
                tracing::warn!(project_id = %id, %error, "removing previews failed");
            }
        });
    }
    Ok(Json(settings))
}

#[derive(Deserialize)]
struct CommentTokenBody {
    token: String,
}

async fn set_comment_token(
    AdminUser(user): AdminUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(body): Json<CommentTokenBody>,
) -> ApiResult<StatusCode> {
    let project = find_project(&state, id).await?;
    let token = body.token.trim();
    if token.is_empty()
        || token.len() > 4096
        || token.chars().any(|c| c.is_control() || c.is_whitespace())
    {
        return Err(ApiError::BadRequest(
            "the token must be 1..=4096 characters without whitespace".into(),
        ));
    }
    let sealed = state.previews.seal_comment_token(id, token);
    if !db::previews::set_comment_token(&state.db, id, Some(&sealed)).await? {
        return Err(ApiError::BadRequest(
            "save the preview settings before adding a comment token".into(),
        ));
    }
    db::audit::record(
        &state.db,
        Some(user.id),
        "user",
        "set_preview_comment_token",
        Some(&project.slug),
        json!({ "project_id": id }),
    )
    .await
    .ok();
    Ok(StatusCode::NO_CONTENT)
}

async fn remove_comment_token(
    AdminUser(user): AdminUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    let project = find_project(&state, id).await?;
    db::previews::set_comment_token(&state.db, id, None).await?;
    db::audit::record(
        &state.db,
        Some(user.id),
        "user",
        "remove_preview_comment_token",
        Some(&project.slug),
        json!({ "project_id": id }),
    )
    .await
    .ok();
    Ok(StatusCode::NO_CONTENT)
}

/// Removes one preview now (the request stays open; a new push recreates
/// it).
async fn remove_preview(
    AdminUser(user): AdminUser,
    State(state): State<AppState>,
    Path((id, number)): Path<(Uuid, i64)>,
) -> ApiResult<(StatusCode, Json<serde_json::Value>)> {
    let project = find_project(&state, id).await?;
    let preview = db::previews::find_by_pr(&state.db, id, number)
        .await?
        .ok_or_else(|| ApiError::NotFound("preview not found".into()))?;
    db::previews::mark_closing(&state.db, preview.id, "removed").await?;
    db::audit::record(
        &state.db,
        Some(user.id),
        "user",
        "close_preview",
        Some(&preview.slug),
        json!({ "parent_project_id": project.id, "pr_number": number, "reason": "removed" }),
    )
    .await
    .ok();
    preview_service::spawn_teardown(
        state.db.clone(),
        state.agent.clone(),
        state.previews.clone(),
        id,
        number,
    );
    Ok((StatusCode::ACCEPTED, Json(json!({ "closing": true }))))
}

async fn find_project(state: &AppState, id: Uuid) -> ApiResult<ComposeProject> {
    db::compose_projects::find_by_id(&state.db, id)
        .await?
        .ok_or_else(|| ApiError::NotFound("Compose project not found".into()))
}

#[cfg(test)]
mod tests {
    use crate::db;
    use crate::services::previews as preview_service;
    use crate::test_support;
    use axum::body::Body;
    use axum::http::{Method, Request, StatusCode};
    use hmac::{Hmac, Mac};
    use panel_core::adapters::mock::MockAdapter;
    use panel_core::types::ComposeSource;
    use serde_json::{json, Value};
    use sha2::Sha256;
    use std::sync::Arc;
    use tower::ServiceExt;
    use uuid::Uuid;

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    async fn call(
        router: &axum::Router,
        cookie: &str,
        method: Method,
        uri: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
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
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    fn pr_event(action: &str, number: u64, head_repo: u64) -> Value {
        json!({
            "action": action,
            "number": number,
            "pull_request": {
                "number": number,
                "head": { "ref": format!("feature-{number}"), "sha": SHA, "repo": { "id": head_repo } },
                "base": { "ref": "main", "sha": SHA, "repo": { "id": 1 } },
            },
        })
    }

    async fn deliver(
        router: &axum::Router,
        project: Uuid,
        secret: &str,
        body: Value,
    ) -> (StatusCode, Value) {
        let body = body.to_string();
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(body.as_bytes());
        let signature = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));
        let request = Request::post(format!("/api/webhooks/compose/{project}"))
            .header("x-github-event", "pull_request")
            .header("x-hub-signature-256", signature)
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap();
        let response = router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    /// A Git project with previews enabled (limit 2) and its webhook secret.
    async fn setup() -> (
        crate::state::AppState,
        Arc<MockAdapter>,
        axum::Router,
        String,
        Uuid,
        String,
        String,
    ) {
        let pool = test_support::test_pool().await;
        let adapter = Arc::new(MockAdapter::new());
        let state = test_support::app_state(pool.clone(), adapter.clone());
        let owner = test_support::user(&pool, true).await;
        let cookie = test_support::session_cookie(&state, owner.id).await;
        let slug = format!("pv{}", &Uuid::new_v4().simple().to_string()[..10]);
        let project = db::compose_projects::create(
            &pool,
            Uuid::new_v4(),
            "Preview parent",
            &slug,
            owner.id,
            None,
            &ComposeSource::Git {
                repository_url: "https://github.com/acme/shop.git".into(),
                branch: "main".into(),
                compose_path: "compose.yml".into(),
            },
        )
        .await
        .unwrap();
        let nonce = crate::services::webhook::new_nonce();
        db::compose_projects::set_webhook_nonce(&pool, project.id, Some(&nonce))
            .await
            .unwrap();
        let secret = state.webhook_key.secret_for(project.id, &nonce);
        let router = crate::routes::build_router(state.clone());
        let base = format!("{}.example.com", &Uuid::new_v4().simple().to_string()[..8]);
        let (status, settings) = call(
            &router,
            &cookie,
            Method::PUT,
            &format!("/api/compose-projects/{}/previews", project.id),
            Some(json!({
                "enabled": true,
                "base_domain": format!("*.{base}"),
                "service_name": "web",
                "container_port": 3000,
                "max_active": 2,
                "ttl_hours": 48,
                "resource_limits": { "memory_mb": 256, "cpu_millis": 250 },
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{settings}");
        assert_eq!(settings["base_domain"], base);
        assert_eq!(settings["has_comment_token"], false);
        (state, adapter, router, cookie, project.id, secret, slug)
    }

    #[tokio::test]
    async fn pull_request_lifecycle_creates_redeploys_limits_and_destroys_previews() {
        let _guard = test_support::QUEUE_TESTS.lock().await;
        let (state, adapter, router, cookie, project, secret, slug) = setup().await;
        let pool = state.db.clone();

        // Opened: a preview project with its domain and a queued deployment.
        let (status, opened) = deliver(&router, project, &secret, pr_event("opened", 1, 1)).await;
        assert_eq!(status, StatusCode::ACCEPTED, "{opened}");
        assert_eq!(opened["preview"], format!("{slug}-pr-1"));
        assert_eq!(opened["created"], true);
        let first = db::previews::find_by_pr(&pool, project, 1)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first.status, "active");
        assert_eq!(first.head_branch, "feature-1");
        assert!(first
            .domain
            .as_deref()
            .unwrap()
            .starts_with(&format!("{slug}-pr-1.")));
        let preview_project = db::compose_projects::find_by_id(&pool, first.preview_project_id)
            .await
            .unwrap()
            .unwrap();
        match &preview_project.source {
            ComposeSource::Git { branch, .. } => assert_eq!(branch, "feature-1"),
            other => panic!("unexpected source {other:?}"),
        }
        // Deployed with limits, as an ephemeral project.
        let options = preview_service::deploy_options(&pool, first.preview_project_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(options.parent_project_id, project);
        assert_eq!(options.limits.memory_mb, 256);
        assert_eq!(options.limits.cpu_millis, 250);
        assert!(preview_service::deploy_options(&pool, project)
            .await
            .unwrap()
            .is_none());

        // The worker deploys it with the parent's service variables, the
        // limits, and as an ephemeral project.
        crate::services::env_layers::replace(
            &pool,
            &state.secret_box,
            crate::db::projects::EnvScope::Service(project),
            &[("PARENT_ONLY".into(), "from-parent".into())],
        )
        .await
        .unwrap();
        let deployment: Uuid = opened["deployment"]["id"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        crate::services::deploy_queue::run_job(
            &pool,
            adapter.as_ref(),
            &state.secret_box,
            &state.github_apps,
            crate::db::compose_projects::ClaimedDeployment {
                id: deployment,
                project_id: first.preview_project_id,
                pull_images: false,
                rollback_of: None,
            },
        )
        .await;
        {
            let deploys = adapter.deploys.lock().unwrap();
            let sent = deploys.last().unwrap();
            assert_eq!(sent.project_slug, format!("{slug}-pr-1"));
            assert!(sent.ephemeral);
            assert_eq!(
                sent.resource_limits,
                Some(panel_core::previews::ResourceLimits {
                    memory_mb: 256,
                    cpu_millis: 250
                })
            );
            assert_eq!(sent.domains.len(), 1);
            assert_eq!(sent.domains[0].service_name, "web");
            assert_eq!(sent.domains[0].container_port, 3000);
            let writes = adapter.environment_writes.lock().unwrap();
            let env = &writes.last().unwrap().environment;
            assert!(env
                .iter()
                .any(|(k, v)| k == "PARENT_ONLY" && v == "from-parent"));
        }

        // A push queues a new deployment; a second push coalesces into it.
        let (status, synced) =
            deliver(&router, project, &secret, pr_event("synchronize", 1, 1)).await;
        assert_eq!(status, StatusCode::ACCEPTED, "{synced}");
        assert_eq!(synced["coalesced"], false, "{synced}");
        let (_, again) = deliver(&router, project, &secret, pr_event("synchronize", 1, 1)).await;
        assert_eq!(again["coalesced"], true, "{again}");

        // Forks are refused; a second request fits; a third hits the limit.
        let (status, fork) = deliver(&router, project, &secret, pr_event("opened", 9, 77)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            fork["ignored"],
            "pull requests from forks are not previewed"
        );
        let (_, second) = deliver(&router, project, &secret, pr_event("opened", 2, 1)).await;
        assert_eq!(second["created"], true, "{second}");
        let (status, third) = deliver(&router, project, &secret, pr_event("opened", 3, 1)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(third["limit"], 2, "{third}");
        assert!(db::previews::find_by_pr(&pool, project, 3)
            .await
            .unwrap()
            .is_none());

        // Listing shows both previews.
        let (status, overview) = call(
            &router,
            &cookie,
            Method::GET,
            &format!("/api/compose-projects/{project}/previews"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(overview["available"], true);
        assert_eq!(overview["previews"].as_array().unwrap().len(), 2);

        // Closed: closing, then torn down (agent removal + rows gone).
        let (status, closed) = deliver(&router, project, &secret, pr_event("closed", 1, 1)).await;
        assert_eq!(status, StatusCode::ACCEPTED, "{closed}");
        let mut gone = false;
        for _ in 0..50 {
            if db::previews::find_by_pr(&pool, project, 1)
                .await
                .unwrap()
                .is_none()
            {
                gone = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(gone, "the closed preview was not torn down");
        assert!(
            db::compose_projects::find_by_id(&pool, first.preview_project_id)
                .await
                .unwrap()
                .is_none()
        );
        assert!(adapter
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|c| c == "remove_compose_project"));
        // Closing again is a no-op; the freed slot admits request 3.
        let (_, again) = deliver(&router, project, &secret, pr_event("closed", 1, 1)).await;
        assert!(again["ignored"].is_string(), "{again}");
        let (_, third) = deliver(&router, project, &secret, pr_event("opened", 3, 1)).await;
        assert_eq!(third["created"], true, "{third}");

        // Clean up the queue rows this test created.
        preview_service::close_all(&pool, adapter.as_ref(), &state.previews, project, "removed")
            .await
            .unwrap();
        assert!(db::previews::list_for_parent(&pool, project)
            .await
            .unwrap()
            .is_empty());
    }

    /// Close racing with queueing and claiming (review of PR #12): a
    /// closing preview's jobs are never claimed, a job claimed before the
    /// close is cancelled and waited out, and a transition blocked behind a
    /// close queues nothing.
    #[tokio::test]
    async fn closing_previews_never_start_a_deployment() {
        let _guard = test_support::QUEUE_TESTS.lock().await;
        let (state, adapter, router, _cookie, project, secret, _slug) = setup().await;
        let pool = state.db.clone();
        let count_jobs = |status: &'static str, project_id: Uuid| {
            let pool = pool.clone();
            async move {
                sqlx::query_scalar::<_, i64>(
                    "SELECT count(*) FROM compose_deployments WHERE project_id = $1 AND status = $2",
                )
                .bind(project_id)
                .bind(status)
                .fetch_one(&pool)
                .await
                .unwrap()
            }
        };
        // 1. A job queued for a closing preview (e.g. through the manual
        //    deploy endpoint) is never claimed, and teardown cancels it.
        let (_, opened) = deliver(&router, project, &secret, pr_event("opened", 7, 1)).await;
        assert_eq!(opened["created"], true, "{opened}");
        let first = db::previews::find_by_pr(&pool, project, 7)
            .await
            .unwrap()
            .unwrap();
        db::previews::mark_closing(&pool, first.id, "closed")
            .await
            .unwrap();
        let claimed =
            db::compose_projects::claim_next_deployment_of(&pool, Some(first.preview_project_id))
                .await
                .unwrap();
        assert!(claimed.is_none(), "a closing preview's job was claimed");
        assert!(
            preview_service::teardown(&pool, adapter.as_ref(), &state.previews, &first)
                .await
                .unwrap()
        );
        assert!(db::previews::find_by_pr(&pool, project, 7)
            .await
            .unwrap()
            .is_none());

        // 2. A job claimed before the close keeps the preview until it stops.
        let (_, opened) = deliver(&router, project, &secret, pr_event("opened", 8, 1)).await;
        assert_eq!(opened["created"], true, "{opened}");
        let second = db::previews::find_by_pr(&pool, project, 8)
            .await
            .unwrap()
            .unwrap();
        let claimed =
            db::compose_projects::claim_next_deployment_of(&pool, Some(second.preview_project_id))
                .await
                .unwrap()
                .expect("the preview job is claimable while active");
        assert_eq!(claimed.project_id, second.preview_project_id);
        db::previews::mark_closing(&pool, second.id, "closed")
            .await
            .unwrap();
        let row = db::previews::find_by_pr(&pool, project, 8)
            .await
            .unwrap()
            .unwrap();
        assert!(
            !preview_service::teardown(&pool, adapter.as_ref(), &state.previews, &row)
                .await
                .unwrap()
        );
        assert!(adapter
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|c| c == "cancel_deployment"));
        db::compose_projects::finish_deployment(
            &pool,
            claimed.id,
            panel_core::types::DeploymentStatus::Cancelled,
            None,
            None,
        )
        .await
        .unwrap();
        assert!(
            preview_service::teardown(&pool, adapter.as_ref(), &state.previews, &row)
                .await
                .unwrap()
        );

        // 3. A push that arrives while a close holds the parent lock waits
        //    for it, sees `closing`, and queues nothing.
        let (_, opened) = deliver(&router, project, &secret, pr_event("opened", 9, 1)).await;
        assert_eq!(opened["created"], true, "{opened}");
        let third = db::previews::find_by_pr(&pool, project, 9)
            .await
            .unwrap()
            .unwrap();
        let job: Uuid = opened["deployment"]["id"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        db::compose_projects::finish_deployment(
            &pool,
            job,
            panel_core::types::DeploymentStatus::Succeeded,
            None,
            Some(SHA),
        )
        .await
        .unwrap();
        let mut close = pool.begin().await.unwrap();
        db::previews::lock_parent(&mut close, project)
            .await
            .unwrap();
        let push = {
            let (router, secret) = (router.clone(), secret.clone());
            tokio::spawn(async move {
                deliver(&router, project, &secret, pr_event("synchronize", 9, 1)).await
            })
        };
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert!(
            !push.is_finished(),
            "the push did not wait for the parent lock"
        );
        sqlx::query("UPDATE preview_deployments SET status = 'closing', close_reason = 'closed' WHERE id = $1")
            .bind(third.id)
            .execute(&mut *close)
            .await
            .unwrap();
        close.commit().await.unwrap();
        let (status, pushed) = push.await.unwrap();
        assert_eq!(status, StatusCode::OK, "{pushed}");
        assert!(pushed.get("deployment").is_none(), "{pushed}");
        assert_eq!(count_jobs("queued", third.preview_project_id).await, 0);
        assert_eq!(count_jobs("running", third.preview_project_id).await, 0);

        preview_service::close_all(&pool, adapter.as_ref(), &state.previews, project, "removed")
            .await
            .unwrap();
        assert!(db::previews::list_for_parent(&pool, project)
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn ttl_sweep_expires_idle_previews_only() {
        let _guard = test_support::QUEUE_TESTS.lock().await;
        let (state, adapter, router, _cookie, project, secret, _slug) = setup().await;
        let pool = state.db.clone();
        for number in [5, 6] {
            let (_, opened) =
                deliver(&router, project, &secret, pr_event("opened", number, 1)).await;
            assert_eq!(opened["created"], true, "{opened}");
        }
        let idle = db::previews::find_by_pr(&pool, project, 5)
            .await
            .unwrap()
            .unwrap();
        db::previews::set_last_activity(
            &pool,
            idle.id,
            chrono::Utc::now() - chrono::Duration::hours(49),
        )
        .await
        .unwrap();
        let report =
            preview_service::sweep(&pool, adapter.as_ref(), &state.previews, chrono::Utc::now())
                .await
                .unwrap();
        assert!(report.expired >= 1, "{report:?}");
        assert!(db::previews::find_by_pr(&pool, project, 5)
            .await
            .unwrap()
            .is_none());
        let fresh = db::previews::find_by_pr(&pool, project, 6)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(fresh.status, "active");

        preview_service::close_all(&pool, adapter.as_ref(), &state.previews, project, "removed")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn settings_are_validated_and_the_comment_token_is_write_only() {
        let (state, _adapter, router, cookie, project, _secret, _slug) = setup().await;
        let uri = format!("/api/compose-projects/{project}/previews");
        for bad in [
            json!({ "enabled": true, "base_domain": "bad domain", "service_name": "web", "container_port": 3000 }),
            json!({ "enabled": true, "base_domain": "p.example.com", "service_name": "web;", "container_port": 3000 }),
            json!({ "enabled": true, "base_domain": "p.example.com", "service_name": "web", "container_port": 3000, "max_active": 0 }),
            json!({ "enabled": true, "base_domain": "p.example.com", "service_name": "web", "container_port": 3000, "ttl_hours": 0 }),
            json!({ "enabled": true, "base_domain": "p.example.com", "service_name": "web", "container_port": 3000, "resource_limits": { "memory_mb": 1, "cpu_millis": 500 } }),
            json!({ "enabled": true, "base_domain": "p.example.com", "service_name": "web", "container_port": 3000, "wildcard_certificate": "../../etc", "https": true }),
        ] {
            let (status, body) = call(&router, &cookie, Method::PUT, &uri, Some(bad.clone())).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{bad} -> {body}");
        }

        let token_uri = format!("{uri}/comment-token");
        let (status, _) = call(
            &router,
            &cookie,
            Method::PUT,
            &token_uri,
            Some(json!({ "token": "ghp_supersecret_value" })),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (_, overview) = call(&router, &cookie, Method::GET, &uri, None).await;
        assert_eq!(overview["settings"]["has_comment_token"], true);
        assert!(!overview.to_string().contains("ghp_supersecret_value"));
        let sealed = db::previews::comment_token(&state.db, project)
            .await
            .unwrap()
            .unwrap();
        assert!(!String::from_utf8_lossy(&sealed).contains("supersecret"));
        let (status, _) = call(&router, &cookie, Method::DELETE, &token_uri, None).await;
        assert_eq!(status, StatusCode::NO_CONTENT);

        // Raw Compose projects cannot have previews.
        let (raw, _) = test_support::compose_project(&state.db).await;
        let (status, overview) = call(
            &router,
            &cookie,
            Method::GET,
            &format!("/api/compose-projects/{raw}/previews"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(overview["available"], false);
    }

    #[tokio::test]
    async fn deployment_results_are_commented_on_the_pull_request() {
        let _guard = test_support::QUEUE_TESTS.lock().await;
        let (receiver, mut requests) = test_support::http_receiver().await;
        let base = receiver.trim_end_matches("/hook").to_string();
        let (mut state, adapter, _router, cookie, project, secret, slug) = setup().await;
        state.previews = preview_service::Previews::with_api_base(
            crate::services::secret_box::SecretBox::derive(&[42u8; 32]),
            test_support::github_apps(),
            &base,
        );
        let router = crate::routes::build_router(state.clone());
        let (status, _) = call(
            &router,
            &cookie,
            Method::PUT,
            &format!("/api/compose-projects/{project}/previews/comment-token"),
            Some(json!({ "token": "ghp_comment_token" })),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (_, opened) = deliver(&router, project, &secret, pr_event("opened", 4, 1)).await;
        let deployment: Uuid = opened["deployment"]["id"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        let preview = db::previews::find_by_pr(&state.db, project, 4)
            .await
            .unwrap()
            .unwrap();
        db::compose_projects::finish_deployment(
            &state.db,
            deployment,
            panel_core::types::DeploymentStatus::Succeeded,
            None,
            Some(SHA),
        )
        .await
        .unwrap();
        state.previews.deployment_finished(
            state.db.clone(),
            preview.preview_project_id,
            deployment,
        );
        let (head, body) = tokio::time::timeout(std::time::Duration::from_secs(5), requests.recv())
            .await
            .expect("a comment request")
            .unwrap();
        assert!(
            head.starts_with("post /repos/acme/shop/issues/4/comments"),
            "{head}"
        );
        assert!(
            head.contains("authorization: bearer ghp_comment_token"),
            "{head}"
        );
        assert!(body.contains(&format!("{slug}-pr-4.")), "{body}");
        assert!(body.contains("Preview deployed"), "{body}");

        preview_service::close_all(
            &state.db,
            adapter.as_ref(),
            &state.previews,
            project,
            "removed",
        )
        .await
        .unwrap();
    }
}
