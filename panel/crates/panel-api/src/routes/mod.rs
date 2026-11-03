pub mod admin;
pub mod auth;
pub mod compose_projects;
pub mod cron;
pub mod databases;
pub mod firewall;
pub mod health;
pub mod mcp;
pub mod sites;
pub mod tls;

use crate::state::AppState;
use axum::Router;
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;

pub fn build_router(state: AppState) -> Router {
    Router::new()
        .nest("/api/auth", auth::router())
        .nest("/api/sites", sites::router())
        .nest("/api/databases", databases::router())
        .nest("/api/mcp", mcp::router())
        .nest("/api/cron", cron::router())
        .nest("/api/compose-projects", compose_projects::router())
        .nest("/api/firewall", firewall::router())
        .nest("/api/tls", tls::router())
        .nest("/api/admin", admin::router())
        .nest("/health", health::router())
        .layer(TraceLayer::new_for_http())
        .layer(CorsLayer::permissive())
        .with_state(state)
}
