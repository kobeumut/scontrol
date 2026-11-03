//! Scheduled jobs of a Compose project (`/api/compose-projects/:id/jobs`).
//!
//! A job runs one exec-form command in a service's running container from
//! a systemd timer the agent generates. Commands are argv arrays and are
//! never joined into a shell string; `sh -c` style command strings are
//! refused (`panel_core::schedule::validate_job_command`). The database row
//! is written first and rolled back if the agent cannot install the units,
//! so the two never disagree about which jobs exist.
//!
//! Runs are collected from the agent by `services::scheduled_jobs`; their
//! output stays on the agent and is read here by byte offset.

use crate::auth::AdminUser;
use crate::db;
use crate::db::scheduled_jobs::{JobInput, JobRunRow};
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use panel_core::adapters::{ScheduledJobRef, ScheduledJobRunLogParams, UpsertScheduledJobParams};
use panel_core::error::CoreError;
use panel_core::schedule::{
    JobSchedule, ScheduledJob, ScheduledJobSpec, DEFAULT_JOB_TIMEOUT_SECS, JOB_RUNS_RETAINED,
    MAX_JOBS_PER_PROJECT,
};
use panel_core::types::{ComposeProject, ComposeSource, DeploymentLogChunk};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Routes merged into `/api/compose-projects`.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/:id/jobs", get(list).post(create))
        .route("/:id/jobs/:job_id", get(show).put(replace).delete(remove))
        .route("/:id/jobs/:job_id/run", post(run_now))
        .route("/:id/jobs/:job_id/runs", get(runs))
        .route("/:id/jobs/:job_id/runs/:run_id/log", get(run_log))
}

/// Routes nested at `/api/scheduled-jobs`.
pub fn preview_router() -> Router<AppState> {
    Router::new().route("/preview", post(preview))
}

#[derive(Deserialize)]
struct JobBody {
    name: String,
    service_name: String,
    command: Vec<String>,
    schedule: JobSchedule,
    #[serde(default = "default_timezone")]
    timezone: String,
    #[serde(default = "default_timeout")]
    timeout_secs: u32,
    #[serde(default = "default_true")]
    enabled: bool,
}

fn default_timezone() -> String {
    "UTC".into()
}

fn default_timeout() -> u32 {
    DEFAULT_JOB_TIMEOUT_SECS
}

fn default_true() -> bool {
    true
}

impl JobBody {
    fn spec(&self) -> ScheduledJobSpec {
        ScheduledJobSpec {
            service_name: self.service_name.trim().to_string(),
            command: self.command.clone(),
            schedule: match &self.schedule {
                JobSchedule::Cron(e) => JobSchedule::Cron(e.trim().to_string()),
                JobSchedule::Calendar(e) => JobSchedule::Calendar(e.trim().to_string()),
            },
            timezone: self.timezone.trim().to_string(),
            timeout_secs: self.timeout_secs,
        }
    }

    /// The validated spec and its `OnCalendar=` values.
    fn validate(&self) -> ApiResult<(ScheduledJobSpec, Vec<String>)> {
        let name = self.name.trim();
        if name.is_empty() || name.chars().count() > 64 || name.chars().any(char::is_control) {
            return Err(ApiError::BadRequest(
                "a job name has 1 to 64 characters".into(),
            ));
        }
        let spec = self.spec();
        let on_calendar = spec.validate().map_err(ApiError::BadRequest)?;
        Ok((spec, on_calendar))
    }
}

fn input<'a>(
    name: &'a str,
    spec: &'a ScheduledJobSpec,
    on_calendar: &'a [String],
    enabled: bool,
) -> JobInput<'a> {
    JobInput {
        name,
        service_name: &spec.service_name,
        command: &spec.command,
        schedule: &spec.schedule,
        timezone: &spec.timezone,
        on_calendar,
        timeout_secs: spec.timeout_secs,
        enabled,
    }
}

fn stored_spec(job: &ScheduledJob) -> ScheduledJobSpec {
    ScheduledJobSpec {
        service_name: job.service_name.clone(),
        command: job.command.clone(),
        schedule: job.schedule.clone(),
        timezone: job.timezone.clone(),
        timeout_secs: job.timeout_secs,
    }
}

async fn list(
    AdminUser(_): AdminUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Vec<ScheduledJob>>> {
    let project = find_project(&state, id).await?;
    Ok(Json(
        db::scheduled_jobs::list_for_project(&state.db, project.id).await?,
    ))
}

async fn show(
    AdminUser(_): AdminUser,
    State(state): State<AppState>,
    Path((id, job_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<Json<ScheduledJob>> {
    Ok(Json(find_job(&state, id, job_id).await?))
}

async fn create(
    AdminUser(user): AdminUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(body): Json<JobBody>,
) -> ApiResult<(StatusCode, Json<ScheduledJob>)> {
    let project = find_project(&state, id).await?;
    if matches!(project.source, ComposeSource::Static(_)) {
        return Err(ApiError::BadRequest(
            "a static site runs no container to execute a job in".into(),
        ));
    }
    let (spec, on_calendar) = body.validate()?;
    let count = db::scheduled_jobs::count_for_project(&state.db, project.id).await?;
    if count >= MAX_JOBS_PER_PROJECT as i64 {
        return Err(ApiError::Conflict(format!(
            "a project can have at most {MAX_JOBS_PER_PROJECT} scheduled jobs"
        )));
    }
    let name = body.name.trim();
    let job_id = Uuid::new_v4();
    let job = db::scheduled_jobs::create(
        &state.db,
        job_id,
        project.id,
        &input(name, &spec, &on_calendar, body.enabled),
    )
    .await
    .map_err(|e| conflict_on_unique(e, "a job with this name already exists"))?;

    let installed = state
        .agent
        .upsert_scheduled_job(UpsertScheduledJobParams {
            job_id,
            project_slug: project.slug.clone(),
            spec,
            enabled: body.enabled,
        })
        .await;
    if let Err(error) = installed {
        // Undo both sides: the agent may have written part of the units.
        let _ = state
            .agent
            .remove_scheduled_job(ScheduledJobRef {
                project_slug: project.slug.clone(),
                job_id,
            })
            .await;
        db::scheduled_jobs::delete(&state.db, job_id).await?;
        return Err(error.into());
    }
    audit(&state, user.id, "create_scheduled_job", &job, &project).await;
    Ok((StatusCode::CREATED, Json(job)))
}

async fn replace(
    AdminUser(user): AdminUser,
    State(state): State<AppState>,
    Path((id, job_id)): Path<(Uuid, Uuid)>,
    Json(body): Json<JobBody>,
) -> ApiResult<Json<ScheduledJob>> {
    let project = find_project(&state, id).await?;
    let previous = find_job(&state, id, job_id).await?;
    let (spec, on_calendar) = body.validate()?;
    let name = body.name.trim();
    let job = db::scheduled_jobs::update(
        &state.db,
        job_id,
        &input(name, &spec, &on_calendar, body.enabled),
    )
    .await
    .map_err(|e| conflict_on_unique(e, "a job with this name already exists"))?
    .ok_or_else(|| ApiError::NotFound("scheduled job not found".into()))?;

    let installed = state
        .agent
        .upsert_scheduled_job(UpsertScheduledJobParams {
            job_id,
            project_slug: project.slug.clone(),
            spec,
            enabled: body.enabled,
        })
        .await;
    if let Err(error) = installed {
        // Put the previous definition back on both sides.
        let old_spec = stored_spec(&previous);
        db::scheduled_jobs::update(
            &state.db,
            job_id,
            &input(
                &previous.name,
                &old_spec,
                &previous.on_calendar,
                previous.enabled,
            ),
        )
        .await?;
        let _ = state
            .agent
            .upsert_scheduled_job(UpsertScheduledJobParams {
                job_id,
                project_slug: project.slug.clone(),
                spec: old_spec,
                enabled: previous.enabled,
            })
            .await;
        return Err(error.into());
    }
    audit(&state, user.id, "update_scheduled_job", &job, &project).await;
    Ok(Json(job))
}

async fn remove(
    AdminUser(user): AdminUser,
    State(state): State<AppState>,
    Path((id, job_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<Json<serde_json::Value>> {
    let project = find_project(&state, id).await?;
    let job = find_job(&state, id, job_id).await?;
    match state
        .agent
        .remove_scheduled_job(ScheduledJobRef {
            project_slug: project.slug.clone(),
            job_id,
        })
        .await
    {
        Ok(()) | Err(CoreError::NotFound(_)) => {}
        Err(error) => return Err(error.into()),
    }
    db::scheduled_jobs::delete(&state.db, job_id).await?;
    audit(&state, user.id, "remove_scheduled_job", &job, &project).await;
    Ok(Json(serde_json::json!({ "deleted": true })))
}

/// Starts a run now (also for a disabled job). `409` while a run of the
/// job is in progress; runs never overlap.
async fn run_now(
    AdminUser(user): AdminUser,
    State(state): State<AppState>,
    Path((id, job_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<(StatusCode, Json<serde_json::Value>)> {
    let project = find_project(&state, id).await?;
    let job = find_job(&state, id, job_id).await?;
    state
        .agent
        .run_scheduled_job(ScheduledJobRef {
            project_slug: project.slug.clone(),
            job_id,
        })
        .await?;
    state.job_runs.run_started();
    audit(&state, user.id, "run_scheduled_job", &job, &project).await;
    Ok((
        StatusCode::ACCEPTED,
        Json(serde_json::json!({ "started": true })),
    ))
}

async fn runs(
    AdminUser(_): AdminUser,
    State(state): State<AppState>,
    Path((id, job_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<Json<Vec<JobRunRow>>> {
    find_job(&state, id, job_id).await?;
    Ok(Json(
        db::scheduled_jobs::list_runs(&state.db, job_id, JOB_RUNS_RETAINED as i64).await?,
    ))
}

#[derive(Deserialize)]
struct LogQuery {
    #[serde(default)]
    offset: u64,
    max_bytes: Option<u32>,
}

#[derive(Serialize)]
struct RunLogResponse {
    #[serde(flatten)]
    chunk: DeploymentLogChunk,
    status: String,
}

async fn run_log(
    AdminUser(_): AdminUser,
    State(state): State<AppState>,
    Path((id, job_id, run_id)): Path<(Uuid, Uuid, Uuid)>,
    Query(query): Query<LogQuery>,
) -> ApiResult<Json<RunLogResponse>> {
    let project = find_project(&state, id).await?;
    find_job(&state, id, job_id).await?;
    let run = db::scheduled_jobs::find_run(&state.db, job_id, run_id)
        .await?
        .ok_or_else(|| ApiError::NotFound("job run not found".into()))?;
    let chunk = state
        .agent
        .read_scheduled_job_run_log(ScheduledJobRunLogParams {
            project_slug: project.slug,
            job_id,
            run_id,
            offset: query.offset,
            max_bytes: query.max_bytes.unwrap_or(64 * 1024),
        })
        .await?;
    Ok(Json(RunLogResponse {
        chunk,
        status: run.status,
    }))
}

#[derive(Deserialize)]
struct PreviewBody {
    schedule: JobSchedule,
    #[serde(default = "default_timezone")]
    timezone: String,
}

/// The `OnCalendar=` values a schedule produces, for the editor.
async fn preview(
    AdminUser(_): AdminUser,
    Json(body): Json<PreviewBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let schedule = match body.schedule {
        JobSchedule::Cron(e) => JobSchedule::Cron(e.trim().to_string()),
        JobSchedule::Calendar(e) => JobSchedule::Calendar(e.trim().to_string()),
    };
    let on_calendar = schedule
        .on_calendar(body.timezone.trim())
        .map_err(ApiError::BadRequest)?;
    Ok(Json(serde_json::json!({ "on_calendar": on_calendar })))
}

async fn find_project(state: &AppState, id: Uuid) -> ApiResult<ComposeProject> {
    db::compose_projects::find_by_id(&state.db, id)
        .await?
        .ok_or_else(|| ApiError::NotFound("Compose project not found".into()))
}

async fn find_job(state: &AppState, project_id: Uuid, job_id: Uuid) -> ApiResult<ScheduledJob> {
    db::scheduled_jobs::find(&state.db, project_id, job_id)
        .await?
        .ok_or_else(|| ApiError::NotFound("scheduled job not found".into()))
}

fn conflict_on_unique(error: anyhow::Error, message: &str) -> ApiError {
    let unique = error
        .downcast_ref::<sqlx::Error>()
        .and_then(|e| e.as_database_error())
        .is_some_and(|e| e.is_unique_violation());
    if unique {
        ApiError::Conflict(message.into())
    } else {
        ApiError::Internal(error)
    }
}

/// Audit entries name the job and its schedule, never more of the command
/// than its program (arguments may carry data the operator typed in).
async fn audit(
    state: &AppState,
    user_id: Uuid,
    action: &str,
    job: &ScheduledJob,
    project: &ComposeProject,
) {
    db::audit::record(
        &state.db,
        Some(user_id),
        "user",
        action,
        Some(&job.id.to_string()),
        serde_json::json!({
            "project_id": project.id,
            "name": job.name,
            "service_name": job.service_name,
            "program": job.command.first(),
            "schedule": job.schedule,
            "timezone": job.timezone,
            "enabled": job.enabled,
        }),
    )
    .await
    .ok();
}

#[cfg(test)]
mod tests {
    use crate::services::scheduled_jobs::collect_once;
    use crate::test_support;
    use axum::body::Body;
    use axum::http::{Method, Request, StatusCode};
    use chrono::Utc;
    use panel_core::adapters::mock::MockAdapter;
    use panel_core::error::CoreError;
    use panel_core::schedule::{JobRunInfo, JobRunStatus, JobRunTrigger};
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
        let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, body)
    }

    /// A Compose project that was never deployed (no agent work queued).
    async fn project(router: &axum::Router, cookie: &str, source: Value) -> (String, String) {
        let slug = format!("job{}", &Uuid::new_v4().simple().to_string()[..12]);
        let (status, body) = call(
            router,
            cookie,
            Method::POST,
            "/api/compose-projects",
            Some(json!({ "name": "Jobs", "slug": slug, "source": source, "deploy": false })),
        )
        .await;
        assert!(status.is_success(), "{status} {body}");
        (body["project"]["id"].as_str().unwrap().to_string(), slug)
    }

    fn raw() -> Value {
        json!({ "kind": "raw", "compose_yaml": "services:\n  web:\n    image: nginx\n" })
    }

    fn job(name: &str) -> Value {
        json!({
            "name": name,
            "service_name": "web",
            "command": ["php", "artisan", "schedule:run"],
            "schedule": { "kind": "cron", "expression": "0 9 1,15 * mon" },
            "timezone": "Europe/Istanbul",
            "timeout_secs": 120,
        })
    }

    #[tokio::test]
    async fn jobs_are_validated_installed_and_rolled_back() {
        let pool = test_support::test_pool().await;
        let adapter = Arc::new(MockAdapter::new());
        let state = test_support::app_state(pool.clone(), adapter.clone());
        let admin = test_support::user(&pool, true).await;
        let cookie = test_support::session_cookie(&state, admin.id).await;
        let router = crate::routes::build_router(state);
        let (id, slug) = project(&router, &cookie, raw()).await;
        let jobs = format!("/api/compose-projects/{id}/jobs");

        let mut shell = job("shell");
        shell["command"] = json!(["sh", "-c", "php artisan schedule:run"]);
        let mut wrapped = job("wrapped");
        wrapped["command"] = json!(["env", "A=1", "bash", "-lc", "id"]);
        let mut bad_cron = job("cron");
        bad_cron["schedule"] = json!({ "kind": "cron", "expression": "61 * * * *" });
        let mut injected = job("calendar");
        injected["schedule"] =
            json!({ "kind": "calendar", "expression": "daily\nExecStart=/bin/sh" });
        let mut bad_zone = job("zone");
        bad_zone["timezone"] = json!("../../etc/passwd");
        let mut bad_service = job("service");
        bad_service["service_name"] = json!("--privileged");
        let mut bad_timeout = job("timeout");
        bad_timeout["timeout_secs"] = json!(0);
        for body in [
            shell,
            wrapped,
            bad_cron,
            injected,
            bad_zone,
            bad_service,
            bad_timeout,
            job(""),
        ] {
            let (status, response) =
                call(&router, &cookie, Method::POST, &jobs, Some(body.clone())).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body} → {response}");
        }
        assert!(adapter.job_upserts.lock().unwrap().is_empty());

        let (status, created) =
            call(&router, &cookie, Method::POST, &jobs, Some(job("nightly"))).await;
        assert_eq!(status, StatusCode::CREATED, "{created}");
        assert_eq!(
            created["on_calendar"],
            json!([
                "*-*-01,15 09:00:00 Europe/Istanbul",
                "Mon *-*-* 09:00:00 Europe/Istanbul"
            ])
        );
        assert_eq!(created["enabled"], true);
        let job_id = created["id"].as_str().unwrap().to_string();
        {
            let upserts = adapter.job_upserts.lock().unwrap();
            let sent = upserts.last().unwrap();
            assert_eq!(sent.project_slug, slug);
            assert_eq!(sent.job_id.to_string(), job_id);
            assert_eq!(sent.spec.command, ["php", "artisan", "schedule:run"]);
            assert_eq!(sent.spec.timeout_secs, 120);
        }
        let (status, _) = call(&router, &cookie, Method::POST, &jobs, Some(job("nightly"))).await;
        assert_eq!(status, StatusCode::CONFLICT);

        // The agent failing to install a job leaves nothing behind.
        adapter.fail_next_call("upsert_scheduled_job");
        let (status, _) = call(&router, &cookie, Method::POST, &jobs, Some(job("broken"))).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(
            adapter.calls.lock().unwrap().last().unwrap(),
            "remove_scheduled_job"
        );
        let (_, listed) = call(&router, &cookie, Method::GET, &jobs, None).await;
        assert_eq!(listed.as_array().unwrap().len(), 1);

        // An update the agent refuses restores the previous definition.
        let one = format!("{jobs}/{job_id}");
        let mut changed = job("nightly");
        changed["schedule"] = json!({ "kind": "calendar", "expression": "weekly" });
        changed["enabled"] = json!(false);
        adapter.fail_next_call_with(
            "upsert_scheduled_job",
            CoreError::Validation("systemd rejected the schedule".into()),
        );
        let (status, _) = call(&router, &cookie, Method::PUT, &one, Some(changed.clone())).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (_, current) = call(&router, &cookie, Method::GET, &one, None).await;
        assert_eq!(current["schedule"]["kind"], "cron");
        assert_eq!(current["enabled"], true);
        assert!(adapter.job_upserts.lock().unwrap().last().unwrap().enabled);

        let (status, updated) = call(&router, &cookie, Method::PUT, &one, Some(changed)).await;
        assert_eq!(status, StatusCode::OK, "{updated}");
        assert_eq!(updated["on_calendar"], json!(["weekly Europe/Istanbul"]));
        assert_eq!(updated["enabled"], false);
        assert!(!adapter.job_upserts.lock().unwrap().last().unwrap().enabled);

        // Run now: accepted, or 409 while a run is in progress.
        let (status, _) = call(&router, &cookie, Method::POST, &format!("{one}/run"), None).await;
        assert_eq!(status, StatusCode::ACCEPTED);
        adapter.fail_next_call_with(
            "run_scheduled_job",
            CoreError::AlreadyExists("a run of this job is already in progress".into()),
        );
        let (status, _) = call(&router, &cookie, Method::POST, &format!("{one}/run"), None).await;
        assert_eq!(status, StatusCode::CONFLICT);

        // Jobs are scoped to their project.
        let (other, _) = project(&router, &cookie, raw()).await;
        let (status, _) = call(
            &router,
            &cookie,
            Method::GET,
            &format!("/api/compose-projects/{other}/jobs/{job_id}"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        // Audit rows name the program, not its arguments.
        let audit: Vec<serde_json::Value> = sqlx::query_scalar(
            "SELECT metadata FROM audit_log WHERE target = $1 ORDER BY created_at",
        )
        .bind(&job_id)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert!(!audit.is_empty());
        assert!(audit
            .iter()
            .all(|row| row["program"] == "php" && !row.to_string().contains("schedule:run")));

        let (status, _) = call(&router, &cookie, Method::DELETE, &one, None).await;
        assert_eq!(status, StatusCode::OK);
        let (_, listed) = call(&router, &cookie, Method::GET, &jobs, None).await;
        assert!(listed.as_array().unwrap().is_empty());

        // Static sites run no container.
        let (static_id, _) = project(
            &router,
            &cookie,
            json!({ "kind": "static", "repository_url": "https://github.com/acme/site.git",
                    "branch": "main", "publish_path": "dist" }),
        )
        .await;
        let (status, _) = call(
            &router,
            &cookie,
            Method::POST,
            &format!("/api/compose-projects/{static_id}/jobs"),
            Some(job("nightly")),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn preview_shows_the_generated_calendar() {
        let pool = test_support::test_pool().await;
        let adapter = Arc::new(MockAdapter::new());
        let state = test_support::app_state(pool.clone(), adapter);
        let admin = test_support::user(&pool, true).await;
        let cookie = test_support::session_cookie(&state, admin.id).await;
        let router = crate::routes::build_router(state);
        let (status, body) = call(
            &router,
            &cookie,
            Method::POST,
            "/api/scheduled-jobs/preview",
            Some(json!({ "schedule": { "kind": "cron", "expression": "*/15 9-17 * * 1-5" } })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body["on_calendar"],
            json!(["Mon..Fri *-*-* 09..17:00/15:00 UTC"])
        );
        let (status, body) = call(
            &router,
            &cookie,
            Method::POST,
            "/api/scheduled-jobs/preview",
            Some(json!({ "schedule": { "kind": "cron", "expression": "@reboot" } })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body["error"].as_str().unwrap().contains("macro"));
        let (status, _) = call(
            &router,
            "",
            Method::POST,
            "/api/scheduled-jobs/preview",
            Some(json!({})),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    fn run(job_id: Uuid, status: JobRunStatus, log_bytes: u64) -> JobRunInfo {
        let now = Utc::now();
        JobRunInfo {
            id: Uuid::new_v4(),
            job_id,
            trigger: JobRunTrigger::Schedule,
            status,
            started_at: now,
            finished_at: None,
            exit_code: None,
            error_message: None,
            log_bytes,
            log_truncated: false,
            updated_at: now,
        }
    }

    #[tokio::test]
    async fn collector_stores_runs_once_and_notifies_each_failure_once() {
        let _serial = test_support::QUEUE_TESTS.lock().await;
        let pool = test_support::test_pool().await;
        let adapter = Arc::new(MockAdapter::new());
        let state = test_support::app_state(pool.clone(), adapter.clone());
        let admin = test_support::user(&pool, true).await;
        let cookie = test_support::session_cookie(&state, admin.id).await;
        let notifier = state.notifier.clone();
        let router = crate::routes::build_router(state);
        let (id, _) = project(&router, &cookie, raw()).await;
        let (_, created) = call(
            &router,
            &cookie,
            Method::POST,
            &format!("/api/compose-projects/{id}/jobs"),
            Some(job("collect")),
        )
        .await;
        let job_id: Uuid = created["id"].as_str().unwrap().parse().unwrap();
        let base = format!("/api/compose-projects/{id}/jobs/{job_id}");

        let (url, mut received) = test_support::http_receiver().await;
        let (status, channel) = call(
            &router,
            &cookie,
            Method::POST,
            "/api/notifications",
            Some(
                json!({ "name": format!("jobs {job_id}"), "kind": "webhook", "url": url,
                         "events": ["job_failed"] }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{channel}");

        // A running run is stored and shown as the job's last run.
        let mut failing = run(job_id, JobRunStatus::Running, 10);
        let ok = {
            let mut r = run(job_id, JobRunStatus::Succeeded, 42);
            r.trigger = JobRunTrigger::Manual;
            r.exit_code = Some(0);
            r.finished_at = Some(r.started_at);
            r.started_at -= chrono::Duration::minutes(5);
            r
        };
        adapter
            .job_runs
            .lock()
            .unwrap()
            .extend([ok.clone(), failing.clone()]);
        collect_once(&pool, adapter.as_ref(), &notifier, None)
            .await
            .unwrap();
        let (_, runs) = call(&router, &cookie, Method::GET, &format!("{base}/runs"), None).await;
        let runs = runs.as_array().unwrap();
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0]["status"], "running");
        assert_eq!(runs[1]["trigger"], "manual");
        let (_, shown) = call(&router, &cookie, Method::GET, &base, None).await;
        assert_eq!(shown["last_run_status"], "running");

        // It fails: stored, notified once, however often it is listed again.
        failing.status = JobRunStatus::Failed;
        failing.exit_code = Some(3);
        failing.error_message = Some("the command exited with code 3".into());
        failing.finished_at = Some(Utc::now());
        failing.updated_at = Utc::now();
        *adapter.job_runs.lock().unwrap() = vec![ok.clone(), failing.clone()];
        for _ in 0..3 {
            collect_once(&pool, adapter.as_ref(), &notifier, None)
                .await
                .unwrap();
        }
        let (headers, body) =
            tokio::time::timeout(std::time::Duration::from_secs(10), received.recv())
                .await
                .expect("job_failed notification")
                .unwrap();
        assert!(headers.contains("x-scontrol-event: job_failed"));
        let payload: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(payload["job"]["id"], job_id.to_string());
        assert_eq!(payload["run"]["exit_code"], 3);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(500), received.recv())
                .await
                .is_err(),
            "a failure was notified twice"
        );
        let (_, shown) = call(&router, &cookie, Method::GET, &base, None).await;
        assert_eq!(shown["last_run_status"], "failed");

        // A finished run is never rewritten, even if a stale record shows up.
        let mut stale = failing.clone();
        stale.status = JobRunStatus::Running;
        assert_eq!(
            crate::db::scheduled_jobs::upsert_run(&pool, &stale)
                .await
                .unwrap(),
            None
        );

        // Logs are read through the agent, by run.
        let (status, log) = call(
            &router,
            &cookie,
            Method::GET,
            &format!("{base}/runs/{}/log?offset=0", failing.id),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{log}");
        assert_eq!(log["status"], "failed");
        assert_eq!(log["content"], "mock job output\n");
        let (status, _) = call(
            &router,
            &cookie,
            Method::GET,
            &format!("{base}/runs/{}/log", Uuid::new_v4()),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        let channel_id = channel["id"].as_str().unwrap();
        call(
            &router,
            &cookie,
            Method::DELETE,
            &format!("/api/notifications/{channel_id}"),
            None,
        )
        .await;
    }
}
