//! Integration tests of the RBAC extractors, API keys, and service placement
//! through the real router against PostgreSQL.

use crate::db;
use crate::state::AppState;
use crate::test_support;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use panel_core::adapters::mock::MockAdapter;
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;
use uuid::Uuid;

#[path = "rbac_stage4_tests.rs"]
mod stage4;

enum Auth<'a> {
    Cookie(&'a str),
    Bearer(&'a str),
    None,
}

async fn call(
    router: &axum::Router,
    auth: Auth<'_>,
    method: Method,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut request = Request::builder().method(method).uri(uri);
    request = match auth {
        Auth::Cookie(cookie) => request.header("cookie", cookie),
        Auth::Bearer(token) => request.header("authorization", format!("Bearer {token}")),
        Auth::None => request,
    };
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
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

struct World {
    state: AppState,
    router: axum::Router,
    local: Arc<MockAdapter>,
    /// Project P with environments `production` and `staging`, one service
    /// in each; project Q with one service.
    p: Uuid,
    p_prod: Uuid,
    p_staging: Uuid,
    svc_prod: Uuid,
    svc_staging: Uuid,
    q: Uuid,
    svc_q: Uuid,
}

async fn project(pool: &sqlx::PgPool, owner: Uuid) -> (Uuid, Uuid, Uuid) {
    let (id, prod, staging) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    let slug = format!("p{}", &Uuid::new_v4().simple().to_string()[..10]);
    sqlx::query("INSERT INTO projects (id, name, slug, owner_user_id) VALUES ($1, $2, $2, $3)")
        .bind(id)
        .bind(&slug)
        .bind(owner)
        .execute(pool)
        .await
        .unwrap();
    for (env, name) in [(prod, "production"), (staging, "staging")] {
        sqlx::query(
            "INSERT INTO environments (id, project_id, name, slug) VALUES ($1, $2, $3, $3)",
        )
        .bind(env)
        .bind(id)
        .bind(name)
        .execute(pool)
        .await
        .unwrap();
    }
    (id, prod, staging)
}

async fn service(pool: &sqlx::PgPool, owner: Uuid, environment: Uuid) -> Uuid {
    let id = Uuid::new_v4();
    let slug = format!("s{}", &Uuid::new_v4().simple().to_string()[..10]);
    sqlx::query(
        "INSERT INTO compose_projects (id, name, slug, owner_user_id, source, environment_id) \
         VALUES ($1, $2, $2, $3, '{\"kind\":\"raw\",\"compose_yaml\":\"services: {}\"}', $4)",
    )
    .bind(id)
    .bind(&slug)
    .bind(owner)
    .bind(environment)
    .execute(pool)
    .await
    .unwrap();
    id
}

async fn world() -> World {
    let pool = test_support::test_pool().await;
    let local = Arc::new(MockAdapter::new());
    let state = test_support::app_state(pool.clone(), local.clone());
    let owner = test_support::user(&pool, true).await;
    let (p, p_prod, p_staging) = project(&pool, owner.id).await;
    let (q, q_prod, _) = project(&pool, owner.id).await;
    World {
        svc_prod: service(&pool, owner.id, p_prod).await,
        svc_staging: service(&pool, owner.id, p_staging).await,
        svc_q: service(&pool, owner.id, q_prod).await,
        router: crate::routes::build_router(state.clone()),
        state,
        local,
        p,
        p_prod,
        p_staging,
        q,
    }
}

async fn bind(state: &AppState, user: Uuid, role: &str, scope: panel_core::rbac::Scope) {
    let role = db::rbac::role_id_by_name(&state.db, role)
        .await
        .unwrap()
        .unwrap();
    db::rbac::create_binding(&state.db, user, role, scope, None)
        .await
        .unwrap();
}

fn ids(list: &Value) -> Vec<String> {
    list.as_array()
        .unwrap()
        .iter()
        .map(|v| v["id"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn a_user_without_bindings_gets_nothing() {
    let w = world().await;
    let user = test_support::user(&w.state.db, false).await;
    let cookie = test_support::session_cookie(&w.state, user.id).await;
    let auth = || Auth::Cookie(&cookie);

    let (status, list) = call(
        &w.router,
        auth(),
        Method::GET,
        "/api/compose-projects",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(ids(&list).is_empty());
    let (_, list) = call(&w.router, auth(), Method::GET, "/api/projects", None).await;
    assert!(ids(&list).is_empty());
    for (method, uri) in [
        (Method::GET, format!("/api/compose-projects/{}", w.svc_prod)),
        (
            Method::POST,
            format!("/api/compose-projects/{}/deploy", w.svc_prod),
        ),
        (
            Method::DELETE,
            format!("/api/compose-projects/{}", w.svc_prod),
        ),
        (Method::GET, format!("/api/projects/{}", w.p)),
        (Method::GET, "/api/registries".to_string()),
        (Method::GET, "/api/servers".to_string()),
        (Method::GET, "/api/access/bindings".to_string()),
        (Method::POST, "/api/projects".to_string()),
    ] {
        let (status, _) = call(&w.router, auth(), method.clone(), &uri, Some(json!({}))).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {uri}");
    }
    // Unknown resources are 404, unauthenticated requests 401.
    let (status, _) = call(
        &w.router,
        auth(),
        Method::GET,
        &format!("/api/compose-projects/{}", Uuid::new_v4()),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call(
        &w.router,
        Auth::None,
        Method::GET,
        &format!("/api/compose-projects/{}", w.svc_prod),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(w.local.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn project_bindings_cover_the_project_and_nothing_else() {
    let w = world().await;
    let user = test_support::user(&w.state.db, false).await;
    bind(
        &w.state,
        user.id,
        "deployer",
        panel_core::rbac::Scope::Project { project_id: w.p },
    )
    .await;
    let cookie = test_support::session_cookie(&w.state, user.id).await;
    let auth = || Auth::Cookie(&cookie);

    let (_, list) = call(
        &w.router,
        auth(),
        Method::GET,
        "/api/compose-projects",
        None,
    )
    .await;
    let mut visible = ids(&list);
    visible.sort();
    let mut expected = vec![w.svc_prod.to_string(), w.svc_staging.to_string()];
    expected.sort();
    assert_eq!(visible, expected);

    let (status, _) = call(
        &w.router,
        auth(),
        Method::POST,
        &format!("/api/compose-projects/{}/deploy", w.svc_prod),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let (status, _) = call(
        &w.router,
        auth(),
        Method::POST,
        &format!("/api/compose-projects/{}/stop", w.svc_staging),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        *w.local.calls.lock().unwrap(),
        vec!["stop_compose_project".to_string()]
    );

    // Not in the role: delete, variables, domains. Not in scope: project Q.
    for (method, uri, body) in [
        (
            Method::DELETE,
            format!("/api/compose-projects/{}", w.svc_prod),
            None,
        ),
        (
            Method::PUT,
            format!("/api/compose-projects/{}/variables", w.svc_prod),
            Some(json!({ "variables": [] })),
        ),
        (
            Method::GET,
            format!("/api/compose-projects/{}/variables", w.svc_prod),
            None,
        ),
        (
            Method::POST,
            format!("/api/compose-projects/{}/domains", w.svc_prod),
            Some(json!({})),
        ),
        (
            Method::POST,
            format!("/api/compose-projects/{}/deploy", w.svc_q),
            None,
        ),
        (
            Method::GET,
            format!("/api/compose-projects/{}", w.svc_q),
            None,
        ),
        (
            Method::PATCH,
            format!("/api/projects/{}", w.p),
            Some(json!({ "name": "x" })),
        ),
        (Method::GET, format!("/api/projects/{}", w.q), None),
    ] {
        let (status, _) = call(&w.router, auth(), method.clone(), &uri, body).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {uri}");
    }
    let (status, project) = call(
        &w.router,
        auth(),
        Method::GET,
        &format!("/api/projects/{}", w.p),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(project["environments"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn environment_bindings_do_not_reach_sibling_environments() {
    let w = world().await;
    let user = test_support::user(&w.state.db, false).await;
    bind(
        &w.state,
        user.id,
        "viewer",
        panel_core::rbac::Scope::Environment {
            project_id: w.p,
            environment_id: w.p_staging,
        },
    )
    .await;
    let cookie = test_support::session_cookie(&w.state, user.id).await;
    let auth = || Auth::Cookie(&cookie);

    let (status, _) = call(
        &w.router,
        auth(),
        Method::GET,
        &format!("/api/compose-projects/{}", w.svc_staging),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = call(
        &w.router,
        auth(),
        Method::GET,
        &format!("/api/compose-projects/{}/variables", w.svc_staging),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = call(
        &w.router,
        auth(),
        Method::GET,
        &format!("/api/compose-projects/{}", w.svc_prod),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = call(
        &w.router,
        auth(),
        Method::POST,
        &format!("/api/compose-projects/{}/deploy", w.svc_staging),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    // Environment routes check the environment; project-level ones refuse.
    let (status, _) = call(
        &w.router,
        auth(),
        Method::GET,
        &format!(
            "/api/projects/{}/environments/{}/variables",
            w.p, w.p_staging
        ),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = call(
        &w.router,
        auth(),
        Method::GET,
        &format!("/api/projects/{}/environments/{}/variables", w.p, w.p_prod),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = call(
        &w.router,
        auth(),
        Method::GET,
        &format!("/api/projects/{}/variables", w.p),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    // An environment of another project in the path is not found.
    let (status, _) = call(
        &w.router,
        auth(),
        Method::GET,
        &format!(
            "/api/projects/{}/environments/{}/variables",
            w.q, w.p_staging
        ),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // The project is listed with only the bound environment.
    let (_, list) = call(&w.router, auth(), Method::GET, "/api/projects", None).await;
    let projects = list.as_array().unwrap();
    assert_eq!(projects.len(), 1);
    assert_eq!(projects[0]["id"], w.p.to_string());
    let environments = projects[0]["environments"].as_array().unwrap();
    assert_eq!(environments.len(), 1);
    assert_eq!(environments[0]["id"], w.p_staging.to_string());
}

#[tokio::test]
async fn api_keys_are_scoped_hashed_and_revocable() {
    let w = world().await;
    let admin = test_support::user(&w.state.db, true).await;
    let cookie = test_support::session_cookie(&w.state, admin.id).await;

    let (status, created) = call(
        &w.router,
        Auth::Cookie(&cookie),
        Method::POST,
        "/api/api-keys",
        Some(json!({
            "name": "ci",
            "grants": [
                { "permission": "services:deploy", "scope": { "kind": "project", "project_id": w.p } },
                { "permission": "services:read", "scope": { "kind": "project", "project_id": w.p } },
            ],
            "expires_in_days": 30,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let token = created["token"].as_str().unwrap().to_string();
    let key_id = created["key"]["id"].as_str().unwrap().to_string();
    assert!(token.starts_with("sck_"));
    // Only a hash is stored; the listing never shows the token.
    let stored: String = sqlx::query_scalar("SELECT secret_hash FROM api_keys WHERE id = $1")
        .bind(Uuid::parse_str(&key_id).unwrap())
        .fetch_one(&w.state.db)
        .await
        .unwrap();
    assert!(stored.starts_with("$argon2id$"));
    let (_, secret) = crate::auth::api_keys::parse(&token).unwrap();
    assert!(!stored.contains(secret));
    let (_, keys) = call(
        &w.router,
        Auth::Cookie(&cookie),
        Method::GET,
        "/api/api-keys",
        None,
    )
    .await;
    assert!(!keys.to_string().contains(&token));

    let bearer = || Auth::Bearer(&token);
    let (status, _) = call(
        &w.router,
        bearer(),
        Method::POST,
        &format!("/api/compose-projects/{}/deploy", w.svc_prod),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let (_, list) = call(
        &w.router,
        bearer(),
        Method::GET,
        "/api/compose-projects",
        None,
    )
    .await;
    assert_eq!(
        ids(&list).len(),
        2,
        "only project P, although the owner is an admin"
    );
    for (method, uri) in [
        (
            Method::POST,
            format!("/api/compose-projects/{}/deploy", w.svc_q),
        ),
        (
            Method::DELETE,
            format!("/api/compose-projects/{}", w.svc_prod),
        ),
        (Method::GET, "/api/registries".to_string()),
        (Method::GET, "/api/api-keys".to_string()),
        (Method::POST, "/api/api-keys".to_string()),
        (Method::GET, "/api/auth/me".to_string()),
    ] {
        let (status, _) = call(&w.router, bearer(), method.clone(), &uri, Some(json!({}))).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {uri}");
    }

    // A wrong secret with the right prefix, and garbage, are 401.
    let forged = format!("{}{}", &token[..token.len() - 4], "AAAA");
    let (status, _) = call(
        &w.router,
        Auth::Bearer(&forged),
        Method::GET,
        "/api/compose-projects",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = call(
        &w.router,
        Auth::Bearer("nope"),
        Method::GET,
        "/api/compose-projects",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let (status, _) = call(
        &w.router,
        Auth::Cookie(&cookie),
        Method::DELETE,
        &format!("/api/api-keys/{key_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = call(
        &w.router,
        bearer(),
        Method::GET,
        "/api/compose-projects",
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "revocation applies at once, despite the cache"
    );
}

#[tokio::test]
async fn keys_never_exceed_their_owner() {
    let w = world().await;
    let user = test_support::user(&w.state.db, false).await;
    bind(
        &w.state,
        user.id,
        "deployer",
        panel_core::rbac::Scope::Project { project_id: w.p },
    )
    .await;
    let cookie = test_support::session_cookie(&w.state, user.id).await;

    for grants in [
        json!([{ "permission": "services:delete", "scope": { "kind": "project", "project_id": w.p } }]),
        json!([{ "permission": "services:deploy", "scope": { "kind": "global" } }]),
        json!([{ "permission": "services:deploy", "scope": { "kind": "project", "project_id": w.q } }]),
        json!([{ "permission": "system:admin", "scope": { "kind": "global" } }]),
        json!([{ "permission": "access:manage", "scope": { "kind": "global" } }]),
        json!([]),
    ] {
        let (status, body) = call(
            &w.router,
            Auth::Cookie(&cookie),
            Method::POST,
            "/api/api-keys",
            Some(json!({ "name": "x", "grants": grants })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{grants} → {body}");
    }

    let (status, created) = call(
        &w.router,
        Auth::Cookie(&cookie),
        Method::POST,
        "/api/api-keys",
        Some(json!({ "name": "deploy", "grants": [
            { "permission": "services:deploy", "scope": { "kind": "environment", "project_id": w.p, "environment_id": w.p_prod } },
        ] })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let token = created["token"].as_str().unwrap().to_string();
    let deploy = |id: Uuid| format!("/api/compose-projects/{id}/deploy");
    let (status, _) = call(
        &w.router,
        Auth::Bearer(&token),
        Method::POST,
        &deploy(w.svc_prod),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let (status, _) = call(
        &w.router,
        Auth::Bearer(&token),
        Method::POST,
        &deploy(w.svc_staging),
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "the key covers production only"
    );

    // Taking the owner's role away disables the key with it.
    sqlx::query("DELETE FROM role_bindings WHERE user_id = $1")
        .bind(user.id)
        .execute(&w.state.db)
        .await
        .unwrap();
    let (status, _) = call(
        &w.router,
        Auth::Bearer(&token),
        Method::POST,
        &deploy(w.svc_prod),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let audit: Vec<String> = sqlx::query_scalar(
        "SELECT actor_kind FROM audit_log WHERE actor_user_id = $1 AND action = 'deploy_compose_project'",
    )
    .bind(user.id)
    .fetch_all(&w.state.db)
    .await
    .unwrap();
    assert_eq!(audit, vec!["api_key".to_string()]);
}

#[tokio::test]
async fn bindings_need_access_manage_and_cannot_escalate() {
    let w = world().await;
    let admin = test_support::user(&w.state.db, true).await;
    let admin_cookie = test_support::session_cookie(&w.state, admin.id).await;
    let user = test_support::user(&w.state.db, false).await;
    let user_cookie = test_support::session_cookie(&w.state, user.id).await;
    let roles = db::rbac::list_roles(&w.state.db).await.unwrap();
    let role = |name: &str| roles.iter().find(|r| r.name == name).unwrap().id;

    // A custom role holding access:manage, granted globally by the admin.
    let (status, custom) = call(
        &w.router,
        Auth::Cookie(&admin_cookie),
        Method::POST,
        "/api/access/roles",
        Some(json!({ "name": format!("acl-{}", &Uuid::new_v4().simple().to_string()[..6]), "permissions": ["access:manage", "services:read"] })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{custom}");
    let (status, _) = call(
        &w.router,
        Auth::Cookie(&admin_cookie),
        Method::POST,
        "/api/access/bindings",
        Some(json!({ "user_id": user.id, "role_id": custom["id"], "scope": { "kind": "global" } })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // The user may now bind, but only what they hold themselves.
    let other = test_support::user(&w.state.db, false).await;
    let (status, _) = call(
        &w.router,
        Auth::Cookie(&user_cookie),
        Method::POST,
        "/api/access/bindings",
        Some(
            json!({ "user_id": other.id, "role_id": role("admin"), "scope": { "kind": "global" } }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = call(
        &w.router,
        Auth::Cookie(&user_cookie),
        Method::POST,
        "/api/access/bindings",
        Some(json!({ "user_id": user.id, "role_id": role("deployer"), "scope": { "kind": "project", "project_id": w.p } })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "no services:deploy to hand out"
    );
    let (status, _) = call(
        &w.router,
        Auth::Cookie(&user_cookie),
        Method::POST,
        "/api/access/roles",
        Some(json!({ "name": "sneaky", "permissions": ["system:admin"] })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Built-in roles are fixed; environments must belong to their project.
    let (status, _) = call(
        &w.router,
        Auth::Cookie(&admin_cookie),
        Method::DELETE,
        &format!("/api/access/roles/{}", role("viewer")),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, _) = call(
        &w.router,
        Auth::Cookie(&admin_cookie),
        Method::POST,
        "/api/access/bindings",
        Some(json!({ "user_id": other.id, "role_id": role("viewer"),
            "scope": { "kind": "environment", "project_id": w.q, "environment_id": w.p_prod } })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, body) = call(
        &w.router,
        Auth::Cookie(&admin_cookie),
        Method::POST,
        "/api/access/roles",
        Some(json!({ "name": "bad", "permissions": ["services:everything"] })),
    )
    .await;
    assert!(status.is_client_error(), "{status} {body}");
}

#[tokio::test]
async fn services_run_on_the_agent_of_their_server() {
    let w = world().await;
    let admin = test_support::user(&w.state.db, true).await;
    let cookie = test_support::session_cookie(&w.state, admin.id).await;
    let remote = Arc::new(MockAdapter::new());
    let name = format!("r-{}", &Uuid::new_v4().simple().to_string()[..8]);

    let (status, created) = call(
        &w.router,
        Auth::Cookie(&cookie),
        Method::POST,
        "/api/servers",
        Some(json!({ "name": name, "transport": "ssh_socket", "address": format!("/run/scontrol/remote/{name}.sock") })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let server_id = Uuid::parse_str(created["server"]["id"].as_str().unwrap()).unwrap();
    // The tunnel is not up: the server is offline and takes no services.
    assert_eq!(created["server"]["status"], "offline");
    let body = |server: Uuid| {
        json!({
            "name": "web", "slug": format!("w{}", &Uuid::new_v4().simple().to_string()[..10]),
            "source": { "kind": "raw", "compose_yaml": "services:\n  web:\n    image: nginx\n" },
            "environment_id": w.p_prod, "server_id": server, "deploy": false,
        })
    };
    let (status, _) = call(
        &w.router,
        Auth::Cookie(&cookie),
        Method::POST,
        "/api/compose-projects",
        Some(body(server_id)),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);

    w.state.agents.set_override(server_id, remote.clone());
    let (status, checked) = call(
        &w.router,
        Auth::Cookie(&cookie),
        Method::POST,
        &format!("/api/servers/{server_id}/check"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(checked["server"]["status"], "online", "{checked}");

    let (status, service) = call(
        &w.router,
        Auth::Cookie(&cookie),
        Method::POST,
        "/api/compose-projects",
        Some(body(server_id)),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{service}");
    assert_eq!(service["project"]["server_id"], server_id.to_string());
    let service_id = service["project"]["id"].as_str().unwrap().to_string();

    // Without a server the service stays on this host, even with an empty
    // remote server online: adding a server changes nothing for old clients.
    let mut unplaced = body(server_id);
    unplaced.as_object_mut().unwrap().remove("server_id");
    let (status, local_service) = call(
        &w.router,
        Auth::Cookie(&cookie),
        Method::POST,
        "/api/compose-projects",
        Some(unplaced),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(
        local_service["project"]["server_id"],
        db::servers::LOCAL_SERVER_ID.to_string()
    );

    w.local.calls.lock().unwrap().clear();
    let (status, _) = call(
        &w.router,
        Auth::Cookie(&cookie),
        Method::POST,
        &format!("/api/compose-projects/{service_id}/stop"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(remote
        .calls
        .lock()
        .unwrap()
        .contains(&"stop_compose_project".to_string()));
    assert!(
        w.local.calls.lock().unwrap().is_empty(),
        "the local agent is not touched"
    );

    // Scheduled jobs of the service live on its server too: installed by
    // its agent, and their runs collected from it.
    let (status, job) = call(&w.router, Auth::Cookie(&cookie), Method::POST, &format!("/api/compose-projects/{service_id}/jobs"), Some(json!({
        "name": "nightly", "service_name": "web", "command": ["php", "artisan", "schedule:run"],
        "schedule": { "kind": "cron", "expression": "0 3 * * *" }, "timezone": "UTC", "timeout_secs": 60,
    }))).await;
    assert_eq!(status, StatusCode::CREATED, "{job}");
    assert_eq!(remote.job_upserts.lock().unwrap().len(), 1);
    assert!(w.local.job_upserts.lock().unwrap().is_empty());
    let remote_agent: Arc<dyn panel_core::adapters::AdapterPort> = remote.clone();
    crate::services::scheduled_jobs::collect_server(
        &w.state.db,
        remote_agent.as_ref(),
        &w.state.notifier,
        server_id,
        None,
    )
    .await
    .unwrap();
    assert!(remote
        .calls
        .lock()
        .unwrap()
        .contains(&"list_scheduled_job_runs".to_string()));
    assert!(!w
        .local
        .calls
        .lock()
        .unwrap()
        .contains(&"list_scheduled_job_runs".to_string()));

    // A database service placed on the server is run by its agent.
    let (status, database) = call(&w.router, Auth::Cookie(&cookie), Method::POST, "/api/database-services", Some(json!({
        "name": "db", "slug": format!("d{}", &Uuid::new_v4().simple().to_string()[..10]),
        "engine": "postgres", "environment_id": w.p_prod, "server_id": server_id, "deploy": false,
    }))).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{database}");
    let database_id = database["service"]["project"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let (status, _) = call(
        &w.router,
        Auth::Cookie(&cookie),
        Method::POST,
        &format!("/api/database-services/{database_id}/stop"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        remote
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| *c == "stop_compose_project")
            .count(),
        2
    );
    let (status, _) = call(
        &w.router,
        Auth::Cookie(&cookie),
        Method::DELETE,
        &format!("/api/database-services/{database_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // A server with services cannot be removed; draining stops placement.
    let (status, _) = call(
        &w.router,
        Auth::Cookie(&cookie),
        Method::DELETE,
        &format!("/api/servers/{server_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, _) = call(
        &w.router,
        Auth::Cookie(&cookie),
        Method::PUT,
        &format!("/api/servers/{server_id}"),
        Some(json!({ "name": name, "draining": true })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = call(
        &w.router,
        Auth::Cookie(&cookie),
        Method::POST,
        "/api/compose-projects",
        Some(body(server_id)),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);

    let (status, _) = call(
        &w.router,
        Auth::Cookie(&cookie),
        Method::DELETE,
        &format!("/api/compose-projects/{service_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(remote
        .calls
        .lock()
        .unwrap()
        .contains(&"remove_compose_project".to_string()));
    let (status, _) = call(
        &w.router,
        Auth::Cookie(&cookie),
        Method::DELETE,
        &format!("/api/servers/{server_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn mtls_servers_enroll_from_a_csr_and_rotate() {
    let w = world().await;
    let _guard = test_support::PKI_TESTS.lock().await;
    let admin = test_support::user(&w.state.db, true).await;
    let cookie = test_support::session_cookie(&w.state, admin.id).await;
    let name = format!("m-{}", &Uuid::new_v4().simple().to_string()[..8]);
    let first = panel_core::pki::generate_agent_csr("10.9.8.7").unwrap();

    let (status, _) = call(&w.router, Auth::Cookie(&cookie), Method::POST, "/api/servers",
        Some(json!({ "name": name, "transport": "mtls", "address": "10.9.8.7:7843", "csr_pem": "garbage" }))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, created) = call(&w.router, Auth::Cookie(&cookie), Method::POST, "/api/servers",
        Some(json!({ "name": name, "transport": "mtls", "address": "10.9.8.7:7843", "csr_pem": first.cert_pem }))).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let server_id = Uuid::parse_str(created["server"]["id"].as_str().unwrap()).unwrap();
    assert!(created["certificate_pem"]
        .as_str()
        .unwrap()
        .contains("BEGIN CERTIFICATE"));
    let first_fp = created["server"]["cert_fingerprint"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        first_fp,
        panel_core::pki::fingerprint_pem(created["certificate_pem"].as_str().unwrap()).unwrap()
    );
    assert!(!created.to_string().contains("PRIVATE KEY"));

    let second = panel_core::pki::generate_agent_csr("10.9.8.7").unwrap();
    let (status, rotated) = call(
        &w.router,
        Auth::Cookie(&cookie),
        Method::POST,
        &format!("/api/servers/{server_id}/certificate"),
        Some(json!({ "csr_pem": second.cert_pem })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{rotated}");
    let server = db::servers::find(&w.state.db, server_id)
        .await
        .unwrap()
        .unwrap();
    assert!(server.rotation_pending);
    let pins = server.accepted_fingerprints(chrono::Utc::now());
    assert_eq!(pins.len(), 2);
    assert!(pins.contains(&first_fp));
    assert!(pins.contains(
        &panel_core::pki::fingerprint_pem(rotated["certificate_pem"].as_str().unwrap()).unwrap()
    ));

    let (status, _) = call(
        &w.router,
        Auth::Cookie(&cookie),
        Method::DELETE,
        &format!("/api/servers/{server_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn a_role_admin_cannot_mint_installation_admins() {
    let w = world().await;
    let user = test_support::user(&w.state.db, false).await;
    let superuser = test_support::user(&w.state.db, true).await;
    bind(&w.state, user.id, "admin", panel_core::rbac::Scope::Global).await;
    let cookie = test_support::session_cookie(&w.state, user.id).await;
    let name = format!("n{}", &Uuid::new_v4().simple().to_string()[..8]);
    let body = |is_admin: bool| json!({ "username": name, "email": format!("{name}@example.com"), "password": "a-long-password-123", "is_admin": is_admin });

    // A global `admin` binding passes `AdminUser` ...
    let (status, _) = call(
        &w.router,
        Auth::Cookie(&cookie),
        Method::GET,
        "/api/registries",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // ... but cannot create a superuser or weaken one.
    let (status, _) = call(
        &w.router,
        Auth::Cookie(&cookie),
        Method::POST,
        "/api/admin/users",
        Some(body(true)),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = call(
        &w.router,
        Auth::Cookie(&cookie),
        Method::POST,
        &format!("/api/admin/users/{}/disable-totp", superuser.id),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = call(
        &w.router,
        Auth::Cookie(&cookie),
        Method::POST,
        "/api/admin/users",
        Some(body(false)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

/// A custom role with exactly `permissions`, bound to `user` on project `p`.
async fn bind_custom(
    state: &AppState,
    user: Uuid,
    p: Uuid,
    permissions: &[panel_core::rbac::Permission],
) {
    let name = format!("c-{}", &Uuid::new_v4().simple().to_string()[..10]);
    let role = db::rbac::save_role(&state.db, None, &name, "", permissions)
        .await
        .unwrap()
        .unwrap();
    db::rbac::create_binding(
        &state.db,
        user,
        role,
        panel_core::rbac::Scope::Project { project_id: p },
        None,
    )
    .await
    .unwrap();
}

async fn service_variable_keys(state: &AppState, service: Uuid) -> Vec<String> {
    sqlx::query_scalar("SELECT key FROM env_variables WHERE service_id = $1 ORDER BY key")
        .bind(service)
        .fetch_all(&state.db)
        .await
        .unwrap()
}

#[tokio::test]
async fn deploying_with_an_environment_needs_variables_update() {
    let w = world().await;
    let user = test_support::user(&w.state.db, false).await;
    bind(
        &w.state,
        user.id,
        "deployer",
        panel_core::rbac::Scope::Project { project_id: w.p },
    )
    .await;
    let cookie = test_support::session_cookie(&w.state, user.id).await;
    let deploy = format!("/api/compose-projects/{}/deploy", w.svc_prod);
    let with_env = json!({ "environment": [["INJECTED", "x"]] });

    let (status, _) = call(
        &w.router,
        Auth::Cookie(&cookie),
        Method::POST,
        &deploy,
        Some(with_env.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(
        service_variable_keys(&w.state, w.svc_prod).await.is_empty(),
        "nothing was written"
    );
    let (status, _) = call(
        &w.router,
        Auth::Cookie(&cookie),
        Method::POST,
        &deploy,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);

    // A deploy-only key of an admin is held to the same boundary.
    let admin = test_support::user(&w.state.db, true).await;
    let admin_cookie = test_support::session_cookie(&w.state, admin.id).await;
    let (status, created) = call(&w.router, Auth::Cookie(&admin_cookie), Method::POST, "/api/api-keys", Some(json!({
        "name": "deploy-only",
        "grants": [{ "permission": "services:deploy", "scope": { "kind": "project", "project_id": w.p } }],
    }))).await;
    assert_eq!(status, StatusCode::CREATED);
    let token = created["token"].as_str().unwrap().to_string();
    let (status, _) = call(
        &w.router,
        Auth::Bearer(&token),
        Method::POST,
        &deploy,
        Some(with_env),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(service_variable_keys(&w.state, w.svc_prod).await.is_empty());
    let (status, _) = call(
        &w.router,
        Auth::Bearer(&token),
        Method::POST,
        &deploy,
        Some(json!({ "pull_images": true })),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
}

#[tokio::test]
async fn configuration_access_cannot_deploy_through_side_doors() {
    use panel_core::rbac::Permission;
    let w = world().await;
    let user = test_support::user(&w.state.db, false).await;
    bind_custom(
        &w.state,
        user.id,
        w.p,
        &[
            Permission::ServicesRead,
            Permission::ServicesUpdate,
            Permission::ServicesCreate,
        ],
    )
    .await;
    let cookie = test_support::session_cookie(&w.state, user.id).await;
    let auth = || Auth::Cookie(&cookie);
    let base = format!("/api/compose-projects/{}", w.svc_prod);
    let domain = |name: &str| {
        format!(
            "{name}-{}.example.com",
            &Uuid::new_v4().simple().to_string()[..8]
        )
    };
    let deployments = |state: AppState, id: Uuid| async move {
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM compose_deployments WHERE project_id = $1",
        )
        .bind(id)
        .fetch_one(&state.db)
        .await
        .unwrap()
    };

    let (status, _) = call(
        &w.router,
        auth(),
        Method::POST,
        &format!("{base}/deploy"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // Adding a domain redeploys by default: refused before anything changes.
    let (status, _) = call(
        &w.router,
        auth(),
        Method::POST,
        &format!("{base}/domains"),
        Some(json!({ "domain": domain("a"), "service_name": "web", "container_port": 80 })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (_, domains) = call(
        &w.router,
        auth(),
        Method::GET,
        &format!("{base}/domains"),
        None,
    )
    .await;
    assert!(domains.as_array().unwrap().is_empty());

    // Without the redeploy, configuring is allowed. (The loopback port
    // allocator answers 409 "retry" when another test takes the same port
    // at the same moment; that is its documented contract.)
    let mut attempt = 0;
    let (status, added) = loop {
        let result = call(&w.router, auth(), Method::POST, &format!("{base}/domains"),
            Some(json!({ "domain": domain("b"), "service_name": "web", "container_port": 80, "redeploy": false }))).await;
        attempt += 1;
        if result.0 != StatusCode::CONFLICT
            || attempt == 5
            || !result.1.to_string().contains("retry")
        {
            break result;
        }
    };
    assert_eq!(status, StatusCode::CREATED, "{added}");
    assert!(added["deployment"].is_null());
    let domain_id = added["id"].as_str().unwrap().to_string();
    let (status, _) = call(
        &w.router,
        auth(),
        Method::DELETE,
        &format!("{base}/domains/{domain_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = call(
        &w.router,
        auth(),
        Method::DELETE,
        &format!("{base}/domains/{domain_id}?redeploy=false"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(deployments(w.state.clone(), w.svc_prod).await, 0);

    // A webhook secret triggers deployments.
    let (status, _) = call(
        &w.router,
        auth(),
        Method::POST,
        &format!("{base}/webhook"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // Creating with the default first deployment, or with variables.
    let create = |deploy: bool, environment: Value| {
        json!({
            "name": "web", "slug": format!("c{}", &Uuid::new_v4().simple().to_string()[..10]),
            "source": { "kind": "raw", "compose_yaml": "services:\n  web:\n    image: nginx\n" },
            "environment_id": w.p_prod, "deploy": deploy, "environment": environment,
        })
    };
    let (status, _) = call(
        &w.router,
        auth(),
        Method::POST,
        "/api/compose-projects",
        Some(create(true, json!([]))),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = call(
        &w.router,
        auth(),
        Method::POST,
        "/api/compose-projects",
        Some(create(false, json!([["K", "v"]]))),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, body) = call(
        &w.router,
        auth(),
        Method::POST,
        "/api/compose-projects",
        Some(create(false, json!([]))),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert!(body["deployment"].is_null());
}

#[tokio::test]
async fn mcp_tools_use_the_same_permissions_as_the_api() {
    let w = world().await;
    let admin = test_support::user(&w.state.db, true).await;
    let admin_cookie = test_support::session_cookie(&w.state, admin.id).await;
    let (status, created) = call(&w.router, Auth::Cookie(&admin_cookie), Method::POST, "/api/api-keys", Some(json!({
        "name": "mcp",
        "grants": [
            { "permission": "services:deploy", "scope": { "kind": "project", "project_id": w.p } },
            { "permission": "services:read", "scope": { "kind": "project", "project_id": w.p } },
        ],
    }))).await;
    assert_eq!(status, StatusCode::CREATED);
    let token = created["token"].as_str().unwrap().to_string();
    let tool = |name: &str, arguments: Value| {
        json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": name, "arguments": arguments },
        })
    };
    let is_error = |response: &Value| response["result"]["isError"].as_bool().unwrap();

    let (status, response) = call(
        &w.router,
        Auth::Bearer(&token),
        Method::POST,
        "/api/mcp",
        Some(tool(
            "deploy_compose_project",
            json!({ "project_id": w.svc_prod }),
        )),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(!is_error(&response), "{response}");
    let (_, response) = call(
        &w.router,
        Auth::Bearer(&token),
        Method::POST,
        "/api/mcp",
        Some(tool(
            "deploy_compose_project",
            json!({ "project_id": w.svc_q }),
        )),
    )
    .await;
    assert!(is_error(&response));
    assert!(response.to_string().contains("permission denied"));
    let (_, response) = call(
        &w.router,
        Auth::Bearer(&token),
        Method::POST,
        "/api/mcp",
        Some(tool("list_compose_projects", json!({}))),
    )
    .await;
    let listed = response["result"]["content"][0]["text"].as_str().unwrap();
    assert!(listed.contains(&w.svc_prod.to_string()) && !listed.contains(&w.svc_q.to_string()));
    let (_, response) = call(
        &w.router,
        Auth::Bearer(&token),
        Method::POST,
        "/api/mcp",
        Some(tool("list_sites", json!({}))),
    )
    .await;
    assert!(is_error(&response));
    assert!(response
        .to_string()
        .contains("not available with an API key"));

    // A session user without bindings is refused too.
    let user = test_support::user(&w.state.db, false).await;
    let cookie = test_support::session_cookie(&w.state, user.id).await;
    let (_, response) = call(
        &w.router,
        Auth::Cookie(&cookie),
        Method::POST,
        "/api/mcp",
        Some(tool(
            "deploy_compose_project",
            json!({ "project_id": w.svc_prod }),
        )),
    )
    .await;
    assert!(is_error(&response));
}
