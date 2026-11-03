//! Public Git webhook receiver. No session: every delivery is authenticated
//! by its signature (see `services::webhook`). Unknown projects, disabled
//! webhooks, and bad signatures all answer the same `404`, so the endpoint
//! does not reveal which project IDs exist.

use crate::db;
use crate::error::{ApiError, ApiResult};
use crate::services::webhook::{self, Delivery};
use crate::state::AppState;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use axum::{Json, Router};
use bytes::Bytes;
use serde_json::json;
use uuid::Uuid;

/// Push payloads with many commits can be large; anything bigger is refused
/// before it is buffered.
const MAX_WEBHOOK_BODY: usize = 5 * 1024 * 1024;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/compose/:project_id", post(compose_push))
        .layer(DefaultBodyLimit::max(MAX_WEBHOOK_BODY))
}

async fn compose_push(
    State(state): State<AppState>,
    Path(project_id): Path<Uuid>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<(StatusCode, Json<serde_json::Value>)> {
    let not_found = || ApiError::NotFound("webhook not found".into());
    let project = db::compose_projects::find_by_id(&state.db, project_id)
        .await?
        .ok_or_else(not_found)?;
    let nonce = db::compose_projects::webhook_nonce(&state.db, project_id)
        .await?
        .ok_or_else(not_found)?;
    let secret = state.webhook_key.secret_for(project_id, &nonce);
    if !webhook::authenticate(&headers, &body, &secret) {
        tracing::warn!(%project_id, "webhook delivery with an invalid signature");
        return Err(not_found());
    }

    let Some((_, branch)) = project.source.git_checkout() else {
        return Ok(ignored("project does not deploy from Git"));
    };
    let (pushed_branch, commit, changed_paths) = match webhook::interpret(&headers, &body)
        .map_err(|reason| ApiError::BadRequest(reason.into()))?
    {
        Delivery::Ping => return Ok((StatusCode::OK, Json(json!({ "pong": true })))),
        Delivery::Ignored(reason) => return Ok(ignored(reason)),
        Delivery::Push {
            branch,
            commit,
            changed_paths,
        } => (branch, commit, changed_paths),
    };
    if pushed_branch != branch {
        return Ok(ignored("push is not to the deployed branch"));
    }
    // A payload that does not list every changed file cannot prove the push
    // missed the watch paths, so it deploys.
    let watch_paths = db::compose_projects::webhook_watch_paths(&state.db, project_id).await?;
    if let Some(changed) = &changed_paths {
        if !watch_paths.is_empty() && !webhook::matches_watch_paths(&watch_paths, changed) {
            return Ok(ignored("no changed file matches the watch paths"));
        }
    }

    // Several pushes in quick succession need one deployment, not one each:
    // a queued job will fetch the branch head when it runs anyway.
    if let Some(queued) =
        db::compose_projects::find_queued_deployment(&state.db, project_id).await?
    {
        return Ok((
            StatusCode::ACCEPTED,
            Json(json!({ "coalesced": true, "deployment": queued })),
        ));
    }
    let deployment = state
        .deploy_queue
        .enqueue(&state.db, project_id, &project.slug, false, "webhook")
        .await?;
    db::audit::record(
        &state.db,
        None,
        "webhook",
        "deploy_compose_project",
        Some(&project.slug),
        json!({ "project_id": project_id, "deployment_id": deployment.id, "commit": commit }),
    )
    .await
    .ok();
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({ "coalesced": false, "deployment": deployment })),
    ))
}

fn ignored(reason: &str) -> (StatusCode, Json<serde_json::Value>) {
    (StatusCode::OK, Json(json!({ "ignored": reason })))
}

#[cfg(test)]
mod tests {
    use crate::db;
    use crate::test_support;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use hmac::{Hmac, Mac};
    use panel_core::adapters::mock::MockAdapter;
    use panel_core::types::ComposeSource;
    use serde_json::{json, Value};
    use sha2::Sha256;
    use std::sync::Arc;
    use tower::ServiceExt;
    use uuid::Uuid;

    async fn deliver(router: &axum::Router, project: Uuid, secret: &str, body: Value) -> Value {
        let body = body.to_string();
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(body.as_bytes());
        let signature = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));
        let request = Request::post(format!("/api/webhooks/compose/{project}"))
            .header("x-github-event", "push")
            .header("x-hub-signature-256", signature)
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap();
        let response = router.clone().oneshot(request).await.unwrap();
        assert!(response.status().is_success(), "{}", response.status());
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    fn push(files: &[&str]) -> Value {
        json!({
            "ref": "refs/heads/main",
            "after": "0123456789abcdef0123456789abcdef01234567",
            "commits": [{ "added": [], "removed": [], "modified": files }],
        })
    }

    #[tokio::test]
    async fn watch_paths_filter_pushes_and_unknown_file_lists_deploy() {
        let pool = test_support::test_pool().await;
        let state = test_support::app_state(pool.clone(), Arc::new(MockAdapter::new()));
        let owner = test_support::user(&pool, true).await;
        let project = db::compose_projects::create(
            &pool,
            Uuid::new_v4(),
            "Webhook test",
            &format!("p{}", &Uuid::new_v4().simple().to_string()[..12]),
            owner.id,
            &ComposeSource::Git {
                repository_url: "https://example.com/app.git".into(),
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
        db::compose_projects::set_webhook_watch_paths(
            &pool,
            project.id,
            &["services/api".into(), "**/compose.yml".into()],
        )
        .await
        .unwrap();
        let secret = state.webhook_key.secret_for(project.id, &nonce);
        let router = crate::routes::build_router(state);

        let skipped = deliver(&router, project.id, &secret, push(&["docs/readme.md"])).await;
        assert_eq!(
            skipped["ignored"],
            "no changed file matches the watch paths"
        );

        let queued = deliver(
            &router,
            project.id,
            &secret,
            push(&["services/api/main.rs"]),
        )
        .await;
        assert_eq!(queued["coalesced"], false, "{queued}");

        // Without file lists the push cannot be ruled out; it coalesces into
        // the job queued above.
        let unknown = deliver(
            &router,
            project.id,
            &secret,
            json!({ "ref": "refs/heads/main", "after": "ab12", "commits": [] }),
        )
        .await;
        assert_eq!(unknown["coalesced"], true, "{unknown}");

        // Watch-path matching happens only after authentication.
        let forged = router
            .clone()
            .oneshot(
                Request::post(format!("/api/webhooks/compose/{}", project.id))
                    .header("x-github-event", "push")
                    .header("x-hub-signature-256", "sha256=00")
                    .body(Body::from(push(&["services/api/x"]).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(forged.status(), StatusCode::NOT_FOUND);
    }
}
