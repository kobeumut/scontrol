use super::*;
use panel_core::rbac::Permission as P;

async fn remote(w: &World, cookie: &str) -> (Uuid, Arc<MockAdapter>) {
    let name = format!("gene82-{}", Uuid::new_v4().simple());
    let (status, created) = call(&w.router, Auth::Cookie(cookie), Method::POST, "/api/servers",
        Some(json!({"name": name, "transport": "ssh_socket", "address": format!("/run/scontrol/remote/{name}.sock")}))).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let id = created["server"]["id"].as_str().unwrap().parse().unwrap();
    let agent = Arc::new(MockAdapter::new());
    w.state.agents.set_override(id, agent.clone());
    sqlx::query("UPDATE compose_projects SET server_id = $1 WHERE id = $2")
        .bind(id)
        .bind(w.svc_prod)
        .execute(&w.state.db)
        .await
        .unwrap();
    (id, agent)
}

async fn scoped(w: &World, permissions: &[P]) -> String {
    let user = test_support::user(&w.state.db, false).await;
    bind_custom(&w.state, user.id, w.p, permissions).await;
    test_support::session_cookie(&w.state, user.id).await
}

#[tokio::test]
async fn domain_secrets_require_scoped_variable_and_deploy_grants_and_go_only_to_owner() {
    let _guard = test_support::QUEUE_TESTS.lock().await;
    let w = world().await;
    let admin = test_support::user(&w.state.db, true).await;
    let admin = test_support::session_cookie(&w.state, admin.id).await;
    let (_, agent) = remote(&w, &admin).await;
    let domain = db::compose_projects::create_domain(
        &w.state.db,
        w.svc_prod,
        &format!("g{}.example.com", Uuid::new_v4().simple()),
        "web",
        80,
        false,
    )
    .await
    .unwrap()
    .unwrap();
    let uri = format!(
        "/api/compose-projects/{}/domains/{}/options",
        w.svc_prod, domain.id
    );
    let body = json!({"options": {"basic_auth": true}, "basic_auth": {
        "action": "set", "value": [{"username": "alice", "password": "never-echo-this-password"}]}, "redeploy": false});
    let updater = scoped(&w, &[P::ServicesUpdate]).await;
    let secret_writer = scoped(&w, &[P::ServicesUpdate, P::VariablesUpdate]).await;
    for (cookie, input) in [
        (&updater, body.clone()),
        (&secret_writer, {
            let mut b = body.clone();
            b["redeploy"] = json!(true);
            b
        }),
    ] {
        let (status, _) = call(
            &w.router,
            Auth::Cookie(cookie),
            Method::PUT,
            &uri,
            Some(input),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }
    assert!(agent.vhost_secret_writes.lock().unwrap().is_empty());
    assert_eq!(
        db::compose_projects::list_domains(&w.state.db, w.svc_prod)
            .await
            .unwrap()[0]
            .options,
        domain.options
    );
    let wrong = format!(
        "/api/compose-projects/{}/domains/{}/options",
        w.svc_q, domain.id
    );
    assert_eq!(
        call(
            &w.router,
            Auth::Cookie(&secret_writer),
            Method::PUT,
            &wrong,
            Some(body.clone())
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let (status, result) = call(
        &w.router,
        Auth::Cookie(&secret_writer),
        Method::PUT,
        &uri,
        Some(body),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{result}");
    assert!(!result.to_string().contains("never-echo-this-password"));
    let before = db::compose_projects::list_domains(&w.state.db, w.svc_prod)
        .await
        .unwrap()
        .remove(0)
        .options;
    let revision = before.secret_revision.unwrap();
    assert_eq!(
        agent.vhost_secret_writes.lock().unwrap()[0].revision,
        revision
    );
    assert!(w.local.vhost_secret_writes.lock().unwrap().is_empty());
    // Stale updates cannot resurrect a revoked secret revision.
    let removed = panel_core::vhost::VhostOptions::default();
    assert!(db::compose_projects::set_domain_options(
        &w.state.db,
        w.svc_prod,
        domain.id,
        &before,
        &removed
    )
    .await
    .unwrap());
    assert!(!db::compose_projects::set_domain_options(
        &w.state.db,
        w.svc_prod,
        domain.id,
        &before,
        &before
    )
    .await
    .unwrap());
    assert_eq!(
        db::compose_projects::list_domains(&w.state.db, w.svc_prod)
            .await
            .unwrap()[0]
            .options,
        removed
    );
}

#[tokio::test]
async fn template_install_checks_each_permission_before_creating_or_writing() {
    let _guard = test_support::QUEUE_TESTS.lock().await;
    let w = world().await;
    let slug = format!("template-{}", &Uuid::new_v4().simple().to_string()[..8]);
    let base = [
        P::TemplatesInstall,
        P::ServicesCreate,
        P::VariablesUpdate,
        P::ServicesDeploy,
    ];
    let body = json!({"name":"WordPress", "slug":slug, "environment_id":w.p_prod});
    for missing in base {
        let permissions: Vec<_> = base.iter().copied().filter(|p| *p != missing).collect();
        let cookie = scoped(&w, &permissions).await;
        assert_eq!(
            call(
                &w.router,
                Auth::Cookie(&cookie),
                Method::POST,
                "/api/templates/wordpress/install",
                Some(body.clone())
            )
            .await
            .0,
            StatusCode::FORBIDDEN,
            "{missing:?}"
        );
        let count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM compose_projects WHERE slug = $1")
                .bind(&slug)
                .fetch_one(&w.state.db)
                .await
                .unwrap();
        assert_eq!(count, 0);
    }
    let cookie = scoped(&w, &base).await;
    let mut foreign = body.clone();
    foreign["environment_id"] = json!(
        db::compose_projects::find_by_id(&w.state.db, w.svc_q)
            .await
            .unwrap()
            .unwrap()
            .environment_id
    );
    assert_eq!(
        call(
            &w.router,
            Auth::Cookie(&cookie),
            Method::POST,
            "/api/templates/wordpress/install",
            Some(foreign)
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let (status, installed) = call(
        &w.router,
        Auth::Cookie(&cookie),
        Method::POST,
        "/api/templates/wordpress/install",
        Some(body),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{installed}");
    let id: Uuid = installed["project"]["id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let vars = db::projects::layer_variables(&w.state.db, db::projects::EnvScope::Service(id))
        .await
        .unwrap();
    assert!(!vars.is_empty());
    for variable in vars.iter().filter(|v| v.key.contains("PASSWORD")) {
        let aad = format!("scontrol/env-var/v1:service:{id}:{}", variable.key);
        let plaintext = w
            .state
            .secret_box
            .open(&variable.value_sealed, aad.as_bytes())
            .unwrap();
        assert!(!installed
            .to_string()
            .contains(std::str::from_utf8(&plaintext).unwrap()));
    }
    assert!(w.local.calls.lock().unwrap().is_empty());
    // No deploy grant is needed when installation explicitly does not deploy.
    let cookie = scoped(&w, &base[..3]).await;
    assert_eq!(call(&w.router, Auth::Cookie(&cookie), Method::POST, "/api/templates/wordpress/install",
        Some(json!({"name":"WordPress", "slug":format!("{slug}-idle"), "environment_id":w.p_prod, "deploy":false}))).await.0, StatusCode::ACCEPTED);
}

#[tokio::test]
async fn previews_require_creation_deletion_and_secret_permissions_in_the_parent_scope() {
    let _guard = test_support::QUEUE_TESTS.lock().await;
    let w = world().await;
    sqlx::query("UPDATE compose_projects SET source = $1 WHERE id = $2")
        .bind(json!({"kind":"git", "repository_url":"https://github.com/acme/shop.git", "branch":"main", "compose_path":"compose.yml"}))
        .bind(w.svc_prod).execute(&w.state.db).await.unwrap();
    let uri = format!("/api/compose-projects/{}/previews", w.svc_prod);
    let body = json!({"enabled":true, "base_domain":"preview.example.com", "service_name":"web", "container_port":80});
    let updater = scoped(&w, &[P::ServicesUpdate, P::ServicesDeploy]).await;
    let creator = scoped(
        &w,
        &[P::ServicesUpdate, P::ServicesDeploy, P::ServicesCreate],
    )
    .await;
    assert_eq!(
        call(
            &w.router,
            Auth::Cookie(&updater),
            Method::PUT,
            &uri,
            Some(body.clone())
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert!(db::previews::settings(&w.state.db, w.svc_prod)
        .await
        .unwrap()
        .is_none());
    assert_eq!(
        call(
            &w.router,
            Auth::Cookie(&creator),
            Method::PUT,
            &uri,
            Some(body.clone())
        )
        .await
        .0,
        StatusCode::OK
    );
    let mut disabled = body;
    disabled["enabled"] = json!(false);
    assert_eq!(
        call(
            &w.router,
            Auth::Cookie(&creator),
            Method::PUT,
            &uri,
            Some(disabled)
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert!(
        db::previews::settings(&w.state.db, w.svc_prod)
            .await
            .unwrap()
            .unwrap()
            .enabled
    );
    assert_eq!(
        call(
            &w.router,
            Auth::Cookie(&creator),
            Method::DELETE,
            &format!("{uri}/1"),
            None
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(
            &w.router,
            Auth::Cookie(&creator),
            Method::PUT,
            &format!("{uri}/comment-token"),
            Some(json!({"token":"private-gene82-token"}))
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert!(db::previews::comment_token(&w.state.db, w.svc_prod)
        .await
        .unwrap()
        .is_none());
    let writer = scoped(&w, &[P::ServicesUpdate, P::VariablesUpdate]).await;
    assert_eq!(
        call(
            &w.router,
            Auth::Cookie(&writer),
            Method::PUT,
            &format!("{uri}/comment-token"),
            Some(json!({"token":"private-gene82-token"}))
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    let reader = scoped(&w, &[P::ServicesRead]).await;
    let (_, result) = call(&w.router, Auth::Cookie(&reader), Method::GET, &uri, None).await;
    assert!(!result.to_string().contains("private-gene82-token"));
    assert!(w.local.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn terminal_scope_and_all_session_operations_are_pinned_to_the_remote_agent() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let w = world().await;
    let admin = test_support::user(&w.state.db, true).await;
    let admin = test_support::session_cookie(&w.state, admin.id).await;
    let (_, agent) = remote(&w, &admin).await;
    let denied = scoped(&w, &[P::ServicesRead, P::ServicesDeploy]).await;
    let terminal = scoped(&w, &[P::ServicesTerminal]).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = w.router.clone();
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    for (cookie, service, expected) in [
        (&denied, w.svc_prod, "403"),
        (&terminal, w.svc_q, "403"),
        (&terminal, w.svc_prod, "101"),
    ] {
        let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
        let request = format!("GET /api/compose-projects/{service}/terminal?service=web HTTP/1.1\r\nHost: {address}\r\nOrigin: http://{address}\r\nCookie: {cookie}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n");
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut bytes = vec![0; 2048];
        let n = tokio::time::timeout(std::time::Duration::from_secs(5), stream.read(&mut bytes))
            .await
            .unwrap()
            .unwrap();
        assert!(String::from_utf8_lossy(&bytes[..n]).starts_with(&format!("HTTP/1.1 {expected}")));
        if expected == "101" {
            // Masked binary input, then disconnect; all RPCs must use this agent.
            stream
                .write_all(&[0x82, 0x81, 0, 0, 0, 0, b'x'])
                .await
                .unwrap();
            let resize = br#"{"type":"resize","cols":100,"rows":30}"#;
            let mut frame = vec![0x81, 0x80 | resize.len() as u8, 0, 0, 0, 0];
            frame.extend_from_slice(resize);
            stream.write_all(&frame).await.unwrap();
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                while !["write_terminal", "resize_terminal"]
                    .iter()
                    .all(|method| agent.calls.lock().unwrap().iter().any(|c| c == method))
                {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
        }
    }
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !agent
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|c| c == "close_terminal")
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    server.abort();
    let _ = server.await;
    assert_eq!(agent.terminals.lock().unwrap().len(), 1);
    assert!(w.local.terminals.lock().unwrap().is_empty());
    assert!(!w
        .local
        .calls
        .lock()
        .unwrap()
        .iter()
        .any(|c| c.contains("terminal")));
}
