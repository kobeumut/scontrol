//! D4 observability endpoints: Nginx request statistics and Docker cleanup.
//!
//! Statistics are readable by any signed-in user (they hold no secrets:
//! host names, counts, status classes, latency, bytes). Cleanup settings
//! and runs are admin-only.

use crate::auth::{AdminUser, AuthUser};
use crate::db;
use crate::db::observability::{
    CleanupRun, CleanupSettings, CursorStatus, HostTotals, SeriesPoint,
};
use crate::error::{ApiError, ApiResult};
use crate::services::observability as service;
use crate::state::AppState;
use axum::extract::{Query, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{Duration, Utc};
use panel_core::observability::{DEFAULT_PRUNE_AGE_HOURS, MAX_PRUNE_AGE_HOURS};
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/requests", get(requests))
        .route("/docker-cleanup", get(cleanup_overview))
        .route(
            "/docker-cleanup/settings",
            axum::routing::put(save_settings),
        )
        .route("/docker-cleanup/run", post(run_now))
}

#[derive(Deserialize)]
struct RangeQuery {
    /// Look-back window, 1 to 720 hours (default 24).
    hours: Option<u32>,
    /// Restrict the chart to one host.
    host: Option<String>,
}

#[derive(Serialize)]
struct RequestStats {
    hours: u32,
    step_minutes: i32,
    hosts: Vec<HostTotals>,
    series: Vec<SeriesPoint>,
    /// How reading each server's access log went.
    sources: Vec<CursorStatus>,
}

/// Chart resolution: about 240 points whatever the window.
fn step_minutes(hours: u32) -> i32 {
    let wanted = (hours * 60).div_ceil(240) as i32;
    [1, 5, 15, 30, 60, 180, 360]
        .into_iter()
        .find(|s| *s >= wanted)
        .unwrap_or(360)
}

async fn requests(
    AuthUser(_): AuthUser,
    State(state): State<AppState>,
    Query(q): Query<RangeQuery>,
) -> ApiResult<Json<RequestStats>> {
    let hours = q.hours.unwrap_or(24);
    if !(1..=24 * 30).contains(&hours) {
        return Err(ApiError::BadRequest("hours must be 1 to 720".into()));
    }
    let host = q.host.as_deref().map(str::trim).filter(|h| !h.is_empty());
    if let Some(h) = host {
        if panel_core::observability::normalize_host(h)
            != h.to_ascii_lowercase().trim_end_matches('.')
        {
            return Err(ApiError::BadRequest("invalid host".into()));
        }
    }
    let since = Utc::now() - Duration::hours(i64::from(hours));
    let step = step_minutes(hours);
    let host = host.map(|h| h.to_ascii_lowercase());
    Ok(Json(RequestStats {
        hours,
        step_minutes: step,
        hosts: db::observability::host_totals(&state.db, since).await?,
        series: db::observability::series(&state.db, since, step, host.as_deref()).await?,
        sources: db::observability::cursor_statuses(&state.db).await?,
    }))
}

#[derive(Serialize)]
struct CleanupOverview {
    settings: CleanupSettings,
    runs: Vec<CleanupRun>,
    /// Allowed automatic intervals, in hours.
    intervals: &'static [i32],
}

const INTERVALS: &[i32] = &[6, 12, 24, 72, 168];

async fn cleanup_overview(
    AdminUser(_): AdminUser,
    State(state): State<AppState>,
) -> ApiResult<Json<CleanupOverview>> {
    Ok(Json(CleanupOverview {
        settings: db::observability::cleanup_settings(&state.db).await?,
        runs: db::observability::recent_runs(&state.db, 20).await?,
        intervals: INTERVALS,
    }))
}

fn check_age(hours: i32) -> ApiResult<u32> {
    if !(1..=MAX_PRUNE_AGE_HOURS as i32).contains(&hours) {
        return Err(ApiError::BadRequest(format!(
            "older_than_hours must be 1 to {MAX_PRUNE_AGE_HOURS}"
        )));
    }
    Ok(hours as u32)
}

async fn save_settings(
    AdminUser(user): AdminUser,
    State(state): State<AppState>,
    Json(body): Json<CleanupSettings>,
) -> ApiResult<Json<CleanupSettings>> {
    check_age(body.older_than_hours)?;
    if !INTERVALS.contains(&body.interval_hours) {
        return Err(ApiError::BadRequest(
            "interval_hours must be 6, 12, 24, 72 or 168".into(),
        ));
    }
    let saved = db::observability::save_cleanup_settings(&state.db, &body).await?;
    db::audit::record(
        &state.db,
        Some(user.id),
        "user",
        "update_docker_cleanup_settings",
        None,
        json!({ "enabled": saved.enabled, "interval_hours": saved.interval_hours,
                "older_than_hours": saved.older_than_hours }),
    )
    .await
    .ok();
    Ok(Json(saved))
}

#[derive(Deserialize, Default)]
struct RunBody {
    older_than_hours: Option<i32>,
    /// Server to clean; this host when omitted.
    #[serde(default)]
    server_id: Option<Uuid>,
}

/// One-click cleanup. Only the age can be chosen; the commands are fixed.
async fn run_now(
    AdminUser(user): AdminUser,
    State(state): State<AppState>,
    body: Option<Json<RunBody>>,
) -> ApiResult<Json<CleanupRun>> {
    let body = body.map(|Json(b)| b).unwrap_or_default();
    let age = match body.older_than_hours {
        Some(h) => check_age(h)?,
        None => db::observability::cleanup_settings(&state.db)
            .await
            .map(|s| s.older_than_hours as u32)
            .unwrap_or(DEFAULT_PRUNE_AGE_HOURS),
    };
    let server = body.server_id.unwrap_or(db::servers::LOCAL_SERVER_ID);
    let agent = state.agents.for_server_id(&state.db, server).await?;
    let run = service::run_cleanup(&state.db, agent, server, "manual", age).await?;
    db::audit::record(
        &state.db,
        Some(user.id),
        "user",
        "docker_cleanup",
        None,
        json!({ "run_id": run.id, "status": run.status, "older_than_hours": age,
                "images_freed_bytes": run.images_freed_bytes,
                "builder_freed_bytes": run.builder_freed_bytes }),
    )
    .await
    .ok();
    Ok(Json(run))
}

#[cfg(test)]
mod tests {
    use super::step_minutes;
    use crate::test_support;
    use axum::body::Body;
    use axum::http::{Method, Request, StatusCode};
    use panel_core::adapters::mock::MockAdapter;
    use panel_core::adapters::DockerPruneOutcome;
    use serde_json::{json, Value};
    use std::sync::Arc;
    use tower::ServiceExt;

    #[test]
    fn chart_resolution_scales_with_the_window() {
        assert_eq!(step_minutes(1), 1);
        assert_eq!(step_minutes(24), 15);
        assert_eq!(step_minutes(168), 60);
        assert_eq!(step_minutes(720), 180);
    }

    async fn call(
        router: &axum::Router,
        cookie: &str,
        method: Method,
        uri: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut req = Request::builder()
            .method(method)
            .uri(uri)
            .header("cookie", cookie);
        let body = match body {
            Some(b) => {
                req = req.header("content-type", "application/json");
                Body::from(b.to_string())
            }
            None => Body::empty(),
        };
        let res = router
            .clone()
            .oneshot(req.body(body).unwrap())
            .await
            .unwrap();
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    #[tokio::test]
    async fn cleanup_is_admin_only_validated_and_reports_freed_space() {
        let _serial = test_support::QUEUE_TESTS.lock().await;
        let pool = test_support::test_pool().await;
        let adapter = Arc::new(MockAdapter::new());
        *adapter.prune_outcome.lock().unwrap() = Some(DockerPruneOutcome {
            images_freed_bytes: 5_000,
            builder_freed_bytes: 700,
        });
        let state = test_support::app_state(pool.clone(), adapter.clone());
        let admin = test_support::user(&pool, true).await;
        let plain = test_support::user(&pool, false).await;
        let admin_cookie = test_support::session_cookie(&state, admin.id).await;
        let plain_cookie = test_support::session_cookie(&state, plain.id).await;
        let router = crate::routes::build_router(state);

        let (status, _) = call(
            &router,
            &plain_cookie,
            Method::POST,
            "/api/observability/docker-cleanup/run",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, _) = call(
            &router,
            &plain_cookie,
            Method::GET,
            "/api/observability/docker-cleanup",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);

        let (status, _) = call(
            &router,
            &admin_cookie,
            Method::POST,
            "/api/observability/docker-cleanup/run",
            Some(json!({ "older_than_hours": 0 })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _) = call(
            &router,
            &admin_cookie,
            Method::PUT,
            "/api/observability/docker-cleanup/settings",
            Some(json!({ "enabled": true, "interval_hours": 5, "older_than_hours": 168 })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        let (status, saved) = call(
            &router,
            &admin_cookie,
            Method::PUT,
            "/api/observability/docker-cleanup/settings",
            Some(json!({ "enabled": true, "interval_hours": 12, "older_than_hours": 72 })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{saved}");
        assert_eq!(saved["interval_hours"], 12);

        // A queued deployment on this host holds the cleanup back.
        let (service, _) = test_support::compose_project(&pool).await;
        crate::db::compose_projects::enqueue_deployment(
            &pool,
            uuid::Uuid::new_v4(),
            service,
            "agent://t",
            false,
            "manual",
            None,
        )
        .await
        .unwrap();
        let (status, body) = call(
            &router,
            &admin_cookie,
            Method::POST,
            "/api/observability/docker-cleanup/run",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert!(adapter.prunes.lock().unwrap().is_empty());
        // Ours and leftovers of other tests sharing the database.
        sqlx::query(
            "UPDATE compose_deployments SET status = 'failed', finished_at = now()
             WHERE status IN ('queued', 'running')",
        )
        .execute(&pool)
        .await
        .unwrap();
        crate::db::compose_projects::delete(&pool, service)
            .await
            .unwrap();

        let (status, run) = call(
            &router,
            &admin_cookie,
            Method::POST,
            "/api/observability/docker-cleanup/run",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{run}");
        assert_eq!(run["status"], "succeeded");
        assert_eq!(run["images_freed_bytes"], 5000);
        assert_eq!(run["older_than_hours"], 72);

        // Restore the shared singleton row.
        sqlx::query("UPDATE docker_cleanup_settings SET enabled = FALSE, interval_hours = 24, older_than_hours = 168")
            .execute(&pool)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn request_stats_need_a_session_and_validate_input() {
        let _serial = test_support::QUEUE_TESTS.lock().await;
        let pool = test_support::test_pool().await;
        let state = test_support::app_state(pool.clone(), Arc::new(MockAdapter::new()));
        let plain = test_support::user(&pool, false).await;
        let cookie = test_support::session_cookie(&state, plain.id).await;
        let router = crate::routes::build_router(state);

        let (status, _) = call(
            &router,
            "",
            Method::GET,
            "/api/observability/requests",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let (status, body) = call(
            &router,
            &cookie,
            Method::GET,
            "/api/observability/requests?hours=6",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body["hosts"].is_array() && body["series"].is_array());
        let (status, _) = call(
            &router,
            &cookie,
            Method::GET,
            "/api/observability/requests?hours=0",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _) = call(
            &router,
            &cookie,
            Method::GET,
            "/api/observability/requests?host=a%20b",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }
}
