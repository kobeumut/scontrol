//! Helpers for tests that run against the shared PostgreSQL test database
//! (`DATABASE_URL`, default the local `scontrol-test-pg` container).

use crate::auth::session::{self, CookieCipher};
use crate::db;
use crate::services::deploy_queue::DeployQueue;
use crate::services::github_app::GitHubApps;
use crate::services::metrics::MetricsCollector;
use crate::services::notifications::Notifier;
use crate::services::previews::Previews;
use crate::services::scheduled_jobs::JobRunCollector;
use crate::services::secret_box::SecretBox;
use crate::services::webhook::WebhookKey;
use crate::state::AppState;
use panel_core::adapters::mock::MockAdapter;
use sqlx::PgPool;
use std::sync::Arc;
use uuid::Uuid;

/// Tests that claim from, or depend on the state of, the shared deployment
/// queue table hold this so they do not interleave.
pub static QUEUE_TESTS: once_cell::sync::Lazy<tokio::sync::Mutex<()>> =
    once_cell::sync::Lazy::new(|| tokio::sync::Mutex::new(()));

pub async fn test_pool() -> PgPool {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://scontrol:scontrol@localhost:55432/scontrol".to_string());
    let pool = PgPool::connect(&url)
        .await
        .expect("connect to test database");
    db::run_migrations(&pool).await.expect("migrations");
    pool
}

/// A user with a unique name, so tests sharing the database never collide.
pub async fn user(pool: &PgPool, is_admin: bool) -> db::users::UserRow {
    db::users::create_user(
        pool,
        &format!("u{}", &Uuid::new_v4().simple().to_string()[..8]),
        &format!("{}@example.com", Uuid::new_v4()),
        "hash",
        is_admin,
    )
    .await
    .unwrap()
}

/// Application state backed by `adapter` instead of the agent socket. No
/// deployment workers are started.
pub fn app_state(pool: PgPool, adapter: Arc<MockAdapter>) -> AppState {
    let key = [42u8; 32];
    AppState {
        db: pool,
        agent: adapter,
        cookie_cipher: CookieCipher::new(&key),
        session_ttl_hours: 1,
        deploy_queue: DeployQueue::new(),
        webhook_key: WebhookKey::derive(&key),
        notifier: Notifier::new(SecretBox::derive(&key)),
        secret_box: SecretBox::derive(&key),
        github_apps: github_apps(),
        job_runs: JobRunCollector::new(),
        metrics: MetricsCollector::new(),
        previews: Previews::with_api_base(
            SecretBox::derive(&key),
            github_apps(),
            "http://127.0.0.1:9",
        ),
    }
}

/// A `Cookie` header value carrying a fresh session for `user_id`.
pub async fn session_cookie(state: &AppState, user_id: Uuid) -> String {
    let (token, token_hash) = session::generate_token();
    let expires_at = chrono::Utc::now() + chrono::Duration::hours(1);
    db::users::create_session(&state.db, user_id, &token_hash, expires_at)
        .await
        .unwrap();
    let value = state.cookie_cipher.encrypt(&token).unwrap();
    format!("{}={value}", session::SESSION_COOKIE_NAME)
}

/// GitHub App service with the test key; it talks to an unroutable API
/// unless a test builds its own with `GitHubApps::with_api_base`.
pub fn github_apps() -> GitHubApps {
    GitHubApps::with_api_base(SecretBox::derive(&[42u8; 32]), "http://127.0.0.1:9")
}

/// A one-route HTTP receiver: answers 200 and hands over each request's
/// lower-cased headers and body.
pub async fn http_receiver() -> (String, tokio::sync::mpsc::Receiver<(String, String)>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/hook", listener.local_addr().unwrap());
    let (tx, rx) = tokio::sync::mpsc::channel(8);
    tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut raw = Vec::new();
            let mut buf = [0u8; 4096];
            let (head, body) = loop {
                let n = socket.read(&mut buf).await.unwrap();
                raw.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&raw).to_string();
                if let Some((head, body)) = text.split_once("\r\n\r\n") {
                    let length = head
                        .lines()
                        .find_map(|l| {
                            l.to_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    if body.len() >= length {
                        break (head.to_lowercase(), body.to_string());
                    }
                }
            };
            socket
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
                .await
                .unwrap();
            tx.send((head, body)).await.ok();
        }
    });
    (url, rx)
}

/// Metrics tests share the global metric tables, so they run one at a time.
pub static METRICS_TESTS: once_cell::sync::Lazy<tokio::sync::Mutex<()>> =
    once_cell::sync::Lazy::new(|| tokio::sync::Mutex::new(()));

/// Empties the metric tables and restores default alert settings.
pub async fn reset_metrics(pool: &PgPool) {
    for statement in [
        "DELETE FROM server_metrics",
        "DELETE FROM container_metrics",
        "DELETE FROM metric_alert_state",
        "UPDATE metric_alert_settings SET enabled = TRUE, cpu_pct = 85, memory_pct = 85, \
         disk_pct = 85, container_memory_pct = 90, hysteresis_pct = 5, trigger_samples = 3, \
         clear_samples = 3",
    ] {
        sqlx::query(statement).execute(pool).await.unwrap();
    }
}

/// A bare Compose project row (with its project and environment); returns
/// `(id, slug)`.
pub async fn compose_project(pool: &PgPool) -> (Uuid, String) {
    let owner = user(pool, true).await;
    let suffix = &Uuid::new_v4().simple().to_string()[..10];
    let (project_id, environment_id, id) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    let slug = format!("m-{suffix}");
    sqlx::query("INSERT INTO projects (id, name, slug, owner_user_id) VALUES ($1, $2, $2, $3)")
        .bind(project_id)
        .bind(&slug)
        .bind(owner.id)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO environments (id, project_id, name, slug) VALUES ($1, $2, 'production', 'production')",
    )
    .bind(environment_id)
    .bind(project_id)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO compose_projects (id, name, slug, owner_user_id, source, environment_id) \
         VALUES ($1, $2, $2, $3, '{\"kind\":\"raw\",\"compose_yaml\":\"services: {}\"}', $4)",
    )
    .bind(id)
    .bind(&slug)
    .bind(owner.id)
    .bind(environment_id)
    .execute(pool)
    .await
    .unwrap();
    (id, slug)
}
