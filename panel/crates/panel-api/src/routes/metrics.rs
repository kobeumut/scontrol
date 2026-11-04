//! Server and container metrics (`/api/metrics`).
//!
//! - `GET /current`: the latest agent snapshot, containers mapped to panel
//!   projects (other Compose projects on the host are left out).
//! - `GET /server?range=`: host history.
//! - `GET /projects/:id?range=`: per-service history of a Compose project.
//! - `GET /alerts`, `PUT /alerts/settings`: thresholds and firing alerts.
//!
//! The range picks the stored resolution: up to 24 h reads 30 s buckets,
//! 7 d reads 5 min averages, 30 d reads 1 h averages.

use crate::auth::AdminUser;
use crate::db;
use crate::db::metrics::{AlertSettings, AlertStateRow, ContainerPoint, ServerPoint};
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;
use axum::extract::{Path, Query, State};
use axum::routing::{get, put};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use panel_core::metrics::{
    ContainerMetrics, HostMetrics, COARSE_RESOLUTION_SECS, MEDIUM_RESOLUTION_SECS,
    RAW_RESOLUTION_SECS,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use uuid::Uuid;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/current", get(current))
        .route("/server", get(server))
        .route("/projects/:id", get(project))
        .route("/alerts", get(alerts))
        .route("/alerts/settings", put(update_settings))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
pub enum Range {
    #[serde(rename = "1h")]
    #[default]
    Hour,
    #[serde(rename = "6h")]
    SixHours,
    #[serde(rename = "24h")]
    Day,
    #[serde(rename = "7d")]
    Week,
    #[serde(rename = "30d")]
    Month,
}

impl Range {
    /// `(window, resolution)`.
    pub fn window(self) -> (chrono::Duration, i32) {
        match self {
            Range::Hour => (chrono::Duration::hours(1), RAW_RESOLUTION_SECS),
            Range::SixHours => (chrono::Duration::hours(6), RAW_RESOLUTION_SECS),
            Range::Day => (chrono::Duration::hours(24), RAW_RESOLUTION_SECS),
            Range::Week => (chrono::Duration::days(7), MEDIUM_RESOLUTION_SECS),
            Range::Month => (chrono::Duration::days(30), COARSE_RESOLUTION_SECS),
        }
    }
}

#[derive(Deserialize)]
struct RangeQuery {
    #[serde(default)]
    range: Range,
}

#[derive(Serialize)]
struct ContainerView {
    compose_project_id: Uuid,
    project_name: String,
    #[serde(flatten)]
    metrics: ContainerMetrics,
}

#[derive(Serialize)]
struct CurrentView {
    sampled_at: Option<DateTime<Utc>>,
    interval_secs: Option<f64>,
    sample_micros: Option<u64>,
    host: Option<HostMetrics>,
    containers: Vec<ContainerView>,
}

async fn current(
    AdminUser(_): AdminUser,
    State(state): State<AppState>,
) -> ApiResult<Json<CurrentView>> {
    let Some(snapshot) = state.metrics.latest() else {
        return Ok(Json(CurrentView {
            sampled_at: None,
            interval_secs: None,
            sample_micros: None,
            host: None,
            containers: Vec::new(),
        }));
    };
    let projects: HashMap<String, (Uuid, String)> = db::metrics::project_slugs(&state.db)
        .await?
        .into_iter()
        .map(|(id, slug, name)| (slug, (id, name)))
        .collect();
    let containers = snapshot
        .containers
        .into_iter()
        .filter_map(|metrics| {
            let (id, name) = projects.get(&metrics.project)?;
            Some(ContainerView {
                compose_project_id: *id,
                project_name: name.clone(),
                metrics,
            })
        })
        .collect();
    Ok(Json(CurrentView {
        sampled_at: Some(snapshot.sampled_at),
        interval_secs: Some(snapshot.interval_secs),
        sample_micros: Some(snapshot.sample_micros),
        host: Some(snapshot.host),
        containers,
    }))
}

#[derive(Serialize)]
struct ServerSeries {
    range: Range,
    resolution_secs: i32,
    points: Vec<ServerPoint>,
}

async fn server(
    AdminUser(_): AdminUser,
    State(state): State<AppState>,
    Query(query): Query<RangeQuery>,
) -> ApiResult<Json<ServerSeries>> {
    let (window, resolution) = query.range.window();
    let points = db::metrics::server_series(&state.db, resolution, Utc::now() - window).await?;
    Ok(Json(ServerSeries {
        range: query.range,
        resolution_secs: resolution,
        points,
    }))
}

#[derive(Serialize)]
struct ServiceSeries {
    service: String,
    points: Vec<ContainerPoint>,
}

#[derive(Serialize)]
struct ProjectSeries {
    range: Range,
    resolution_secs: i32,
    services: Vec<ServiceSeries>,
}

async fn project(
    AdminUser(_): AdminUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Query(query): Query<RangeQuery>,
) -> ApiResult<Json<ProjectSeries>> {
    if db::compose_projects::find_by_id(&state.db, id)
        .await?
        .is_none()
    {
        return Err(ApiError::NotFound("compose project".into()));
    }
    let (window, resolution) = query.range.window();
    let rows =
        db::metrics::container_series(&state.db, id, resolution, Utc::now() - window).await?;
    let mut services: BTreeMap<String, Vec<ContainerPoint>> = BTreeMap::new();
    for row in rows {
        services.entry(row.service.clone()).or_default().push(row);
    }
    Ok(Json(ProjectSeries {
        range: query.range,
        resolution_secs: resolution,
        services: services
            .into_iter()
            .map(|(service, points)| ServiceSeries { service, points })
            .collect(),
    }))
}

#[derive(Serialize)]
struct AlertsView {
    settings: AlertSettings,
    firing: Vec<AlertStateRow>,
}

async fn alerts(
    AdminUser(_): AdminUser,
    State(state): State<AppState>,
) -> ApiResult<Json<AlertsView>> {
    Ok(Json(AlertsView {
        settings: db::metrics::alert_settings(&state.db).await?,
        firing: db::metrics::firing_alerts(&state.db).await?,
    }))
}

async fn update_settings(
    AdminUser(user): AdminUser,
    State(state): State<AppState>,
    Json(body): Json<AlertSettings>,
) -> ApiResult<Json<AlertSettings>> {
    body.validate().map_err(ApiError::BadRequest)?;
    let saved = db::metrics::update_alert_settings(&state.db, &body).await?;
    db::audit::record(
        &state.db,
        Some(user.id),
        "user",
        "metrics.alerts.update",
        Some("metric_alert_settings"),
        serde_json::to_value(&saved).unwrap_or_default(),
    )
    .await
    .ok();
    Ok(Json(saved))
}

#[cfg(test)]
mod tests {
    use crate::test_support;
    use axum::body::Body;
    use axum::http::{Method, Request, StatusCode};
    use chrono::Utc;
    use panel_core::adapters::mock::MockAdapter;
    use serde_json::{json, Value};
    use std::sync::Arc;
    use tower::ServiceExt;

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
        let body = match body {
            Some(body) => {
                request = request.header("content-type", "application/json");
                Body::from(body.to_string())
            }
            None => Body::empty(),
        };
        let response = router
            .clone()
            .oneshot(request.body(body).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    #[tokio::test]
    async fn metrics_endpoints() {
        let _serial = test_support::METRICS_TESTS.lock().await;
        let pool = test_support::test_pool().await;
        test_support::reset_metrics(&pool).await;
        let adapter = Arc::new(MockAdapter::new());
        let state = test_support::app_state(pool.clone(), adapter.clone());
        let admin = test_support::user(&pool, true).await;
        let cookie = test_support::session_cookie(&state, admin.id).await;
        let plain = test_support::user(&pool, false).await;
        let plain_cookie = test_support::session_cookie(&state, plain.id).await;
        let (project_id, slug) = test_support::compose_project(&pool).await;
        let collector = state.metrics.clone();
        let notifier = state.notifier.clone();
        let router = crate::routes::build_router(state);

        // Before the first collection there is nothing to show.
        let (status, body) =
            call(&router, &cookie, Method::GET, "/api/metrics/current", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["host"], Value::Null);
        let (status, _) = call(
            &router,
            &plain_cookie,
            Method::GET,
            "/api/metrics/current",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);

        let mut snapshot = crate::services::metrics::tests::snapshot(Utc::now(), 12.5);
        snapshot.containers = vec![
            panel_core::metrics::ContainerMetrics {
                container_id: "a".repeat(64),
                name: format!("{slug}-web-1"),
                project: slug.clone(),
                service: "web".into(),
                cpu_pct: 3.0,
                mem_bytes: 1024,
                mem_limit_bytes: None,
                io_read_bps: 0.0,
                io_write_bps: 0.0,
            },
            panel_core::metrics::ContainerMetrics {
                container_id: "b".repeat(64),
                name: "foreign-web-1".into(),
                project: "not-a-panel-project".into(),
                service: "web".into(),
                cpu_pct: 3.0,
                mem_bytes: 1024,
                mem_limit_bytes: None,
                io_read_bps: 0.0,
                io_write_bps: 0.0,
            },
        ];
        *adapter.metrics.lock().unwrap() = Some(snapshot);
        let collected =
            crate::services::metrics::collect_once(&pool, adapter.as_ref(), &notifier, None)
                .await
                .unwrap()
                .unwrap();
        collector.set_latest(collected);

        let (_, body) = call(&router, &cookie, Method::GET, "/api/metrics/current", None).await;
        assert_eq!(body["host"]["cpu_pct"], json!(12.5));
        assert_eq!(body["containers"].as_array().unwrap().len(), 1);
        assert_eq!(
            body["containers"][0]["compose_project_id"],
            json!(project_id)
        );
        assert_eq!(body["containers"][0]["service"], "web");

        let (status, body) = call(
            &router,
            &cookie,
            Method::GET,
            "/api/metrics/server?range=1h",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["resolution_secs"], 30);
        assert_eq!(body["points"].as_array().unwrap().len(), 1);
        let (_, body) = call(
            &router,
            &cookie,
            Method::GET,
            "/api/metrics/server?range=7d",
            None,
        )
        .await;
        assert_eq!(body["resolution_secs"], 300);
        let (status, _) = call(
            &router,
            &cookie,
            Method::GET,
            "/api/metrics/server?range=2y",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        let (status, body) = call(
            &router,
            &cookie,
            Method::GET,
            &format!("/api/metrics/projects/{project_id}?range=24h"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["services"][0]["service"], "web");
        assert_eq!(body["services"][0]["points"].as_array().unwrap().len(), 1);
        let (status, _) = call(
            &router,
            &cookie,
            Method::GET,
            &format!("/api/metrics/projects/{}", uuid::Uuid::new_v4()),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        // Alert settings: validated, saved, and returned.
        let (_, body) = call(&router, &cookie, Method::GET, "/api/metrics/alerts", None).await;
        assert_eq!(body["settings"]["cpu_pct"], json!(85.0));
        assert_eq!(body["firing"], json!([]));
        let mut settings = body["settings"].clone();
        settings["cpu_pct"] = json!(150);
        let (status, _) = call(
            &router,
            &cookie,
            Method::PUT,
            "/api/metrics/alerts/settings",
            Some(settings.clone()),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        settings["cpu_pct"] = json!(70);
        settings["trigger_samples"] = json!(5);
        let (status, body) = call(
            &router,
            &cookie,
            Method::PUT,
            "/api/metrics/alerts/settings",
            Some(settings),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["cpu_pct"], json!(70.0));
        assert_eq!(body["trigger_samples"], 5);
        test_support::reset_metrics(&pool).await;
    }
}
