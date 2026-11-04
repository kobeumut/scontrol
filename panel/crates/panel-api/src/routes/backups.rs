//! S3 backup destinations and streaming database/volume backups.
//!
//! Access keys are write-only: requests may set or replace them, responses
//! only report metadata. Database passwords are not stored at all; the
//! caller sends one per run, and it travels to the agent (never to a log,
//! audit entry, or run row). Backups and restores are asynchronous: the
//! endpoint records a `running` row, answers `202`, and a task finishes the
//! row, so no HTTP handler waits on a multi-gigabyte upload.

use crate::auth::AdminUser;
use crate::db;
use crate::db::backups::{DestinationFields, DestinationRow, RunRow};
use crate::error::{ApiError, ApiResult};
use crate::services::backups as sealed;
use crate::state::AppState;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use panel_core::adapters::*;
use panel_core::error::CoreError;
use panel_core::types::validate;
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/destinations", get(list).post(create))
        .route("/destinations/test", post(test_new))
        .route("/destinations/:id", get(show).put(update).delete(remove))
        .route("/destinations/:id/test", post(test_saved))
        .route("/destinations/:id/objects", get(objects))
        .route("/destinations/:id/database/backup", post(backup_database))
        .route("/destinations/:id/database/restore", post(restore_database))
        .route("/destinations/:id/volume/backup", post(backup_volume))
        .route("/destinations/:id/volume/restore", post(restore_volume))
        .route("/runs", get(runs))
        .route("/runs/:id", get(run))
}

#[derive(Serialize)]
struct DestinationView {
    id: Uuid,
    name: String,
    endpoint: String,
    region: String,
    bucket: String,
    prefix: String,
    retention_count: i32,
    /// Always true: a destination cannot be stored without its keys.
    has_credential: bool,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl From<DestinationRow> for DestinationView {
    fn from(row: DestinationRow) -> Self {
        Self {
            id: row.id,
            name: row.name,
            endpoint: row.endpoint,
            region: row.region,
            bucket: row.bucket,
            prefix: row.prefix,
            retention_count: row.retention_count,
            has_credential: !row.credential_ciphertext.is_empty(),
            created_at: row.created_at,
            updated_at: row.updated_at,
        }
    }
}

#[derive(Serialize)]
struct RunView {
    id: Uuid,
    destination_id: Uuid,
    kind: String,
    source: String,
    status: String,
    object_key: Option<String>,
    size_bytes: Option<i64>,
    pruned: i32,
    error: Option<String>,
    started_at: DateTime<Utc>,
    finished_at: Option<DateTime<Utc>>,
}

impl From<RunRow> for RunView {
    fn from(r: RunRow) -> Self {
        Self {
            id: r.id,
            destination_id: r.destination_id,
            kind: r.kind,
            source: r.source,
            status: r.status,
            object_key: r.object_key,
            size_bytes: r.size_bytes,
            pruned: r.pruned,
            error: r.error,
            started_at: r.started_at,
            finished_at: r.finished_at,
        }
    }
}

#[derive(Deserialize)]
struct DestinationBody {
    name: String,
    endpoint: String,
    region: String,
    bucket: String,
    #[serde(default = "default_prefix")]
    prefix: String,
    #[serde(default = "default_retention")]
    retention_count: i32,
    /// Both keys are required on create. On update, omit both to keep them.
    #[serde(default)]
    access_key: Option<String>,
    #[serde(default)]
    secret_key: Option<String>,
}

fn default_prefix() -> String {
    "scontrol".into()
}

fn default_retention() -> i32 {
    7
}

struct Checked {
    name: String,
    endpoint: String,
    prefix: String,
}

fn check(body: &DestinationBody) -> ApiResult<Checked> {
    let name = body.name.trim();
    if name.is_empty() || name.chars().count() > 100 {
        return Err(ApiError::BadRequest("name must be 1–100 characters".into()));
    }
    let endpoint = validate::normalize_s3_endpoint(&body.endpoint).ok_or_else(|| {
        ApiError::BadRequest("endpoint must look like https://s3.example.com[:port]".into())
    })?;
    if !validate::is_valid_s3_bucket(&body.bucket) {
        return Err(ApiError::BadRequest("invalid bucket name".into()));
    }
    if !validate::is_valid_s3_region(&body.region) {
        return Err(ApiError::BadRequest(
            "region must be lowercase letters, digits and dashes (use `auto` for R2)".into(),
        ));
    }
    if !validate::is_valid_backup_path(&body.prefix) {
        return Err(ApiError::BadRequest(
            "prefix must be `/`-separated names of letters, digits, . _ -".into(),
        ));
    }
    if !(0..=1000).contains(&body.retention_count) {
        return Err(ApiError::BadRequest(
            "retention_count must be 0 (keep all) to 1000".into(),
        ));
    }
    match (&body.access_key, &body.secret_key) {
        (None, None) => {}
        (Some(a), Some(s)) => {
            if !validate::is_valid_s3_access_key(a) {
                return Err(ApiError::BadRequest("invalid access key".into()));
            }
            if !validate::is_valid_s3_secret_key(s) {
                return Err(ApiError::BadRequest(
                    "secret key must be 1–256 printable characters without whitespace".into(),
                ));
            }
        }
        _ => {
            return Err(ApiError::BadRequest(
                "send the access key and the secret key together".into(),
            ))
        }
    }
    Ok(Checked {
        name: name.to_string(),
        endpoint,
        prefix: body.prefix.clone(),
    })
}

fn name_taken(error: sqlx::Error) -> ApiError {
    match &error {
        sqlx::Error::Database(db) if db.is_unique_violation() => {
            ApiError::Conflict("a destination with this name already exists".into())
        }
        _ => error.into(),
    }
}

async fn list(
    AdminUser(_): AdminUser,
    State(state): State<AppState>,
) -> ApiResult<Json<Vec<DestinationView>>> {
    let rows = db::backups::list_destinations(&state.db).await?;
    Ok(Json(rows.into_iter().map(Into::into).collect()))
}

async fn show(
    AdminUser(_): AdminUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<DestinationView>> {
    Ok(Json(find(&state, id).await?.into()))
}

async fn create(
    AdminUser(user): AdminUser,
    State(state): State<AppState>,
    Json(body): Json<DestinationBody>,
) -> ApiResult<(StatusCode, Json<DestinationView>)> {
    let checked = check(&body)?;
    let (Some(access), Some(secret)) = (&body.access_key, &body.secret_key) else {
        return Err(ApiError::BadRequest(
            "access_key and secret_key are required".into(),
        ));
    };
    let id = Uuid::new_v4();
    let ciphertext = sealed::seal(state.notifier.secrets(), id, access, secret);
    let row = db::backups::create_destination(&state.db, id, &fields(&body, &checked), &ciphertext)
        .await
        .map_err(name_taken)?;
    audit(&state, user.id, "create_backup_destination", &row).await;
    Ok((StatusCode::CREATED, Json(row.into())))
}

fn fields<'a>(body: &'a DestinationBody, c: &'a Checked) -> DestinationFields<'a> {
    DestinationFields {
        name: &c.name,
        endpoint: &c.endpoint,
        region: &body.region,
        bucket: &body.bucket,
        prefix: &c.prefix,
        retention_count: body.retention_count,
    }
}

async fn update(
    AdminUser(user): AdminUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(body): Json<DestinationBody>,
) -> ApiResult<Json<DestinationView>> {
    let checked = check(&body)?;
    let existing = find(&state, id).await?;
    let ciphertext = match (&body.access_key, &body.secret_key) {
        (Some(access), Some(secret)) => sealed::seal(state.notifier.secrets(), id, access, secret),
        _ => {
            // Never send stored keys to a different endpoint or bucket.
            if existing.endpoint != checked.endpoint || existing.bucket != body.bucket {
                return Err(ApiError::BadRequest(
                    "enter the keys again when the endpoint or bucket changes".into(),
                ));
            }
            existing.credential_ciphertext
        }
    };
    let row = db::backups::update_destination(&state.db, id, &fields(&body, &checked), &ciphertext)
        .await
        .map_err(name_taken)?
        .ok_or_else(not_found)?;
    audit(&state, user.id, "update_backup_destination", &row).await;
    Ok(Json(row.into()))
}

async fn remove(
    AdminUser(user): AdminUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    let existing = find(&state, id).await?;
    // Objects already in the bucket are left alone.
    db::backups::delete_destination(&state.db, id).await?;
    audit(&state, user.id, "delete_backup_destination", &existing).await;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Serialize)]
struct TestResult {
    ok: bool,
    message: String,
}

async fn test_saved(
    AdminUser(_): AdminUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<TestResult>> {
    let target = target(&state, &find(&state, id).await?)?;
    Ok(Json(probe(&state, target).await?))
}

/// Tests keys before they are saved. Nothing is stored.
async fn test_new(
    AdminUser(_): AdminUser,
    State(state): State<AppState>,
    Json(body): Json<DestinationBody>,
) -> ApiResult<Json<TestResult>> {
    let checked = check(&body)?;
    let (Some(access_key), Some(secret_key)) = (body.access_key, body.secret_key) else {
        return Err(ApiError::BadRequest(
            "access_key and secret_key are required".into(),
        ));
    };
    let target = S3Target {
        endpoint: checked.endpoint,
        region: body.region,
        bucket: body.bucket,
        access_key,
        secret_key,
    };
    Ok(Json(probe(&state, target).await?))
}

/// A rejected bucket check is an expected outcome, reported in the body.
async fn probe(state: &AppState, target: S3Target) -> ApiResult<TestResult> {
    match state.agent.test_s3_target(TestS3Params { target }).await {
        Ok(()) => Ok(TestResult {
            ok: true,
            message: "bucket reachable".into(),
        }),
        Err(CoreError::External(message)) => Ok(TestResult {
            ok: false,
            message: message.chars().take(500).collect(),
        }),
        Err(other) => Err(other.into()),
    }
}

async fn objects(
    AdminUser(_): AdminUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Vec<BackupObject>>> {
    let dest = find(&state, id).await?;
    let target = target(&state, &dest)?;
    let mut found = state
        .agent
        .list_backups(ListBackupsParams {
            target,
            prefix: dest.prefix,
        })
        .await?;
    found.reverse(); // newest first
    Ok(Json(found))
}

#[derive(Deserialize)]
struct DatabaseBody {
    engine: BackupEngine,
    host: String,
    port: u16,
    database: String,
    username: String,
    password: String,
    /// Restore only: the object to restore.
    #[serde(default)]
    key: Option<String>,
}

impl DatabaseBody {
    fn connection(&self) -> ApiResult<DatabaseConnection> {
        if !validate::is_valid_db_host(&self.host) {
            return Err(ApiError::BadRequest("invalid database host".into()));
        }
        if self.port == 0 {
            return Err(ApiError::BadRequest("invalid database port".into()));
        }
        if !validate::is_valid_identifier(&self.database)
            || !validate::is_valid_identifier(&self.username)
        {
            return Err(ApiError::BadRequest(
                "database and username must be letters, digits and _ (start with a letter)".into(),
            ));
        }
        if !validate::is_valid_db_password(&self.password) {
            return Err(ApiError::BadRequest(
                "password must be 1–256 printable characters without whitespace".into(),
            ));
        }
        Ok(DatabaseConnection {
            engine: self.engine,
            host: self.host.clone(),
            port: self.port,
            database: self.database.clone(),
            username: self.username.clone(),
            password: self.password.clone(),
        })
    }
}

#[derive(Deserialize)]
struct VolumeBody {
    project_slug: String,
    volume: String,
    #[serde(default)]
    service: Option<String>,
    #[serde(default)]
    stop_service: bool,
    /// Restore only: the object to restore.
    #[serde(default)]
    key: Option<String>,
}

impl VolumeBody {
    fn check(&self) -> ApiResult<()> {
        if !validate::is_valid_project_slug(&self.project_slug)
            || !validate::is_valid_volume_name(&self.volume)
        {
            return Err(ApiError::BadRequest(
                "invalid project or volume name".into(),
            ));
        }
        if let Some(service) = &self.service {
            if !validate::is_valid_service_name(service) {
                return Err(ApiError::BadRequest("invalid service name".into()));
            }
        }
        if self.stop_service && self.service.is_none() {
            return Err(ApiError::BadRequest(
                "stopping the container needs a service name".into(),
            ));
        }
        Ok(())
    }
}

fn restore_key(key: &Option<String>, dest: &DestinationRow) -> ApiResult<String> {
    let key = key
        .clone()
        .ok_or_else(|| ApiError::BadRequest("key is required".into()))?;
    // Only objects under this destination's own prefix.
    if !validate::is_valid_backup_path(&key) || !key.starts_with(&format!("{}/", dest.prefix)) {
        return Err(ApiError::BadRequest(
            "key must be a backup under the destination prefix".into(),
        ));
    }
    Ok(key)
}

type Accepted = (StatusCode, Json<RunView>);

async fn backup_database(
    AdminUser(user): AdminUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(body): Json<DatabaseBody>,
) -> ApiResult<Accepted> {
    let connection = body.connection()?;
    let dest = find(&state, id).await?;
    let params = BackupDatabaseParams {
        target: target(&state, &dest)?,
        prefix: dest.prefix.clone(),
        connection,
        retention_count: dest.retention_count as u32,
    };
    let secrets = vec![body.password.clone()];
    let agent = state.agent.clone();
    spawn_run(
        &state,
        user.id,
        &dest,
        "database_backup",
        &body.database,
        secrets,
        async move {
            let out = agent.backup_database(params).await?;
            Ok((
                Some(out.key),
                Some(out.size_bytes as i64),
                out.pruned as i32,
            ))
        },
    )
    .await
}

async fn restore_database(
    AdminUser(user): AdminUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(body): Json<DatabaseBody>,
) -> ApiResult<Accepted> {
    let connection = body.connection()?;
    let dest = find(&state, id).await?;
    let key = restore_key(&body.key, &dest)?;
    let params = RestoreDatabaseParams {
        target: target(&state, &dest)?,
        key: key.clone(),
        connection,
    };
    let agent = state.agent.clone();
    spawn_run(
        &state,
        user.id,
        &dest,
        "database_restore",
        &body.database,
        vec![body.password.clone()],
        async move {
            agent.restore_database(params).await?;
            Ok((Some(key), None, 0))
        },
    )
    .await
}

async fn backup_volume(
    AdminUser(user): AdminUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(body): Json<VolumeBody>,
) -> ApiResult<Accepted> {
    body.check()?;
    ensure_local_service(&state, &body.project_slug).await?;
    let dest = find(&state, id).await?;
    let params = BackupVolumeParams {
        target: target(&state, &dest)?,
        prefix: dest.prefix.clone(),
        project_slug: body.project_slug.clone(),
        service: body.service.clone(),
        volume: body.volume.clone(),
        stop_service: body.stop_service,
        retention_count: dest.retention_count as u32,
    };
    let agent = state.agent.clone();
    spawn_run(
        &state,
        user.id,
        &dest,
        "volume_backup",
        &format!("{}/{}", body.project_slug, body.volume),
        vec![],
        async move {
            let out = agent.backup_volume(params).await?;
            Ok((
                Some(out.key),
                Some(out.size_bytes as i64),
                out.pruned as i32,
            ))
        },
    )
    .await
}

async fn restore_volume(
    AdminUser(user): AdminUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(body): Json<VolumeBody>,
) -> ApiResult<Accepted> {
    body.check()?;
    ensure_local_service(&state, &body.project_slug).await?;
    let dest = find(&state, id).await?;
    let key = restore_key(&body.key, &dest)?;
    let params = RestoreVolumeParams {
        target: target(&state, &dest)?,
        key: key.clone(),
        project_slug: body.project_slug.clone(),
        service: body.service.clone(),
        volume: body.volume.clone(),
        stop_service: body.stop_service,
    };
    let agent = state.agent.clone();
    spawn_run(
        &state,
        user.id,
        &dest,
        "volume_restore",
        &format!("{}/{}", body.project_slug, body.volume),
        vec![],
        async move {
            agent.restore_volume(params).await?;
            Ok((Some(key), None, 0))
        },
    )
    .await
}

type RunOutcome = Result<(Option<String>, Option<i64>, i32), CoreError>;

/// Records a `running` row, answers 202, and finishes the row from a task.
async fn spawn_run(
    state: &AppState,
    user_id: Uuid,
    dest: &DestinationRow,
    kind: &'static str,
    source: &str,
    secrets: Vec<String>,
    work: impl std::future::Future<Output = RunOutcome> + Send + 'static,
) -> ApiResult<Accepted> {
    let run = db::backups::start_run(&state.db, dest.id, kind, source).await?;
    db::audit::record(
        &state.db,
        Some(user_id),
        "user",
        kind,
        Some(&dest.name),
        json!({ "destination_id": dest.id, "run_id": run.id, "source": source }),
    )
    .await
    .ok();
    let pool = state.db.clone();
    let run_id = run.id;
    tokio::spawn(async move {
        let result = work.await.map_err(|e| {
            let mut text = e.to_string();
            for secret in secrets.iter().filter(|s| s.len() >= 4) {
                text = text.replace(secret, "********");
            }
            text.chars().take(1000).collect::<String>()
        });
        if let Err(e) = db::backups::finish_run(&pool, run_id, result).await {
            tracing::error!(error = %e, %run_id, "recording backup run result");
        }
    });
    Ok((StatusCode::ACCEPTED, Json(run.into())))
}

#[derive(Deserialize)]
struct RunsQuery {
    destination_id: Option<Uuid>,
}

async fn runs(
    AdminUser(_): AdminUser,
    State(state): State<AppState>,
    Query(q): Query<RunsQuery>,
) -> ApiResult<Json<Vec<RunView>>> {
    let rows = db::backups::list_runs(&state.db, q.destination_id).await?;
    Ok(Json(rows.into_iter().map(Into::into).collect()))
}

async fn run(
    AdminUser(_): AdminUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<RunView>> {
    db::backups::find_run(&state.db, id)
        .await?
        .map(|r| Json(r.into()))
        .ok_or_else(|| ApiError::NotFound("backup run not found".into()))
}

fn not_found() -> ApiError {
    ApiError::NotFound("backup destination not found".into())
}

/// Volume backups run on the local agent; a service on a remote server is
/// refused rather than looked for on the wrong host.
async fn ensure_local_service(state: &AppState, slug: &str) -> ApiResult<()> {
    let server: Option<Uuid> =
        sqlx::query_scalar("SELECT server_id FROM compose_projects WHERE slug = $1")
            .bind(slug)
            .fetch_optional(&state.db)
            .await?;
    match server {
        Some(server) if server != db::servers::LOCAL_SERVER_ID => Err(ApiError::Conflict(
            "volume backups are not available yet for services on remote servers".into(),
        )),
        _ => Ok(()),
    }
}

async fn find(state: &AppState, id: Uuid) -> ApiResult<DestinationRow> {
    db::backups::find_destination(&state.db, id)
        .await?
        .ok_or_else(not_found)
}

fn target(state: &AppState, row: &DestinationRow) -> ApiResult<S3Target> {
    Ok(sealed::open(state.notifier.secrets(), row)?)
}

/// Audit entries carry destination metadata only, never the keys.
async fn audit(state: &AppState, user_id: Uuid, action: &str, row: &DestinationRow) {
    db::audit::record(
        &state.db,
        Some(user_id),
        "user",
        action,
        Some(&row.name),
        json!({ "destination_id": row.id, "endpoint": row.endpoint, "bucket": row.bucket }),
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
            let run: Value = serde_json::from_str(&body).unwrap();
            if run["status"] != "running" {
                return run;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        panic!("run never finished");
    }

    #[tokio::test]
    async fn keys_are_sealed_never_returned_and_runs_are_recorded() {
        let _serial = test_support::QUEUE_TESTS.lock().await;
        let pool = test_support::test_pool().await;
        let adapter = Arc::new(MockAdapter::new());
        let state = test_support::app_state(pool.clone(), adapter.clone());
        let admin = test_support::user(&pool, true).await;
        let plain = test_support::user(&pool, false).await;
        let cookie = test_support::session_cookie(&state, admin.id).await;
        let plain_cookie = test_support::session_cookie(&state, plain.id).await;
        let secrets = state.notifier.secrets().clone();
        let router = crate::routes::build_router(state);
        let secret = format!("sk-{}", uuid::Uuid::new_v4());
        let name = format!("r2-{}", uuid::Uuid::new_v4());

        let (status, body) = call(
            &router,
            &cookie,
            Method::POST,
            "/api/backups/destinations",
            Some(json!({
                "name": name, "endpoint": "https://ACC.r2.cloudflarestorage.com/", "region": "auto",
                "bucket": "my-backups", "prefix": "scontrol/s1", "retention_count": 3,
                "access_key": "AKIAEXAMPLE", "secret_key": secret,
            })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        assert!(
            !body.contains(&secret) && !body.contains("AKIAEXAMPLE"),
            "{body}"
        );
        let created: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(created["endpoint"], "https://acc.r2.cloudflarestorage.com");
        let id: uuid::Uuid = created["id"].as_str().unwrap().parse().unwrap();

        // Sealed at rest, opens only for this row.
        let row = crate::db::backups::find_destination(&pool, id)
            .await
            .unwrap()
            .unwrap();
        assert!(!String::from_utf8_lossy(&row.credential_ciphertext).contains(&secret));
        assert_eq!(
            crate::services::backups::open(&secrets, &row)
                .unwrap()
                .secret_key,
            secret
        );

        // Admin only.
        let (status, _) = call(
            &router,
            &plain_cookie,
            Method::GET,
            "/api/backups/destinations",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);

        // Keys stay when omitted, but not across an endpoint change.
        let keep = json!({ "name": name, "endpoint": "https://acc.r2.cloudflarestorage.com", "region": "auto", "bucket": "my-backups", "prefix": "scontrol/s1" });
        let (status, body) = call(
            &router,
            &cookie,
            Method::PUT,
            &format!("/api/backups/destinations/{id}"),
            Some(keep),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let moved = json!({ "name": name, "endpoint": "https://evil.example.com", "region": "auto", "bucket": "my-backups", "prefix": "scontrol/s1" });
        let (status, _) = call(
            &router,
            &cookie,
            Method::PUT,
            &format!("/api/backups/destinations/{id}"),
            Some(moved),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        // Hostile input is refused.
        for bad in [
            json!({ "name": "x", "endpoint": "https://h.com/path", "region": "auto", "bucket": "bbb", "access_key": "A", "secret_key": "s" }),
            json!({ "name": "x", "endpoint": "https://h.com", "region": "auto", "bucket": "bbb", "prefix": "../x", "access_key": "A", "secret_key": "s" }),
            json!({ "name": "x", "endpoint": "https://h.com", "region": "auto", "bucket": "bbb", "access_key": "A" }),
            json!({ "name": "x", "endpoint": "https://h.com", "region": "auto", "bucket": "bbb" }),
        ] {
            let (status, _) = call(
                &router,
                &cookie,
                Method::POST,
                "/api/backups/destinations",
                Some(bad),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
        }

        // A database backup is accepted, recorded, and the password stays out of rows.
        let db_password = format!("dbpw-{}", uuid::Uuid::new_v4());
        let body = json!({ "engine": "postgres", "host": "127.0.0.1", "port": 5432,
            "database": "app", "username": "appuser", "password": db_password });
        let (status, text) = call(
            &router,
            &cookie,
            Method::POST,
            &format!("/api/backups/destinations/{id}/database/backup"),
            Some(body.clone()),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{text}");
        assert!(!text.contains(&db_password));
        let run_id = serde_json::from_str::<Value>(&text).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();
        let run = wait_for_run(&router, &cookie, &run_id).await;
        assert_eq!(run["status"], "succeeded", "{run}");
        assert_eq!(run["kind"], "database_backup");
        assert_eq!(run["object_key"], "scontrol/s1/mock.sql.gz");

        // A failing agent step is recorded with a redacted error.
        adapter.fail_next_call_with(
            "backup_database",
            CoreError::External(format!("pg_dump failed: bad password {db_password}")),
        );
        let (status, text) = call(
            &router,
            &cookie,
            Method::POST,
            &format!("/api/backups/destinations/{id}/database/backup"),
            Some(body.clone()),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED);
        let run_id = serde_json::from_str::<Value>(&text).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();
        let run = wait_for_run(&router, &cookie, &run_id).await;
        assert_eq!(run["status"], "failed");
        let error = run["error"].as_str().unwrap();
        assert!(
            error.contains("pg_dump failed") && !error.contains(&db_password),
            "{error}"
        );

        // Restore only accepts keys under the destination's prefix.
        let mut restore = body.clone();
        restore["key"] = json!("other/prefix/x.sql.gz");
        let (status, _) = call(
            &router,
            &cookie,
            Method::POST,
            &format!("/api/backups/destinations/{id}/database/restore"),
            Some(restore.clone()),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        restore["key"] = json!("scontrol/s1/postgres-app-20260101T000000Z.sql.gz");
        let (status, text) = call(
            &router,
            &cookie,
            Method::POST,
            &format!("/api/backups/destinations/{id}/database/restore"),
            Some(restore),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{text}");

        // Volume backups validate names; stopping needs a service.
        for bad in [
            json!({ "project_slug": "../x", "volume": "data" }),
            json!({ "project_slug": "app", "volume": "da ta" }),
            json!({ "project_slug": "app", "volume": "data", "stop_service": true }),
        ] {
            let (status, _) = call(
                &router,
                &cookie,
                Method::POST,
                &format!("/api/backups/destinations/{id}/volume/backup"),
                Some(bad),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
        }
        let (status, text) = call(&router, &cookie, Method::POST, &format!("/api/backups/destinations/{id}/volume/backup"),
            Some(json!({ "project_slug": "app", "volume": "data", "service": "db", "stop_service": true }))).await;
        assert_eq!(status, StatusCode::ACCEPTED, "{text}");

        // The password is in no audit entry.
        let audit: Vec<(serde_json::Value,)> = sqlx::query_as(
            "SELECT metadata FROM audit_log WHERE metadata::text LIKE '%' || $1 || '%'",
        )
        .bind(&db_password)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert!(audit.is_empty());

        let (status, _) = call(
            &router,
            &cookie,
            Method::DELETE,
            &format!("/api/backups/destinations/{id}"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
    }
}
