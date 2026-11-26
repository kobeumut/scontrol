//! Scheduled backups (`/api/backups/schedules`), run by the panel-api
//! scheduler in `services/backup_schedules.rs`.
//!
//! A schedule targets a managed database service (the panel already holds
//! its sealed password, so none is sent or stored here) or a named volume
//! of a Compose service. Like destinations, schedules are server-wide
//! settings and need `system:admin`. Responses carry no secret: no S3 key,
//! no database password, only names and times.

use crate::auth::AdminUser;
use crate::db;
use crate::db::backup_schedules::{ScheduleFields, ScheduleRow};
use crate::error::{ApiError, ApiResult};
use crate::services::backup_schedules::{self as scheduler, BackupContext};
use crate::state::AppState;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use panel_core::schedule::{next_cron_run, CronSpec};
use panel_core::types::{validate, ComposeProject, ComposeSource};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use uuid::Uuid;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(list).post(create))
        .route("/preview", post(preview))
        .route("/:id", get(show).put(update).delete(remove))
        .route("/:id/run", post(run_now))
}

fn context(state: &AppState) -> BackupContext {
    BackupContext {
        db: state.db.clone(),
        agents: state.agents.clone(),
        notifier: state.notifier.clone(),
        secret_box: state.secret_box.clone(),
    }
}

#[derive(Serialize)]
struct ScheduleView {
    id: Uuid,
    name: String,
    target_kind: String,
    compose_project_id: Uuid,
    /// Slug and name of the service, `None` if it was removed meanwhile.
    project_slug: Option<String>,
    project_name: Option<String>,
    /// The server the service (and so the backup) runs on.
    server_id: Option<Uuid>,
    /// Database schedules: the service's engine and database.
    database_engine: Option<String>,
    database_name: Option<String>,
    volume_name: Option<String>,
    service_name: Option<String>,
    stop_service: bool,
    s3_destination_id: Uuid,
    destination_name: Option<String>,
    cron_expression: String,
    timezone: String,
    retention_count: i32,
    enabled: bool,
    next_run_at: Option<DateTime<Utc>>,
    last_run_at: Option<DateTime<Utc>>,
    last_run_id: Option<Uuid>,
    last_run_status: Option<String>,
    last_run_error: Option<String>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

async fn view(
    state: &AppState,
    row: ScheduleRow,
    last: Option<(String, Option<String>)>,
    destinations: &HashMap<Uuid, String>,
) -> ApiResult<ScheduleView> {
    let project = db::compose_projects::find_by_id(&state.db, row.compose_project_id).await?;
    let (database_engine, database_name) = match project.as_ref().map(|p| &p.source) {
        Some(ComposeSource::Database(spec)) => (
            Some(spec.engine.as_str().to_string()),
            Some(spec.database_name.clone()),
        ),
        _ => (None, None),
    };
    let (last_run_status, last_run_error) = last.unzip();
    Ok(ScheduleView {
        id: row.id,
        name: row.name,
        target_kind: row.target_kind,
        compose_project_id: row.compose_project_id,
        project_slug: project.as_ref().map(|p| p.slug.clone()),
        project_name: project.as_ref().map(|p| p.name.clone()),
        server_id: project.as_ref().map(|p| p.server_id),
        database_engine,
        database_name,
        volume_name: row.volume_name,
        service_name: row.service_name,
        stop_service: row.stop_service,
        s3_destination_id: row.s3_destination_id,
        destination_name: destinations.get(&row.s3_destination_id).cloned(),
        cron_expression: row.cron_expression,
        timezone: row.timezone,
        retention_count: row.retention_count,
        enabled: row.enabled,
        next_run_at: row.next_run_at,
        last_run_at: row.last_run_at,
        last_run_id: row.last_run_id,
        last_run_status,
        last_run_error: last_run_error.flatten(),
        created_at: row.created_at,
        updated_at: row.updated_at,
    })
}

async fn destination_names(state: &AppState) -> ApiResult<HashMap<Uuid, String>> {
    Ok(db::backups::list_destinations(&state.db)
        .await?
        .into_iter()
        .map(|d| (d.id, d.name))
        .collect())
}

async fn view_one(state: &AppState, row: ScheduleRow) -> ApiResult<ScheduleView> {
    let last = match row.last_run_id {
        Some(id) => db::backups::find_run(&state.db, id)
            .await?
            .map(|r| (r.status, r.error)),
        None => None,
    };
    view(state, row, last, &destination_names(state).await?).await
}

async fn list(
    AdminUser(_): AdminUser,
    State(state): State<AppState>,
) -> ApiResult<Json<Vec<ScheduleView>>> {
    let rows = db::backup_schedules::list(&state.db).await?;
    let mut last: HashMap<Uuid, (String, Option<String>)> =
        db::backup_schedules::last_run_statuses(&state.db)
            .await?
            .into_iter()
            .map(|(id, status, error)| (id, (status, error)))
            .collect();
    let destinations = destination_names(&state).await?;
    let mut views = Vec::with_capacity(rows.len());
    for row in rows {
        let l = row.last_run_id.and_then(|id| last.remove(&id));
        views.push(view(&state, row, l, &destinations).await?);
    }
    Ok(Json(views))
}

async fn show(
    AdminUser(_): AdminUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<ScheduleView>> {
    Ok(Json(view_one(&state, find(&state, id).await?).await?))
}

fn default_timezone() -> String {
    "UTC".into()
}
fn default_retention() -> i32 {
    7
}
fn default_true() -> bool {
    true
}

#[derive(Deserialize)]
struct CreateBody {
    name: String,
    /// `database` or `volume`.
    target_kind: String,
    compose_project_id: Uuid,
    #[serde(default)]
    volume_name: Option<String>,
    #[serde(default)]
    service_name: Option<String>,
    #[serde(default)]
    stop_service: bool,
    s3_destination_id: Uuid,
    cron_expression: String,
    #[serde(default = "default_timezone")]
    timezone: String,
    #[serde(default = "default_retention")]
    retention_count: i32,
    #[serde(default = "default_true")]
    enabled: bool,
}

/// Every field is optional; omitted ones keep their value. The target
/// (kind, service, volume) cannot change.
#[derive(Deserialize)]
struct UpdateBody {
    name: Option<String>,
    /// `null` clears the service of a volume schedule.
    #[serde(default, with = "double_option")]
    service_name: Option<Option<String>>,
    stop_service: Option<bool>,
    s3_destination_id: Option<Uuid>,
    cron_expression: Option<String>,
    timezone: Option<String>,
    retention_count: Option<i32>,
    enabled: Option<bool>,
}

/// Distinguishes an absent field (`None`) from an explicit `null`
/// (`Some(None)`).
mod double_option {
    use serde::{Deserialize, Deserializer};
    pub fn deserialize<'de, D: Deserializer<'de>>(
        d: D,
    ) -> Result<Option<Option<String>>, D::Error> {
        Option::<String>::deserialize(d).map(Some)
    }
}

/// Values every schedule must satisfy, created or updated.
struct Common {
    name: String,
    cron_expression: String,
    timezone: String,
    next_run_at: Option<DateTime<Utc>>,
}

fn check_common(
    name: &str,
    cron_expression: &str,
    timezone: &str,
    retention_count: i32,
    enabled: bool,
) -> ApiResult<Common> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 100 || name.chars().any(char::is_control) {
        return Err(ApiError::BadRequest("name must be 1–100 characters".into()));
    }
    let cron_expression = cron_expression.trim();
    let timezone = timezone.trim();
    let next =
        next_cron_run(cron_expression, timezone, Utc::now()).map_err(ApiError::BadRequest)?;
    if !(0..=1000).contains(&retention_count) {
        return Err(ApiError::BadRequest(
            "retention_count must be 0 (keep all) to 1000".into(),
        ));
    }
    Ok(Common {
        name: name.to_string(),
        cron_expression: cron_expression.to_string(),
        timezone: timezone.to_string(),
        next_run_at: enabled.then_some(next),
    })
}

fn check_volume_service(service: Option<&str>, stop_service: bool) -> ApiResult<()> {
    if let Some(service) = service {
        if !validate::is_valid_service_name(service) {
            return Err(ApiError::BadRequest("invalid service name".into()));
        }
    }
    if stop_service && service.is_none() {
        return Err(ApiError::BadRequest(
            "stopping the container needs a service name".into(),
        ));
    }
    Ok(())
}

async fn require_destination(state: &AppState, id: Uuid) -> ApiResult<()> {
    db::backups::find_destination(&state.db, id)
        .await?
        .map(|_| ())
        .ok_or_else(|| ApiError::BadRequest("no such S3 destination".into()))
}

/// Checks that the service can be backed up the way the schedule asks.
async fn check_target(state: &AppState, kind: &str, project: &ComposeProject) -> ApiResult<()> {
    match (kind, &project.source) {
        ("database", ComposeSource::Database(spec)) => {
            scheduler::backup_engine(spec.engine).map_err(ApiError::BadRequest)?;
            if db::database_services::find(&state.db, project.id).await?.is_none() {
                return Err(ApiError::BadRequest(
                    "the database service settings are missing".into(),
                ));
            }
            Ok(())
        }
        ("database", _) => Err(ApiError::BadRequest(
            "database schedules need a managed database service; back up other databases by their volume".into(),
        )),
        ("volume", ComposeSource::Static(_)) => Err(ApiError::BadRequest(
            "static sites have no volumes".into(),
        )),
        ("volume", _) => Ok(()),
        _ => Err(ApiError::BadRequest(
            "target_kind must be `database` or `volume`".into(),
        )),
    }
}

fn name_taken(error: sqlx::Error) -> ApiError {
    match &error {
        sqlx::Error::Database(db) if db.is_unique_violation() => {
            ApiError::Conflict("this service already has a backup schedule with that name".into())
        }
        _ => error.into(),
    }
}

async fn create(
    AdminUser(user): AdminUser,
    State(state): State<AppState>,
    Json(body): Json<CreateBody>,
) -> ApiResult<(StatusCode, Json<ScheduleView>)> {
    let common = check_common(
        &body.name,
        &body.cron_expression,
        &body.timezone,
        body.retention_count,
        body.enabled,
    )?;
    let (volume_name, service_name, stop_service) = match body.target_kind.as_str() {
        "volume" => {
            let volume = body.volume_name.as_deref().map(str::trim).unwrap_or("");
            if !validate::is_valid_volume_name(volume) {
                return Err(ApiError::BadRequest("invalid volume name".into()));
            }
            let service = body
                .service_name
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty());
            check_volume_service(service, body.stop_service)?;
            (Some(volume), service, body.stop_service)
        }
        "database" => {
            if body.volume_name.is_some() || body.service_name.is_some() || body.stop_service {
                return Err(ApiError::BadRequest(
                    "a database schedule takes no volume or service name".into(),
                ));
            }
            (None, None, false)
        }
        _ => {
            return Err(ApiError::BadRequest(
                "target_kind must be `database` or `volume`".into(),
            ))
        }
    };
    let project = db::compose_projects::find_by_id(&state.db, body.compose_project_id)
        .await?
        .ok_or_else(|| ApiError::BadRequest("no such service".into()))?;
    check_target(&state, &body.target_kind, &project).await?;
    require_destination(&state, body.s3_destination_id).await?;
    let row = db::backup_schedules::create(
        &state.db,
        &ScheduleFields {
            name: &common.name,
            target_kind: &body.target_kind,
            compose_project_id: project.id,
            volume_name,
            service_name,
            stop_service,
            s3_destination_id: body.s3_destination_id,
            cron_expression: &common.cron_expression,
            timezone: &common.timezone,
            retention_count: body.retention_count,
            enabled: body.enabled,
            next_run_at: common.next_run_at,
        },
    )
    .await
    .map_err(name_taken)?;
    audit(&state, user.id, "create_backup_schedule", &row).await;
    Ok((StatusCode::CREATED, Json(view_one(&state, row).await?)))
}

async fn update(
    AdminUser(user): AdminUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(body): Json<UpdateBody>,
) -> ApiResult<Json<ScheduleView>> {
    let existing = find(&state, id).await?;
    let enabled = body.enabled.unwrap_or(existing.enabled);
    let retention_count = body.retention_count.unwrap_or(existing.retention_count);
    let common = check_common(
        body.name.as_deref().unwrap_or(&existing.name),
        body.cron_expression
            .as_deref()
            .unwrap_or(&existing.cron_expression),
        body.timezone.as_deref().unwrap_or(&existing.timezone),
        retention_count,
        enabled,
    )?;
    let service_name = match body.service_name {
        Some(service) => service
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
        None => existing.service_name.clone(),
    };
    let stop_service = body.stop_service.unwrap_or(existing.stop_service);
    if existing.target_kind == "volume" {
        check_volume_service(service_name.as_deref(), stop_service)?;
    } else if service_name.is_some() || stop_service {
        return Err(ApiError::BadRequest(
            "a database schedule takes no service name".into(),
        ));
    }
    let destination = body.s3_destination_id.unwrap_or(existing.s3_destination_id);
    require_destination(&state, destination).await?;
    let row = db::backup_schedules::update(
        &state.db,
        id,
        &ScheduleFields {
            name: &common.name,
            target_kind: &existing.target_kind,
            compose_project_id: existing.compose_project_id,
            volume_name: existing.volume_name.as_deref(),
            service_name: service_name.as_deref(),
            stop_service,
            s3_destination_id: destination,
            cron_expression: &common.cron_expression,
            timezone: &common.timezone,
            retention_count,
            enabled,
            next_run_at: common.next_run_at,
        },
    )
    .await
    .map_err(name_taken)?
    .ok_or_else(not_found)?;
    audit(&state, user.id, "update_backup_schedule", &row).await;
    Ok(Json(view_one(&state, row).await?))
}

async fn remove(
    AdminUser(user): AdminUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    let existing = find(&state, id).await?;
    // Past runs stay in the history (their `schedule_id` becomes NULL), and
    // backups already in the bucket are left alone.
    db::backup_schedules::delete(&state.db, id).await?;
    audit(&state, user.id, "delete_backup_schedule", &existing).await;
    Ok(StatusCode::NO_CONTENT)
}

/// Starts a run now, outside the schedule (still recorded as a scheduled
/// run, with the schedule's retention). 409 while one is running.
async fn run_now(
    AdminUser(user): AdminUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<(StatusCode, Json<serde_json::Value>)> {
    let row = find(&state, id).await?;
    if db::backup_schedules::has_running_run(&state.db, id).await? {
        return Err(ApiError::Conflict(
            "a run of this schedule is still in progress".into(),
        ));
    }
    let run = scheduler::start_run(&context(&state), &row).await?;
    audit(&state, user.id, "run_backup_schedule", &row).await;
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({ "run_id": run.id, "status": run.status })),
    ))
}

#[derive(Deserialize)]
struct PreviewBody {
    cron_expression: String,
    #[serde(default = "default_timezone")]
    timezone: String,
}

#[derive(Serialize)]
struct PreviewView {
    next_runs: Vec<DateTime<Utc>>,
}

/// The next five run times of an expression, for the editor.
async fn preview(
    AdminUser(_): AdminUser,
    Json(body): Json<PreviewBody>,
) -> ApiResult<Json<PreviewView>> {
    let spec = CronSpec::parse(&body.cron_expression).map_err(ApiError::BadRequest)?;
    let tz =
        panel_core::schedule::parse_timezone(body.timezone.trim()).map_err(ApiError::BadRequest)?;
    let mut next_runs = Vec::new();
    let mut after = Utc::now();
    while next_runs.len() < 5 {
        match spec.next_after(after, &tz) {
            Some(t) => {
                next_runs.push(t);
                after = t;
            }
            None => break,
        }
    }
    Ok(Json(PreviewView { next_runs }))
}

fn not_found() -> ApiError {
    ApiError::NotFound("backup schedule not found".into())
}

async fn find(state: &AppState, id: Uuid) -> ApiResult<ScheduleRow> {
    db::backup_schedules::find(&state.db, id)
        .await?
        .ok_or_else(not_found)
}

async fn audit(state: &AppState, user_id: Uuid, action: &str, row: &ScheduleRow) {
    db::audit::record(
        &state.db,
        Some(user_id),
        "user",
        action,
        Some(&row.name),
        json!({
            "schedule_id": row.id, "target_kind": row.target_kind,
            "compose_project_id": row.compose_project_id, "volume_name": row.volume_name,
            "destination_id": row.s3_destination_id, "cron_expression": row.cron_expression,
            "timezone": row.timezone, "retention_count": row.retention_count, "enabled": row.enabled,
        }),
    )
    .await
    .ok();
}

#[cfg(test)]
mod tests {
    use crate::services::backup_schedules::{self as scheduler, BackupContext};
    use crate::test_support;
    use axum::body::Body;
    use axum::http::{Method, Request, StatusCode};
    use panel_core::adapters::mock::MockAdapter;
    use panel_core::error::CoreError;
    use serde_json::{json, Value};
    use std::sync::Arc;
    use tower::ServiceExt;
    use uuid::Uuid;

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

    fn parse(text: &str) -> Value {
        serde_json::from_str(text).unwrap_or_else(|_| panic!("not JSON: {text}"))
    }

    async fn wait_for_run(router: &axum::Router, cookie: &str, id: &str) -> Value {
        for _ in 0..100 {
            let (_, body) = call(
                router,
                cookie,
                Method::GET,
                &format!("/api/backups/runs/{id}"),
                None,
            )
            .await;
            let run = parse(&body);
            if run["status"] != "running" {
                return run;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        panic!("run never finished");
    }

    async fn database_service(router: &axum::Router, cookie: &str, engine: &str) -> (Uuid, String) {
        let slug = format!("db{}", &Uuid::new_v4().simple().to_string()[..12]);
        let (status, body) = call(
            router,
            cookie,
            Method::POST,
            "/api/database-services",
            Some(
                json!({ "name": format!("{engine} db"), "slug": slug, "engine": engine,
                         "database_name": "shop", "username": "shop" }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        let id = parse(&body)["service"]["project"]["id"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        (id, slug)
    }

    #[tokio::test]
    async fn schedules_are_validated_run_with_sealed_credentials_and_never_leak_them() {
        let _serial = test_support::QUEUE_TESTS.lock().await;
        let pool = test_support::test_pool().await;
        let adapter = Arc::new(MockAdapter::new());
        let state = test_support::app_state(pool.clone(), adapter.clone());
        let ctx = BackupContext {
            db: state.db.clone(),
            agents: state.agents.clone(),
            notifier: state.notifier.clone(),
            secret_box: state.secret_box.clone(),
        };
        let admin = test_support::user(&pool, true).await;
        let plain = test_support::user(&pool, false).await;
        let cookie = test_support::session_cookie(&state, admin.id).await;
        let plain_cookie = test_support::session_cookie(&state, plain.id).await;
        let router = crate::routes::build_router(state);
        let s3_secret = format!("sk-{}", Uuid::new_v4());

        let (status, body) = call(&router, &cookie, Method::POST, "/api/backups/destinations",
            Some(json!({ "name": format!("sched-{}", Uuid::new_v4()), "endpoint": "https://s3.example.com",
                "region": "auto", "bucket": "my-backups", "prefix": "sched", "retention_count": 9,
                "access_key": "AKIAEXAMPLE", "secret_key": s3_secret }))).await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let dest = parse(&body)["id"].as_str().unwrap().to_string();

        let (db_id, db_slug) = database_service(&router, &cookie, "postgres").await;
        let row = crate::db::database_services::find(&pool, db_id)
            .await
            .unwrap()
            .unwrap();
        let password = crate::services::database_services::open(&ctx.secret_box, &row)
            .unwrap()
            .password;

        // Admin only.
        let (status, _) = call(
            &router,
            &plain_cookie,
            Method::GET,
            "/api/backups/schedules",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);

        let base = json!({ "name": "nightly", "target_kind": "database", "compose_project_id": db_id,
            "s3_destination_id": dest, "cron_expression": "0 3 * * *",
            "timezone": "Europe/Istanbul", "retention_count": 3 });
        let (status, body) = call(
            &router,
            &cookie,
            Method::POST,
            "/api/backups/schedules",
            Some(base.clone()),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        assert!(
            !body.contains(&password) && !body.contains(&s3_secret),
            "{body}"
        );
        let created = parse(&body);
        assert_eq!(created["database_engine"], "postgres");
        assert_eq!(created["database_name"], "shop");
        assert_eq!(created["project_slug"], db_slug.as_str());
        assert!(created["next_run_at"].is_string());
        let id = created["id"].as_str().unwrap().to_string();

        // Same name on the same service conflicts.
        let (status, _) = call(
            &router,
            &cookie,
            Method::POST,
            "/api/backups/schedules",
            Some(base.clone()),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);

        // Hostile or impossible input is refused.
        let (redis, _) = database_service(&router, &cookie, "redis").await;
        let (raw, raw_slug) = test_support::compose_project(&pool).await;
        let with = |patch: Value| {
            let mut body = base.clone();
            body["name"] = json!(format!("n-{}", Uuid::new_v4()));
            body.as_object_mut()
                .unwrap()
                .extend(patch.as_object().unwrap().clone());
            body
        };
        for bad in [
            with(json!({ "cron_expression": "* * *" })),
            with(json!({ "cron_expression": "0 0 31 2 *" })),
            with(json!({ "cron_expression": "@reboot" })),
            with(json!({ "timezone": "Mars/Olympus" })),
            with(json!({ "retention_count": 1001 })),
            with(json!({ "s3_destination_id": Uuid::new_v4() })),
            with(json!({ "compose_project_id": Uuid::new_v4() })),
            with(json!({ "compose_project_id": redis })),
            with(json!({ "compose_project_id": raw })),
            with(json!({ "volume_name": "data" })),
            with(json!({ "target_kind": "host" })),
            with(
                json!({ "target_kind": "volume", "compose_project_id": raw, "volume_name": "../etc" }),
            ),
            with(
                json!({ "target_kind": "volume", "compose_project_id": raw, "volume_name": "data", "stop_service": true }),
            ),
            with(json!({ "name": "" })),
        ] {
            let (status, text) = call(
                &router,
                &cookie,
                Method::POST,
                "/api/backups/schedules",
                Some(bad.clone()),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}: {text}");
        }

        // Run now: the agent gets the service's sealed password, the
        // service's series, and the schedule's retention.
        let (status, body) = call(
            &router,
            &cookie,
            Method::POST,
            &format!("/api/backups/schedules/{id}/run"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        let run_id = parse(&body)["run_id"].as_str().unwrap().to_string();
        let run = wait_for_run(&router, &cookie, &run_id).await;
        assert_eq!(run["status"], "succeeded", "{run}");
        assert_eq!(run["trigger"], "schedule");
        assert_eq!(run["schedule_id"], id.as_str());
        assert_eq!(run["source"], format!("{db_slug}/shop"));
        {
            let sent = adapter.database_backups.lock().unwrap();
            let sent = sent.last().unwrap();
            assert_eq!(sent.connection.password, password);
            assert_eq!(sent.connection.host, "127.0.0.1");
            assert_eq!(i32::from(sent.connection.port), row.loopback_port);
            assert_eq!(
                (
                    sent.connection.database.as_str(),
                    sent.connection.username.as_str()
                ),
                ("shop", "shop")
            );
            assert_eq!(sent.retention_count, 3);
            assert_eq!(
                sent.namespace.as_deref(),
                Some(format!("svc-{db_slug}").as_str())
            );
            assert_eq!(sent.target.secret_key, s3_secret);
        }
        let (_, body) = call(
            &router,
            &cookie,
            Method::GET,
            &format!("/api/backups/runs?schedule_id={id}"),
            None,
        )
        .await;
        assert_eq!(parse(&body).as_array().unwrap().len(), 1);

        // A failure is recorded with the password and the S3 key redacted.
        adapter.fail_next_call_with(
            "backup_database",
            CoreError::External(format!(
                "pg_dump: auth failed for {password} via {s3_secret}"
            )),
        );
        let (status, body) = call(
            &router,
            &cookie,
            Method::POST,
            &format!("/api/backups/schedules/{id}/run"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        let run = wait_for_run(&router, &cookie, parse(&body)["run_id"].as_str().unwrap()).await;
        assert_eq!(run["status"], "failed");
        let error = run["error"].as_str().unwrap();
        assert!(
            error.contains("auth failed")
                && !error.contains(&password)
                && !error.contains(&s3_secret),
            "{error}"
        );
        let (_, body) = call(
            &router,
            &cookie,
            Method::GET,
            &format!("/api/backups/schedules/{id}"),
            None,
        )
        .await;
        assert_eq!(parse(&body)["last_run_status"], "failed");

        // The scheduler starts a due schedule once and moves it forward.
        let past = chrono::Utc::now() - chrono::Duration::minutes(5);
        sqlx::query("UPDATE backup_schedules SET next_run_at = $2 WHERE id = $1")
            .bind(id.parse::<Uuid>().unwrap())
            .bind(past)
            .execute(&pool)
            .await
            .unwrap();
        let before = adapter.database_backups.lock().unwrap().len();
        let now = chrono::Utc::now();
        assert!(scheduler::tick(&ctx, now).await.unwrap() >= 1);
        let row = crate::db::backup_schedules::find(&pool, id.parse().unwrap())
            .await
            .unwrap()
            .unwrap();
        assert!(row.next_run_at.unwrap() > now);
        let last = row.last_run_id.unwrap().to_string();
        assert_eq!(
            wait_for_run(&router, &cookie, &last).await["status"],
            "succeeded"
        );
        assert_eq!(adapter.database_backups.lock().unwrap().len(), before + 1);
        // Not due any more: a second pass does not start it again.
        scheduler::tick(&ctx, now).await.unwrap();
        let again = crate::db::backup_schedules::find(&pool, id.parse().unwrap())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(again.last_run_id, row.last_run_id);

        // Updates: pause, resume with a new expression, refuse bad values.
        let uri = format!("/api/backups/schedules/{id}");
        let (status, body) = call(
            &router,
            &cookie,
            Method::PUT,
            &uri,
            Some(json!({ "enabled": false })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(parse(&body)["next_run_at"].is_null());
        let (status, body) = call(
            &router,
            &cookie,
            Method::PUT,
            &uri,
            Some(
                json!({ "enabled": true, "cron_expression": "*/30 * * * *", "retention_count": 0 }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let updated = parse(&body);
        assert_eq!(updated["cron_expression"], "*/30 * * * *");
        assert_eq!(updated["retention_count"], 0);
        assert_eq!(updated["timezone"], "Europe/Istanbul");
        assert!(updated["next_run_at"].is_string());
        for bad in [
            json!({ "cron_expression": "nope" }),
            json!({ "timezone": "x/../y" }),
            json!({ "service_name": "web" }),
            json!({ "retention_count": -1 }),
        ] {
            let (status, _) = call(&router, &cookie, Method::PUT, &uri, Some(bad.clone())).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}");
        }

        // Volume schedules back up the service's volume.
        let (status, body) = call(
            &router,
            &cookie,
            Method::POST,
            "/api/backups/schedules",
            Some(
                json!({ "name": "volume", "target_kind": "volume", "compose_project_id": raw,
                "volume_name": "data", "service_name": "db", "stop_service": true,
                "s3_destination_id": dest, "cron_expression": "@daily" }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let volume = parse(&body);
        assert_eq!(volume["timezone"], "UTC");
        assert_eq!(volume["retention_count"], 7);
        let volume_id = volume["id"].as_str().unwrap().to_string();
        let (status, body) = call(
            &router,
            &cookie,
            Method::POST,
            &format!("/api/backups/schedules/{volume_id}/run"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        let run = wait_for_run(&router, &cookie, parse(&body)["run_id"].as_str().unwrap()).await;
        assert_eq!(run["status"], "succeeded", "{run}");
        assert_eq!(run["kind"], "volume_backup");
        assert_eq!(run["source"], format!("{raw_slug}/data"));
        // Clearing the service while it is still stopped is refused.
        let (status, _) = call(
            &router,
            &cookie,
            Method::PUT,
            &format!("/api/backups/schedules/{volume_id}"),
            Some(json!({ "service_name": null })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        // Secrets never reach the list or the audit trail.
        let (_, body) = call(
            &router,
            &cookie,
            Method::GET,
            "/api/backups/schedules",
            None,
        )
        .await;
        assert!(!body.contains(&password) && !body.contains(&s3_secret));
        let leaked: Vec<(Value,)> = sqlx::query_as(
            "SELECT metadata FROM audit_log WHERE metadata::text LIKE '%' || $1 || '%' OR metadata::text LIKE '%' || $2 || '%'",
        )
        .bind(&password)
        .bind(&s3_secret)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert!(leaked.is_empty());

        // Deleting keeps the run history; deleting the destination drops its schedules.
        let (status, _) = call(&router, &cookie, Method::DELETE, &uri, None).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (status, _) = call(&router, &cookie, Method::GET, &uri, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (_, body) = call(
            &router,
            &cookie,
            Method::GET,
            &format!("/api/backups/runs/{run_id}"),
            None,
        )
        .await;
        assert!(parse(&body)["schedule_id"].is_null());
        let (status, _) = call(
            &router,
            &cookie,
            Method::DELETE,
            &format!("/api/backups/destinations/{dest}"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (status, _) = call(
            &router,
            &cookie,
            Method::GET,
            &format!("/api/backups/schedules/{volume_id}"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn preview_lists_the_next_runs() {
        let pool = test_support::test_pool().await;
        let state = test_support::app_state(pool.clone(), Arc::new(MockAdapter::new()));
        let admin = test_support::user(&pool, true).await;
        let cookie = test_support::session_cookie(&state, admin.id).await;
        let router = crate::routes::build_router(state);
        let (status, body) = call(
            &router,
            &cookie,
            Method::POST,
            "/api/backups/schedules/preview",
            Some(json!({ "cron_expression": "@hourly", "timezone": "Asia/Kolkata" })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let runs = parse(&body)["next_runs"].as_array().unwrap().clone();
        assert_eq!(runs.len(), 5);
        for bad in [
            json!({ "cron_expression": "x" }),
            json!({ "cron_expression": "@daily", "timezone": "Nope/Zone" }),
        ] {
            let (status, _) = call(
                &router,
                &cookie,
                Method::POST,
                "/api/backups/schedules/preview",
                Some(bad),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
        }
    }
}
