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

#[tokio::test]
async fn a_user_who_owns_a_site_is_not_deleted() {
    let pool = test_support::test_pool().await;
    let state = test_support::app_state(pool.clone(), Arc::new(MockAdapter::new()));
    let router = crate::routes::build_router(state.clone());
    let admin = test_support::user(&pool, true).await;
    let cookie = test_support::session_cookie(&state, admin.id).await;
    let owner = test_support::user(&pool, false).await;
    sqlx::query(
        "INSERT INTO sites (id, domain, owner_user_id, owner_username, runtime, document_root, status)
         VALUES ($1, $2, $3, $4, '{\"kind\":\"static\"}', '/srv/x', 'active')",
    )
    .bind(Uuid::new_v4())
    .bind(format!("{}.example.com", Uuid::new_v4().simple()))
    .bind(owner.id)
    .bind(&owner.username)
    .execute(&pool)
    .await
    .unwrap();

    let uri = format!("/api/admin/users/{}", owner.id);
    let (status, body) = call(&router, &cookie, Method::DELETE, &uri, None).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(db::users::find_by_id(&pool, owner.id)
        .await
        .unwrap()
        .is_some());
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
