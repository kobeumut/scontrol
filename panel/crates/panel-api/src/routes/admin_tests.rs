//! Integration tests of deleting panel users and firewall rules through the
//! real router against PostgreSQL.

use crate::db;
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

async fn audit_count(pool: &sqlx::PgPool, action: &str, target: &str) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM audit_log WHERE action = $1 AND target = $2")
        .bind(action)
        .bind(target)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn admin_deletes_a_user_with_its_sessions_and_an_audit_entry() {
    let pool = test_support::test_pool().await;
    let state = test_support::app_state(pool.clone(), Arc::new(MockAdapter::new()));
    let router = crate::routes::build_router(state.clone());
    let admin = test_support::user(&pool, true).await;
    let cookie = test_support::session_cookie(&state, admin.id).await;
    let target = test_support::user(&pool, false).await;
    let target_cookie = test_support::session_cookie(&state, target.id).await;

    let uri = format!("/api/admin/users/{}", target.id);
    let (status, body) = call(&router, &cookie, Method::DELETE, &uri, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(db::users::find_by_id(&pool, target.id)
        .await
        .unwrap()
        .is_none());
    assert_eq!(
        audit_count(&pool, "delete_user", &target.id.to_string()).await,
        1
    );

    // The deleted user's session no longer authenticates.
    let (status, _) = call(&router, &target_cookie, Method::GET, "/api/auth/me", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Deleting again is a 404.
    let (status, _) = call(&router, &cookie, Method::DELETE, &uri, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn admin_cannot_delete_themselves() {
    let pool = test_support::test_pool().await;
    let state = test_support::app_state(pool.clone(), Arc::new(MockAdapter::new()));
    let router = crate::routes::build_router(state.clone());
    let admin = test_support::user(&pool, true).await;
    let cookie = test_support::session_cookie(&state, admin.id).await;

    let uri = format!("/api/admin/users/{}", admin.id);
    let (status, _) = call(&router, &cookie, Method::DELETE, &uri, None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(db::users::find_by_id(&pool, admin.id)
        .await
        .unwrap()
        .is_some());
}

/// A site (with a cron job and a host database) and a Compose service owned
/// by `owner`; returns `(site id, service id)`.
async fn owned_resources(pool: &sqlx::PgPool, owner: &db::users::UserRow) -> (Uuid, Uuid) {
    let site = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO sites (id, domain, owner_user_id, owner_username, runtime, document_root, status)
         VALUES ($1, $2, $3, $4, '{\"kind\":\"static\"}', '/srv/x', 'active')",
    )
    .bind(site)
    .bind(format!("{}.example.com", Uuid::new_v4().simple()))
    .bind(owner.id)
    .bind(&owner.username)
    .execute(pool)
    .await
    .unwrap();
    db::cron::create(pool, site, "0 3 * * *", "/usr/bin/true")
        .await
        .unwrap();
    let suffix = &Uuid::new_v4().simple().to_string()[..12];
    sqlx::query(
        "INSERT INTO databases (site_id, engine, name, username) VALUES ($1, 'postgres', $2, $2)",
    )
    .bind(site)
    .bind(format!("d{suffix}"))
    .execute(pool)
    .await
    .unwrap();
    let (service, _) = test_support::compose_project(pool).await;
    sqlx::query("UPDATE compose_projects SET owner_user_id = $1 WHERE id = $2")
        .bind(owner.id)
        .bind(service)
        .execute(pool)
        .await
        .unwrap();
    (site, service)
}

async fn exists(pool: &sqlx::PgPool, table: &str, id: Uuid) -> bool {
    sqlx::query_scalar::<_, bool>(&format!(
        "SELECT EXISTS (SELECT 1 FROM {table} WHERE id = $1)"
    ))
    .bind(id)
    .fetch_one(pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn a_user_who_owns_resources_is_kept_unless_they_are_deleted_too() {
    let pool = test_support::test_pool().await;
    let local = Arc::new(MockAdapter::new());
    let state = test_support::app_state(pool.clone(), local.clone());
    let router = crate::routes::build_router(state.clone());
    let admin = test_support::user(&pool, true).await;
    let cookie = test_support::session_cookie(&state, admin.id).await;
    let owner = test_support::user(&pool, false).await;
    let (site, service) = owned_resources(&pool, &owner).await;
    let uri = format!("/api/admin/users/{}", owner.id);

    // Without the flag nothing is touched.
    let (status, body) = call(&router, &cookie, Method::DELETE, &uri, None).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(
        body.to_string().contains("1 site(s) and 1 service(s)"),
        "{body}"
    );
    assert!(local.calls.lock().unwrap().is_empty());
    assert!(exists(&pool, "sites", site).await && exists(&pool, "compose_projects", service).await);

    // With it, each resource is torn down on the host, then the user goes.
    let (status, body) = call(
        &router,
        &cookie,
        Method::DELETE,
        &format!("{uri}?delete_resources=true"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["removed"]["sites"].as_array().unwrap().len(), 1);
    assert_eq!(body["removed"]["services"].as_array().unwrap().len(), 1);
    assert_eq!(body["removed"]["cron_jobs"], 1);
    assert_eq!(body["removed"]["databases"], 1);
    let calls = local.calls.lock().unwrap().clone();
    for call in [
        "remove_compose_project",
        "remove_cron_job",
        "delete_database",
        "delete_site",
    ] {
        assert!(
            calls.iter().any(|c| c == call),
            "{call} missing from {calls:?}"
        );
    }
    assert!(!exists(&pool, "sites", site).await);
    assert!(!exists(&pool, "compose_projects", service).await);
    let databases: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM databases WHERE site_id = $1")
        .bind(site)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(databases, 0);
    assert!(db::users::find_by_id(&pool, owner.id)
        .await
        .unwrap()
        .is_none());
    assert_eq!(
        audit_count(&pool, "delete_user", &owner.id.to_string()).await,
        1
    );
}

#[tokio::test]
async fn a_failed_teardown_keeps_the_user_and_a_retry_finishes() {
    let pool = test_support::test_pool().await;
    let local = Arc::new(MockAdapter::new());
    let state = test_support::app_state(pool.clone(), local.clone());
    let router = crate::routes::build_router(state.clone());
    let admin = test_support::user(&pool, true).await;
    let cookie = test_support::session_cookie(&state, admin.id).await;
    let owner = test_support::user(&pool, false).await;
    let (site, service) = owned_resources(&pool, &owner).await;
    let uri = format!("/api/admin/users/{}?delete_resources=true", owner.id);

    // The host refuses the site: the service is already gone, the site and
    // the user stay, and nothing is orphaned.
    local.fail_next_call("delete_site");
    let (status, _) = call(&router, &cookie, Method::DELETE, &uri, None).await;
    assert!(!status.is_success());
    assert!(!exists(&pool, "compose_projects", service).await);
    assert!(exists(&pool, "sites", site).await);
    assert!(db::users::find_by_id(&pool, owner.id)
        .await
        .unwrap()
        .is_some());

    let (status, body) = call(&router, &cookie, Method::DELETE, &uri, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(!exists(&pool, "sites", site).await);
    assert!(db::users::find_by_id(&pool, owner.id)
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn a_running_deployment_blocks_deleting_its_owner() {
    let pool = test_support::test_pool().await;
    let local = Arc::new(MockAdapter::new());
    let state = test_support::app_state(pool.clone(), local.clone());
    let router = crate::routes::build_router(state.clone());
    let admin = test_support::user(&pool, true).await;
    let cookie = test_support::session_cookie(&state, admin.id).await;
    let owner = test_support::user(&pool, false).await;
    let (site, service) = owned_resources(&pool, &owner).await;
    sqlx::query("UPDATE compose_projects SET status = 'running' WHERE id = $1")
        .bind(service)
        .execute(&pool)
        .await
        .unwrap();

    let uri = format!("/api/admin/users/{}?delete_resources=true", owner.id);
    let (status, body) = call(&router, &cookie, Method::DELETE, &uri, None).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(local.calls.lock().unwrap().is_empty());
    assert!(exists(&pool, "sites", site).await && exists(&pool, "compose_projects", service).await);
    assert!(db::users::find_by_id(&pool, owner.id)
        .await
        .unwrap()
        .is_some());

    // Leave nothing behind in the shared database.
    sqlx::query("UPDATE compose_projects SET status = 'succeeded' WHERE id = $1")
        .bind(service)
        .execute(&pool)
        .await
        .unwrap();
    let (status, _) = call(&router, &cookie, Method::DELETE, &uri, None).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn a_non_admin_cannot_delete_users() {
    let pool = test_support::test_pool().await;
    let state = test_support::app_state(pool.clone(), Arc::new(MockAdapter::new()));
    let router = crate::routes::build_router(state.clone());
    let user = test_support::user(&pool, false).await;
    let cookie = test_support::session_cookie(&state, user.id).await;
    let other = test_support::user(&pool, false).await;

    let uri = format!("/api/admin/users/{}", other.id);
    let (status, _) = call(&router, &cookie, Method::DELETE, &uri, None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(db::users::find_by_id(&pool, other.id)
        .await
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn removing_a_firewall_rule_deletes_it_from_ufw_then_the_panel() {
    let pool = test_support::test_pool().await;
    let local = Arc::new(MockAdapter::new());
    let state = test_support::app_state(pool.clone(), local.clone());
    let router = crate::routes::build_router(state.clone());
    let admin = test_support::user(&pool, true).await;
    let cookie = test_support::session_cookie(&state, admin.id).await;
    // A port no other test uses, so the shared table never collides.
    let port = 20000 + (Uuid::new_v4().as_u128() % 40000) as u16;

    let (status, rule) = call(
        &router,
        &cookie,
        Method::POST,
        "/api/firewall",
        Some(json!({ "port": port, "protocol": "udp", "allow": false })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{rule}");
    let uri = format!("/api/firewall/{}", rule["id"].as_str().unwrap());

    // The agent refusing keeps the rule listed: UFW still enforces it.
    local.fail_next_call("remove_firewall_rule");
    let (status, _) = call(&router, &cookie, Method::DELETE, &uri, None).await;
    assert!(status.is_server_error() || status.is_client_error());
    assert!(db::firewall::list_all(&pool)
        .await
        .unwrap()
        .iter()
        .any(|r| r.port == port));

    local.calls.lock().unwrap().clear();
    let (status, body) = call(&router, &cookie, Method::DELETE, &uri, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(*local.calls.lock().unwrap(), ["remove_firewall_rule"]);
    assert!(!db::firewall::list_all(&pool)
        .await
        .unwrap()
        .iter()
        .any(|r| r.port == port));
    assert_eq!(
        audit_count(&pool, "remove_firewall_rule", &port.to_string()).await,
        1
    );

    let (status, _) = call(&router, &cookie, Method::DELETE, &uri, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
