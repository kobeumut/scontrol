//! The environment's internal database network
//! (`/api/compose-projects/:id/shared-network`).
//!
//! Every environment has one internal Docker bridge network,
//! `scontrol-env-<environment id>`, created by the agent on first use.
//! Database services always join it with their slug as DNS name. Other
//! container services join when this setting is on, so their containers
//! reach `<database slug>:<engine port>` without going through the host's
//! loopback port. Joining is opt-in because Docker also registers every
//! service name on the network: two joined projects that both define, say,
//! `redis` could resolve each other's container.
//!
//! The setting takes effect on the next deployment. Previews never join.

use crate::auth::{perm, ServiceAccess};
use crate::db;
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;
use axum::extract::{Path, State};
use axum::routing::get;
use axum::{Json, Router};
use panel_core::database_services::shared_network_name;
use panel_core::types::{ComposeProject, ComposeSource};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub fn router() -> Router<AppState> {
    Router::new().route("/:id/shared-network", get(show).put(update))
}

#[derive(Debug, Serialize)]
struct SharedNetworkView {
    /// Docker network name.
    network: String,
    /// Whether the next deployment attaches this service's containers.
    joined: bool,
    /// Database services always join; the setting cannot be changed.
    always: bool,
    /// Static sites run no container and cannot join.
    supported: bool,
    /// Database services of the same environment and server, with the
    /// address containers on the network reach them at.
    databases: Vec<NetworkDatabase>,
}

#[derive(Debug, Serialize)]
struct NetworkDatabase {
    id: Uuid,
    name: String,
    engine: &'static str,
    host: String,
    port: u16,
}

#[derive(Debug, Deserialize)]
struct UpdateBody {
    joined: bool,
}

async fn show(
    _: ServiceAccess<perm::ServicesRead>,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<SharedNetworkView>> {
    let project = find(&state, id).await?;
    view(&state, &project).await.map(Json)
}

async fn update(
    ServiceAccess {
        principal: user, ..
    }: ServiceAccess<perm::ServicesUpdate>,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(body): Json<UpdateBody>,
) -> ApiResult<Json<SharedNetworkView>> {
    let project = find(&state, id).await?;
    match project.source {
        ComposeSource::Static(_) => {
            return Err(ApiError::Conflict(
                "static sites run no container and cannot join a network".into(),
            ))
        }
        ComposeSource::Database(_) => {
            return Err(ApiError::Conflict(
                "database services always join their environment's network".into(),
            ))
        }
        _ => {}
    }
    db::compose_projects::set_joins_shared_network(&state.db, id, body.joined).await?;
    db::audit::record(
        &state.db,
        Some(user.id),
        user.actor_kind(),
        "set_compose_shared_network",
        Some(&project.slug),
        serde_json::json!({ "project_id": project.id, "joined": body.joined }),
    )
    .await
    .ok();
    view(&state, &project).await.map(Json)
}

async fn find(state: &AppState, id: Uuid) -> ApiResult<ComposeProject> {
    db::compose_projects::find_by_id(&state.db, id)
        .await?
        .ok_or_else(|| ApiError::NotFound("Compose project not found".into()))
}

async fn view(state: &AppState, project: &ComposeProject) -> ApiResult<SharedNetworkView> {
    let (always, supported) = match project.source {
        ComposeSource::Database(_) => (true, true),
        ComposeSource::Static(_) => (false, false),
        _ => (false, true),
    };
    let joined = always
        || (supported && db::compose_projects::joins_shared_network(&state.db, project.id).await?);
    let rows: Vec<(Uuid, String, String, serde_json::Value)> = sqlx::query_as(
        "SELECT id, name, slug, source FROM compose_projects
         WHERE environment_id = $1 AND server_id = $2 AND source->>'kind' = 'database'
         ORDER BY name",
    )
    .bind(project.environment_id)
    .bind(project.server_id)
    .fetch_all(&state.db)
    .await?;
    let databases = rows
        .into_iter()
        .filter_map(|(id, name, slug, source)| {
            let ComposeSource::Database(spec) = serde_json::from_value(source).ok()? else {
                return None;
            };
            Some(NetworkDatabase {
                id,
                name,
                engine: spec.engine.as_str(),
                host: slug,
                port: spec.engine.container_port(),
            })
        })
        .collect();
    Ok(SharedNetworkView {
        network: shared_network_name(project.environment_id),
        joined,
        always,
        supported,
        databases,
    })
}

#[cfg(test)]
mod tests {
    use crate::test_support;
    use axum::body::Body;
    use axum::http::{Method, Request, StatusCode};
    use panel_core::adapters::mock::MockAdapter;
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
    async fn services_opt_in_and_see_their_environments_databases() {
        let pool = test_support::test_pool().await;
        let state = test_support::app_state(pool.clone(), Arc::new(MockAdapter::new()));
        let admin = test_support::user(&pool, true).await;
        let plain = test_support::user(&pool, false).await;
        let cookie = test_support::session_cookie(&state, admin.id).await;
        let plain_cookie = test_support::session_cookie(&state, plain.id).await;
        let router = crate::routes::build_router(state);

        let (app, _) = test_support::compose_project(&pool).await;
        let environment: Uuid =
            sqlx::query_scalar("SELECT environment_id FROM compose_projects WHERE id = $1")
                .bind(app)
                .fetch_one(&pool)
                .await
                .unwrap();
        let owner: Uuid =
            sqlx::query_scalar("SELECT owner_user_id FROM compose_projects WHERE id = $1")
                .bind(app)
                .fetch_one(&pool)
                .await
                .unwrap();
        // A database service in the same environment, and a static site.
        let (database, static_site) = (Uuid::new_v4(), Uuid::new_v4());
        let db_slug = format!("pg-{}", &Uuid::new_v4().simple().to_string()[..8]);
        for (id, slug, source) in [
            (
                database,
                db_slug.clone(),
                json!({ "kind": "database", "engine": "postgres", "version": "17" }),
            ),
            (
                static_site,
                format!("st-{}", &Uuid::new_v4().simple().to_string()[..8]),
                json!({ "kind": "static", "repository_url": "https://github.com/acme/site.git",
                        "branch": "main", "publish_path": "dist" }),
            ),
        ] {
            sqlx::query(
                "INSERT INTO compose_projects (id, name, slug, owner_user_id, source, environment_id)
                 VALUES ($1, $2, $2, $3, $4, $5)",
            )
            .bind(id)
            .bind(&slug)
            .bind(owner)
            .bind(source)
            .bind(environment)
            .execute(&pool)
            .await
            .unwrap();
        }
        let network = format!("scontrol-env-{}", environment.simple());

        let uri = format!("/api/compose-projects/{app}/shared-network");
        let (status, shown) = call(&router, &cookie, Method::GET, &uri, None).await;
        assert_eq!(status, StatusCode::OK, "{shown}");
        assert_eq!(shown["network"], network);
        assert_eq!(shown["joined"], false);
        assert_eq!(shown["always"], false);
        assert_eq!(
            shown["databases"],
            json!([{ "id": database, "name": db_slug, "engine": "postgres",
                     "host": db_slug, "port": 5432 }])
        );

        // Changing it needs services:update on the service.
        let (status, _) = call(
            &router,
            &plain_cookie,
            Method::PUT,
            &uri,
            Some(json!({ "joined": true })),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, saved) = call(
            &router,
            &cookie,
            Method::PUT,
            &uri,
            Some(json!({ "joined": true })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{saved}");
        assert_eq!(saved["joined"], true);
        assert!(
            crate::db::compose_projects::joins_shared_network(&pool, app)
                .await
                .unwrap()
        );

        // Database services always join; static sites never can.
        let (_, shown) = call(
            &router,
            &cookie,
            Method::GET,
            &format!("/api/compose-projects/{database}/shared-network"),
            None,
        )
        .await;
        assert_eq!(shown["joined"], true);
        assert_eq!(shown["always"], true);
        for id in [database, static_site] {
            let (status, _) = call(
                &router,
                &cookie,
                Method::PUT,
                &format!("/api/compose-projects/{id}/shared-network"),
                Some(json!({ "joined": false })),
            )
            .await;
            assert_eq!(status, StatusCode::CONFLICT);
        }

        for id in [app, database, static_site] {
            crate::db::compose_projects::delete(&pool, id)
                .await
                .unwrap();
        }
    }
}
