//! Helpers for tests that run against the shared PostgreSQL test database
//! (`DATABASE_URL`, default the local `scontrol-test-pg` container).

use crate::auth::session::{self, CookieCipher};
use crate::db;
use crate::services::deploy_queue::DeployQueue;
use crate::services::github_app::GitHubApps;
use crate::services::notifications::Notifier;
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
