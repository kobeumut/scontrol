pub mod admin;
pub mod auth;
pub mod compose_projects;
pub mod cron;
pub mod databases;
pub mod firewall;
pub mod health;
pub mod mcp;
pub mod notifications;
pub mod projects;
pub mod sites;
pub mod tls;
pub mod webhooks;

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
        .nest("/api/projects", projects::router())
        .nest("/api/compose-projects", compose_projects::router())
        .nest("/api/notifications", notifications::router())
        .nest("/api/firewall", firewall::router())
        .nest("/api/tls", tls::router())
        .nest("/api/webhooks", webhooks::router())
        .nest("/api/admin", admin::router())
        // `/health` serves Nginx/systemd probes; the SPA calls everything
        // under `/api`, so the same handlers are mounted there too.
        .nest("/health", health::router())
        .nest("/api/health", health::router())
        .layer(TraceLayer::new_for_http())
        .layer(CorsLayer::permissive())
        .with_state(state)
}
