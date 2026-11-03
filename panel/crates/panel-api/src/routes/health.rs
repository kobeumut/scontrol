use crate::state::AppState;
use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(health))
        .route("/agent", get(agent_health))
}

async fn health() -> Json<serde_json::Value> {
    Json(json!({ "status": "ok" }))
}

async fn agent_health(State(state): State<AppState>) -> Json<serde_json::Value> {
    match state.agent.capabilities().await {
        Ok(caps) => Json(json!({ "status": "ok", "capabilities": caps })),
        Err(e) => Json(json!({ "status": "unreachable", "error": e.to_string() })),
    }
}
