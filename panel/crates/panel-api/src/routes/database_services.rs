//! Managed database services API (`/api/database-services`).
//!
//! A database service is a Compose project with a `database` source, so its
//! deployments, logs, variables, and history are served by the Compose
//! project endpoints too. This module adds what only databases have:
//! creation with generated credentials and a loopback port, the audited
//! credentials view, lifecycle shortcuts, and outside access.
//!
//! Credentials are sealed at rest and appear only in the response of
//! `POST /:id/credentials` (marked `no-store`); audit rows, errors, list
//! views, and queue rows never contain them.

use crate::auth::{perm, Authenticated, Principal, ServiceAccess};
use crate::db;
use crate::db::database_services::DatabaseServiceRow;
use crate::error::{ApiError, ApiResult};
use crate::services::database_services as credentials;
use crate::state::AppState;
use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use panel_core::adapters::{
    ComposeProjectActionParams, RemoveComposeProjectParams, SetDatabaseExposureParams,
};
use panel_core::error::CoreError;
use panel_core::rbac::{Permission, Target};
use panel_core::types::{
    validate, ComposeProject, ComposeSource, DatabaseExposure, DatabaseServiceSpec, DeploymentInfo,
    DeploymentStatus, ManagedDatabaseEngine,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(list).post(create))
        .route("/engines", get(engines))
        .route("/:id", get(show).delete(remove))
        .route("/:id/credentials", post(reveal_credentials))
        .route("/:id/start", post(start))
        .route("/:id/stop", post(stop))
        .route("/:id/restart", post(restart))
        .route("/:id/exposure", axum::routing::put(expose).delete(close))
}

#[derive(Serialize)]
struct EngineInfo {
    engine: ManagedDatabaseEngine,
    versions: &'static [&'static str],
    default_version: &'static str,
    container_port: u16,
    has_database_and_user: bool,
}

async fn engines(_: Authenticated) -> Json<Vec<EngineInfo>> {
    Json(
        ManagedDatabaseEngine::ALL
            .into_iter()
            .map(|engine| EngineInfo {
                engine,
                versions: engine.versions(),
                default_version: engine.default_version(),
                container_port: engine.container_port(),
                has_database_and_user: engine.has_database_and_user(),
            })
            .collect(),
    )
}

#[derive(Serialize)]
struct DatabaseServiceView {
    project: ComposeProject,
    spec: DatabaseServiceSpec,
    /// Host port on 127.0.0.1 the database listens on.
    loopback_port: u16,
    exposure: Option<DatabaseExposure>,
}

fn view(project: ComposeProject, row: &DatabaseServiceRow) -> ApiResult<DatabaseServiceView> {
    let ComposeSource::Database(spec) = &project.source else {
        return Err(ApiError::NotFound("database service not found".into()));
    };
    let spec = (**spec).clone();
    let exposure = row.public_port.map(|port| DatabaseExposure {
        public_port: port as u16,
        allow_from: row.allow_from.clone(),
    });
    Ok(DatabaseServiceView {
        project,
        spec,
        loopback_port: row.loopback_port as u16,
        exposure,
    })
}

async fn find(state: &AppState, id: Uuid) -> ApiResult<(ComposeProject, DatabaseServiceRow)> {
    let not_found = || ApiError::NotFound("database service not found".into());
    let project = db::compose_projects::find_by_id(&state.db, id)
        .await?
        .ok_or_else(not_found)?;
    if !matches!(project.source, ComposeSource::Database(_)) {
        return Err(not_found());
    }
    let row = db::database_services::find(&state.db, id)
        .await?
        .ok_or_else(not_found)?;
    Ok((project, row))
}

async fn list(
    Authenticated(principal): Authenticated,
    State(state): State<AppState>,
) -> ApiResult<Json<Vec<DatabaseServiceView>>> {
    let rows = db::database_services::list(&state.db).await?;
    let mut views = Vec::with_capacity(rows.len());
    for row in rows {
        // Only the services whose environment grants `services:read`.
        let Some(service) = crate::auth::find_service_ref(&state.db, row.project_id).await? else {
            continue;
        };
        if !principal.allows(Permission::ServicesRead, service.target()) {
            continue;
        }
        if let Some(project) = db::compose_projects::find_by_id(&state.db, row.project_id).await? {
            if let Ok(v) = view(project, &row) {
                views.push(v);
            }
        }
    }
    Ok(Json(views))
}

async fn show(
    _: ServiceAccess<perm::ServicesRead>,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<DatabaseServiceView>> {
    let (project, row) = find(&state, id).await?;
    Ok(Json(view(project, &row)?))
}

#[derive(Deserialize)]
struct CreateBody {
    name: String,
    slug: String,
    engine: ManagedDatabaseEngine,
    /// Defaults to the engine's newest supported version.
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    database_name: Option<String>,
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    memory_limit_mb: Option<u32>,
    #[serde(default)]
    cpu_limit_millis: Option<u32>,
    /// Environment (of a project) the service belongs to; the owner's
    /// default `production` environment when omitted.
    #[serde(default)]
    environment_id: Option<Uuid>,
    /// Server to run on; this host when omitted, unless it is draining or
    /// full (then, as with `auto_place`, the least loaded eligible server).
    #[serde(default)]
    server_id: Option<Uuid>,
    #[serde(default)]
    auto_place: bool,
    /// Queue the first deployment right away (default true).
    #[serde(default = "default_true")]
    deploy: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Serialize)]
struct Created {
    service: DatabaseServiceView,
    deployment: Option<DeploymentInfo>,
}

async fn create(
    Authenticated(user): Authenticated,
    State(state): State<AppState>,
    Json(body): Json<CreateBody>,
) -> ApiResult<(StatusCode, Json<Created>)> {
    if body.name.trim().is_empty() || body.name.len() > 100 {
        return Err(ApiError::BadRequest(
            "name must be 1..=100 characters".into(),
        ));
    }
    if !validate::is_valid_project_slug(&body.slug) {
        return Err(ApiError::BadRequest("invalid slug".into()));
    }
    let spec = DatabaseServiceSpec {
        engine: body.engine,
        version: body
            .version
            .unwrap_or_else(|| body.engine.default_version().into()),
        database_name: body.database_name.unwrap_or_else(|| "app".into()),
        username: body.username.unwrap_or_else(|| "app".into()),
        memory_limit_mb: body.memory_limit_mb,
        cpu_limit_millis: body.cpu_limit_millis,
    };
    spec.validate().map_err(ApiError::BadRequest)?;
    let target = match body.environment_id {
        Some(environment_id) => {
            let environment = db::projects::find_environment(&state.db, environment_id)
                .await?
                .ok_or_else(|| ApiError::BadRequest("environment not found".into()))?;
            Target::Environment {
                project: environment.project_id,
                environment: environment.id,
            }
        }
        None => Target::Global,
    };
    user.require(Permission::ServicesCreate, target)?;
    if body.deploy {
        user.require(Permission::ServicesDeploy, target)?;
    }
    let source = ComposeSource::Database(Box::new(spec));
    let server_id =
        crate::routes::compose_projects::place(&state, body.server_id, body.auto_place, &source)
            .await?;
    let project = db::compose_projects::create(
        &state.db,
        Uuid::new_v4(),
        body.name.trim(),
        &body.slug,
        user.id,
        body.environment_id,
        &source,
    )
    .await
    .map_err(|e| {
        let unique = e
            .downcast_ref::<sqlx::Error>()
            .and_then(|e| e.as_database_error())
            .is_some_and(|e| e.is_unique_violation());
        if unique {
            ApiError::Conflict("a service with this slug already exists".into())
        } else {
            ApiError::Internal(e)
        }
    })?;

    if server_id != db::servers::LOCAL_SERVER_ID {
        if let Err(error) = db::servers::place_service(&state.db, project.id, server_id).await {
            db::compose_projects::delete(&state.db, project.id)
                .await
                .ok();
            return Err(error.into());
        }
    }
    let generated = credentials::generate(body.engine);
    let sealed = credentials::seal(&state.secret_box, project.id, &generated);
    drop(generated);
    let row = match db::database_services::create(&state.db, project.id, &sealed).await {
        Ok(Some(row)) => row,
        failed => {
            db::compose_projects::delete(&state.db, project.id)
                .await
                .ok();
            return Err(match failed {
                Err(error) => error.into(),
                _ => ApiError::Conflict("no free loopback port for database services".into()),
            });
        }
    };
    let deployment = if body.deploy {
        Some(
            state
                .deploy_queue
                .enqueue(&state.db, project.id, &project.slug, true, "manual")
                .await?,
        )
    } else {
        None
    };
    db::audit::record(
        &state.db,
        Some(user.id),
        user.actor_kind(),
        "create_database_service",
        Some(&project.slug),
        serde_json::json!({
            "project_id": project.id,
            "engine": body.engine,
            "loopback_port": row.loopback_port,
        }),
    )
    .await
    .ok();
    let project = db::compose_projects::find_by_id(&state.db, project.id)
        .await?
        .ok_or_else(|| ApiError::NotFound("database service not found".into()))?;
    Ok((
        StatusCode::ACCEPTED,
        Json(Created {
            service: view(project, &row)?,
            deployment,
        }),
    ))
}

#[derive(Serialize)]
struct CredentialsView {
    username: String,
    password: String,
    /// MySQL and MariaDB only.
    root_password: Option<String>,
    database: Option<String>,
    host: &'static str,
    port: u16,
    /// For processes on this server (`127.0.0.1`).
    uri: String,
    /// When outside access is open: the URI with `<server-address>` as the
    /// host placeholder and the public port.
    external_uri: Option<String>,
}

/// Returns the credentials (audited, never cached). POST rather than GET so
/// a link, prefetch, or crawler cannot trigger it.
async fn reveal_credentials(
    ServiceAccess {
        principal: user, ..
    }: ServiceAccess<perm::VariablesUpdate>,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<impl IntoResponse> {
    // The one place a stored secret is shown: an interactive session only.
    if user.api_key_id.is_some() {
        return Err(ApiError::Forbidden);
    }
    let (project, row) = find(&state, id).await?;
    let ComposeSource::Database(spec) = &project.source else {
        return Err(ApiError::NotFound("database service not found".into()));
    };
    let creds = credentials::open(&state.secret_box, &row)?;
    let port = row.loopback_port as u16;
    let body = CredentialsView {
        username: spec.login_user().into(),
        password: creds.password.clone(),
        root_password: creds.root_password.clone(),
        database: spec
            .engine
            .has_database_and_user()
            .then(|| spec.database_name.clone()),
        host: "127.0.0.1",
        port,
        uri: spec.connection_uri(&creds, "127.0.0.1", port),
        external_uri: row
            .public_port
            .map(|p| spec.connection_uri(&creds, "<server-address>", p as u16)),
    };
    db::audit::record(
        &state.db,
        Some(user.id),
        user.actor_kind(),
        "reveal_database_credentials",
        Some(&project.slug),
        serde_json::json!({ "project_id": project.id }),
    )
    .await
    .ok();
    Ok((
        [
            (header::CACHE_CONTROL, "no-store"),
            (header::PRAGMA, "no-cache"),
        ],
        Json(body),
    ))
}

async fn start(
    ServiceAccess {
        principal: user, ..
    }: ServiceAccess<perm::ServicesDeploy>,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<(StatusCode, Json<DeploymentInfo>)> {
    let (project, _) = find(&state, id).await?;
    // `docker compose up -d` starts stopped containers and recreates
    // missing ones, so starting is a deployment of the current settings.
    let deployment = state
        .deploy_queue
        .enqueue(&state.db, project.id, &project.slug, false, "manual")
        .await?;
    audit(&state, &user, "start_database_service", &project).await;
    Ok((StatusCode::ACCEPTED, Json(deployment)))
}

async fn stop(
    ServiceAccess {
        principal: user, ..
    }: ServiceAccess<perm::ServicesDeploy>,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<serde_json::Value>> {
    let (project, _) = find(&state, id).await?;
    refuse_while_running(&project)?;
    state
        .agents
        .for_server_id(&state.db, project.server_id)
        .await?
        .stop_compose_project(ComposeProjectActionParams {
            project_slug: project.slug.clone(),
        })
        .await?;
    db::compose_projects::set_status(&state.db, id, DeploymentStatus::Stopped).await?;
    audit(&state, &user, "stop_database_service", &project).await;
    Ok(Json(serde_json::json!({ "stopped": true })))
}

async fn restart(
    ServiceAccess {
        principal: user, ..
    }: ServiceAccess<perm::ServicesDeploy>,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<serde_json::Value>> {
    let (project, _) = find(&state, id).await?;
    refuse_while_running(&project)?;
    state
        .agents
        .for_server_id(&state.db, project.server_id)
        .await?
        .restart_compose_project(ComposeProjectActionParams {
            project_slug: project.slug.clone(),
        })
        .await
        .map_err(|e| match e {
            CoreError::NotFound(_) => {
                ApiError::Conflict("the service has not been deployed yet; start it".into())
            }
            other => other.into(),
        })?;
    if project.status == DeploymentStatus::Stopped {
        db::compose_projects::set_status(&state.db, id, DeploymentStatus::Succeeded).await?;
    }
    audit(&state, &user, "restart_database_service", &project).await;
    Ok(Json(serde_json::json!({ "restarted": true })))
}

async fn remove(
    ServiceAccess {
        principal: user, ..
    }: ServiceAccess<perm::ServicesDelete>,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<serde_json::Value>> {
    let (project, _) = find(&state, id).await?;
    refuse_while_running(&project)?;
    // Closes outside access, removes the containers, and deletes the
    // agent's files. The named data volume is kept (as for every service),
    // so data survives an accidental removal; remove it on the host.
    match state
        .agents
        .for_server_id(&state.db, project.server_id)
        .await?
        .remove_compose_project(RemoveComposeProjectParams {
            project_slug: project.slug.clone(),
            expected_project_id: Some(project.id),
        })
        .await
    {
        Ok(()) | Err(CoreError::NotFound(_)) => {}
        Err(error) => return Err(error.into()),
    }
    db::compose_projects::delete(&state.db, id).await?;
    audit(&state, &user, "remove_database_service", &project).await;
    Ok(Json(serde_json::json!({ "deleted": true })))
}

/// Opens (or changes) outside access: a host Nginx stream proxy on
/// `public_port` to the loopback port, and UFW rules for `allow_from`.
async fn expose(
    ServiceAccess {
        principal: user, ..
    }: ServiceAccess<perm::ServicesUpdate>,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(mut exposure): Json<DatabaseExposure>,
) -> ApiResult<Json<DatabaseServiceView>> {
    // Outside access opens a port in the server's firewall.
    user.require(Permission::ServersManage, Target::Global)?;
    let (project, row) = find(&state, id).await?;
    exposure.normalize().map_err(ApiError::BadRequest)?;
    let port = i32::from(exposure.public_port);
    if db::database_services::public_port_taken(&state.db, port, id).await? {
        return Err(ApiError::Conflict(
            "another database service already uses this public port".into(),
        ));
    }
    apply_exposure(&state, &project, &row, Some(exposure.clone())).await?;
    if let Err(error) =
        db::database_services::set_exposure(&state.db, id, Some(port), &exposure.allow_from).await
    {
        // Keep the host matching the recorded state.
        let previous = row.public_port.map(|p| DatabaseExposure {
            public_port: p as u16,
            allow_from: row.allow_from.clone(),
        });
        apply_exposure(&state, &project, &row, previous).await.ok();
        return Err(
            if error
                .as_database_error()
                .is_some_and(|e| e.is_unique_violation())
            {
                ApiError::Conflict("another database service already uses this public port".into())
            } else {
                error.into()
            },
        );
    }
    db::audit::record(
        &state.db,
        Some(user.id),
        user.actor_kind(),
        "expose_database_service",
        Some(&project.slug),
        serde_json::json!({
            "project_id": project.id,
            "public_port": exposure.public_port,
            "allow_from": exposure.allow_from,
        }),
    )
    .await
    .ok();
    let (project, row) = find(&state, id).await?;
    Ok(Json(view(project, &row)?))
}

async fn close(
    ServiceAccess {
        principal: user, ..
    }: ServiceAccess<perm::ServicesUpdate>,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<DatabaseServiceView>> {
    let (project, row) = find(&state, id).await?;
    apply_exposure(&state, &project, &row, None).await?;
    db::database_services::set_exposure(&state.db, id, None, &[]).await?;
    audit(&state, &user, "close_database_service", &project).await;
    let (project, row) = find(&state, id).await?;
    Ok(Json(view(project, &row)?))
}

async fn apply_exposure(
    state: &AppState,
    project: &ComposeProject,
    row: &DatabaseServiceRow,
    exposure: Option<DatabaseExposure>,
) -> ApiResult<()> {
    state
        .agents
        .for_server_id(&state.db, project.server_id)
        .await?
        .set_database_exposure(SetDatabaseExposureParams {
            project_slug: project.slug.clone(),
            loopback_port: row.loopback_port as u16,
            exposure,
        })
        .await
        // Host-side failures (Nginx stream module missing, UFW error) are
        // operator-actionable and carry no secret, so show them.
        .map_err(|e| match e {
            CoreError::External(message) => ApiError::Conflict(message),
            other => other.into(),
        })
}

fn refuse_while_running(project: &ComposeProject) -> ApiResult<()> {
    if project.status == DeploymentStatus::Running {
        return Err(ApiError::Conflict(
            "a deployment is running; wait for it to finish".into(),
        ));
    }
    Ok(())
}

async fn audit(state: &AppState, principal: &Principal, action: &str, project: &ComposeProject) {
    db::audit::record(
        &state.db,
        Some(principal.id),
        principal.actor_kind(),
        action,
        Some(&project.slug),
        serde_json::json!({ "project_id": project.id }),
    )
    .await
    .ok();
}

#[cfg(test)]
mod tests {
    use crate::db::compose_projects;
    use crate::test_support;
    use axum::body::Body;
    use axum::http::{Method, Request, StatusCode};
    use panel_core::adapters::mock::MockAdapter;
    use panel_core::error::CoreError;
    use panel_core::types::{ComposeSource, DeploymentStatus};
    use serde_json::{json, Value};
    use std::sync::Arc;
    use tower::ServiceExt;

    async fn call(
        router: &axum::Router,
        cookie: &str,
        method: Method,
        uri: &str,
        body: Option<Value>,
    ) -> (StatusCode, String, Option<String>) {
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
        let cache = response
            .headers()
            .get("cache-control")
            .map(|v| v.to_str().unwrap().to_string());
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        (status, String::from_utf8(bytes.to_vec()).unwrap(), cache)
    }

    fn slug(prefix: &str) -> String {
        format!(
            "{prefix}{}",
            &uuid::Uuid::new_v4().simple().to_string()[..12]
        )
    }

    #[tokio::test]
    async fn every_engine_is_created_with_sealed_credentials_and_a_loopback_port() {
        let _serial = test_support::QUEUE_TESTS.lock().await;
        let pool = test_support::test_pool().await;
        let adapter = Arc::new(MockAdapter::new());
        let state = test_support::app_state(pool.clone(), adapter.clone());
        let admin = test_support::user(&pool, true).await;
        let cookie = test_support::session_cookie(&state, admin.id).await;
        let secrets = state.secret_box.clone();
        let router = crate::routes::build_router(state);

        let (status, body, _) = call(
            &router,
            &cookie,
            Method::GET,
            "/api/database-services/engines",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let engines: Vec<Value> = serde_json::from_str(&body).unwrap();
        assert_eq!(engines.len(), 5);

        let mut ports = std::collections::HashSet::new();
        for engine in ["postgres", "mysql", "mariadb", "mongo", "redis"] {
            let (status, body, _) = call(
                &router,
                &cookie,
                Method::POST,
                "/api/database-services",
                Some(json!({ "name": format!("{engine} db"), "slug": slug("db"),
                             "engine": engine, "database_name": "shop", "username": "shop" })),
            )
            .await;
            assert_eq!(status, StatusCode::ACCEPTED, "{engine}: {body}");
            let created: Value = serde_json::from_str(&body).unwrap();
            let service = &created["service"];
            assert_eq!(service["project"]["source"]["kind"], "database");
            assert_eq!(service["spec"]["engine"], engine);
            assert!(service["exposure"].is_null());
            let port = service["loopback_port"].as_u64().unwrap();
            assert!((25000..=29999).contains(&port));
            assert!(ports.insert(port), "ports must be unique");
            assert_eq!(created["deployment"]["status"], "queued");
            let id: uuid::Uuid = service["project"]["id"].as_str().unwrap().parse().unwrap();

            // Sealed at rest; never in list/show bodies or the audit trail.
            let row = crate::db::database_services::find(&pool, id)
                .await
                .unwrap()
                .unwrap();
            let creds = crate::services::database_services::open(&secrets, &row).unwrap();
            assert!(!String::from_utf8_lossy(&row.credentials_ciphertext).contains(&creds.password));
            for uri in [
                "/api/database-services".to_string(),
                format!("/api/database-services/{id}"),
                format!("/api/compose-projects/{id}"),
            ] {
                let (status, body, _) = call(&router, &cookie, Method::GET, &uri, None).await;
                assert_eq!(status, StatusCode::OK, "{uri}");
                assert!(!body.contains(&creds.password), "{uri} leaks the password");
            }

            // The worker sends the credentials with the deployment only.
            let mut job = None;
            while let Some(claimed) = compose_projects::claim_next_deployment(&pool)
                .await
                .unwrap()
            {
                if claimed.project_id == id {
                    job = Some(claimed);
                    break;
                }
                compose_projects::finish_deployment(
                    &pool,
                    claimed.id,
                    DeploymentStatus::Failed,
                    None,
                    None,
                )
                .await
                .unwrap();
            }
            let job = job.expect("queued deployment");
            let queue_row: String = sqlx::query_scalar(
                "SELECT row_to_json(d)::text FROM compose_deployments d WHERE id = $1",
            )
            .bind(job.id)
            .fetch_one(&pool)
            .await
            .unwrap();
            assert!(!queue_row.contains(&creds.password));
            crate::services::deploy_queue::run_job(
                &pool,
                adapter.as_ref(),
                &secrets,
                &test_support::github_apps(),
                job,
            )
            .await;
            let sent = adapter.deploys.lock().unwrap().last().cloned().unwrap();
            let database = sent
                .database
                .expect("database settings in the deploy request");
            assert_eq!(u64::from(database.loopback_port), port);
            assert_eq!(database.credentials, creds);
            assert!(matches!(sent.source, ComposeSource::Database(_)));
            // The request's Debug form (what a stray log line would show)
            // never contains the secret.
            assert!(!format!("{:?}", database).contains(&creds.password));

            // Credentials view: audited, no-store, engine-shaped URI.
            let (status, body, cache) = call(
                &router,
                &cookie,
                Method::POST,
                &format!("/api/database-services/{id}/credentials"),
                None,
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            assert_eq!(cache.as_deref(), Some("no-store"));
            let view: Value = serde_json::from_str(&body).unwrap();
            assert_eq!(view["password"], creds.password.as_str());
            assert_eq!(view["host"], "127.0.0.1");
            assert!(view["uri"]
                .as_str()
                .unwrap()
                .contains(&format!("@127.0.0.1:{port}")));
            assert_eq!(
                view["root_password"].is_string(),
                engine == "mysql" || engine == "mariadb"
            );
            assert!(view["external_uri"].is_null());

            let audit: Vec<String> = sqlx::query_scalar(
                "SELECT metadata::text FROM audit_log WHERE metadata->>'project_id' = $1",
            )
            .bind(id.to_string())
            .fetch_all(&pool)
            .await
            .unwrap();
            assert!(audit.len() >= 2);
            assert!(audit.iter().all(|m| !m.contains(&creds.password)));
        }
    }

    #[tokio::test]
    async fn hostile_input_is_refused_and_generic_endpoints_cannot_bypass() {
        let pool = test_support::test_pool().await;
        let adapter = Arc::new(MockAdapter::new());
        let state = test_support::app_state(pool.clone(), adapter.clone());
        let admin = test_support::user(&pool, true).await;
        let cookie = test_support::session_cookie(&state, admin.id).await;
        let router = crate::routes::build_router(state);

        for bad in [
            json!({ "name": "x", "slug": slug("db"), "engine": "oracle" }),
            json!({ "name": "x", "slug": slug("db"), "engine": "postgres", "version": "latest" }),
            json!({ "name": "x", "slug": slug("db"), "engine": "postgres", "database_name": "a;b" }),
            json!({ "name": "x", "slug": slug("db"), "engine": "mysql", "username": "root" }),
            json!({ "name": "x", "slug": "../etc", "engine": "redis" }),
            json!({ "name": "", "slug": slug("db"), "engine": "redis" }),
        ] {
            let (status, body, _) = call(
                &router,
                &cookie,
                Method::POST,
                "/api/database-services",
                Some(bad),
            )
            .await;
            assert!(status.is_client_error(), "{status}: {body}");
        }
        // A database source cannot be created without generated credentials.
        let (status, _, _) = call(
            &router,
            &cookie,
            Method::POST,
            "/api/compose-projects",
            Some(json!({ "name": "x", "slug": slug("db"),
                         "source": { "kind": "database", "engine": "postgres", "version": "17" } })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        let (status, body, _) = call(
            &router,
            &cookie,
            Method::POST,
            "/api/database-services",
            Some(
                json!({ "name": "cache", "slug": slug("db"), "engine": "redis", "deploy": false }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        let id = serde_json::from_str::<Value>(&body).unwrap()["service"]["project"]["id"]
            .as_str()
            .unwrap()
            .to_string();
        // No HTTP domains for databases.
        let (status, _, _) = call(
            &router,
            &cookie,
            Method::POST,
            &format!("/api/compose-projects/{id}/domains"),
            Some(
                json!({ "domain": "db.example.com", "service_name": "db", "container_port": 6379 }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        // Not a database service: 404 on database routes.
        let (status, body, _) = call(
            &router,
            &cookie,
            Method::POST,
            "/api/compose-projects",
            Some(json!({ "name": "web", "slug": slug("web"), "deploy": false,
                         "source": { "kind": "raw", "compose_yaml": "services:\n  web:\n    image: nginx" } })),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        let web = serde_json::from_str::<Value>(&body).unwrap()["project"]["id"]
            .as_str()
            .unwrap()
            .to_string();
        let (status, _, _) = call(
            &router,
            &cookie,
            Method::POST,
            &format!("/api/database-services/{web}/credentials"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        // Unauthenticated callers get nothing.
        let (status, _, _) = call(&router, "", Method::GET, "/api/database-services", None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let (status, _, _) = call(
            &router,
            "",
            Method::POST,
            &format!("/api/database-services/{id}/credentials"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn outside_access_is_explicit_validated_and_removable() {
        // It queues a start; keep queue-draining tests from claiming it.
        let _serial = test_support::QUEUE_TESTS.lock().await;
        let pool = test_support::test_pool().await;
        let adapter = Arc::new(MockAdapter::new());
        let state = test_support::app_state(pool.clone(), adapter.clone());
        let admin = test_support::user(&pool, true).await;
        let cookie = test_support::session_cookie(&state, admin.id).await;
        let router = crate::routes::build_router(state);
        let create =
            |s: String| json!({ "name": "pg", "slug": s, "engine": "postgres", "deploy": false });
        let mut ids = Vec::new();
        for _ in 0..2 {
            let (status, body, _) = call(
                &router,
                &cookie,
                Method::POST,
                "/api/database-services",
                Some(create(slug("pg"))),
            )
            .await;
            assert_eq!(status, StatusCode::ACCEPTED, "{body}");
            ids.push(
                serde_json::from_str::<Value>(&body).unwrap()["service"]["project"]["id"]
                    .as_str()
                    .unwrap()
                    .to_string(),
            );
        }
        let public_port = 10000 + (uuid::Uuid::new_v4().as_u128() % 15000) as u16;

        for bad in [
            json!({ "public_port": 22 }),
            json!({ "public_port": 8080 }),
            json!({ "public_port": 45000 }),
            json!({ "public_port": public_port, "allow_from": ["0.0.0.0/0; deny"] }),
            json!({ "public_port": public_port, "allow_from": ["10.0.0.0/40"] }),
        ] {
            let (status, body, _) = call(
                &router,
                &cookie,
                Method::PUT,
                &format!("/api/database-services/{}/exposure", ids[0]),
                Some(bad),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        }
        assert!(adapter.exposures.lock().unwrap().is_empty());

        let (status, body, _) = call(
            &router,
            &cookie,
            Method::PUT,
            &format!("/api/database-services/{}/exposure", ids[0]),
            Some(json!({ "public_port": public_port, "allow_from": [" 203.0.113.0/24 "] })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let view: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(view["exposure"]["public_port"], public_port);
        assert_eq!(view["exposure"]["allow_from"], json!(["203.0.113.0/24"]));
        {
            let sent = adapter.exposures.lock().unwrap();
            let last = sent.last().unwrap();
            assert_eq!(
                last.loopback_port,
                view["loopback_port"].as_u64().unwrap() as u16
            );
            assert_eq!(last.exposure.as_ref().unwrap().public_port, public_port);
        }
        // The same public port cannot be taken twice.
        let (status, _, _) = call(
            &router,
            &cookie,
            Method::PUT,
            &format!("/api/database-services/{}/exposure", ids[1]),
            Some(json!({ "public_port": public_port })),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        // Host failures are reported, and nothing is recorded.
        adapter.fail_next_call_with(
            "set_database_exposure",
            CoreError::External("the Nginx stream module is not configured".into()),
        );
        let (status, body, _) = call(
            &router,
            &cookie,
            Method::PUT,
            &format!("/api/database-services/{}/exposure", ids[1]),
            Some(json!({ "public_port": public_port + 1 })),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert!(body.contains("stream module"), "{body}");
        let (_, body, _) = call(
            &router,
            &cookie,
            Method::GET,
            &format!("/api/database-services/{}", ids[1]),
            None,
        )
        .await;
        assert!(serde_json::from_str::<Value>(&body).unwrap()["exposure"].is_null());

        let (status, body, _) = call(
            &router,
            &cookie,
            Method::DELETE,
            &format!("/api/database-services/{}/exposure", ids[0]),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(serde_json::from_str::<Value>(&body).unwrap()["exposure"].is_null());
        assert!(adapter
            .exposures
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .exposure
            .is_none());

        // Lifecycle goes through the agent; removal drops the row too.
        for (action, call_name) in [
            ("stop", "stop_compose_project"),
            ("restart", "restart_compose_project"),
        ] {
            let (status, body, _) = call(
                &router,
                &cookie,
                Method::POST,
                &format!("/api/database-services/{}/{action}", ids[0]),
                None,
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{action}: {body}");
            assert!(adapter.calls.lock().unwrap().iter().any(|c| c == call_name));
        }
        let (status, body, _) = call(
            &router,
            &cookie,
            Method::POST,
            &format!("/api/database-services/{}/start", ids[0]),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        for id in &ids {
            sqlx::query("UPDATE compose_deployments SET status = 'cancelled' WHERE project_id = $1::uuid AND status = 'queued'")
                .bind(id)
                .execute(&pool)
                .await
                .unwrap();
            let (status, body, _) = call(
                &router,
                &cookie,
                Method::DELETE,
                &format!("/api/database-services/{id}"),
                None,
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            let id: uuid::Uuid = id.parse().unwrap();
            assert!(crate::db::database_services::find(&pool, id)
                .await
                .unwrap()
                .is_none());
        }
    }
}
