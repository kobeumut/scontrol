use crate::auth::Authenticated;
use crate::mcp::{self, protocol::JsonRpcRequest, McpIdentity};
use crate::state::AppState;
use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use serde_json::Value;

pub fn router() -> Router<AppState> {
    Router::new().route("/", post(handle))
}

/// HTTP transport for MCP: authenticated the same way as the rest of the
/// API (session cookie or API key), so an MCP client acts with exactly the
/// caller's permissions (a key's narrowed ones) and no wider credential.
async fn handle(
    Authenticated(principal): Authenticated,
    State(state): State<AppState>,
    Json(request): Json<JsonRpcRequest>,
) -> Json<Value> {
    let identity = McpIdentity::from_principal(&principal);
    let response = mcp::handle_request(&state, identity, request).await;
    Json(serde_json::to_value(response).unwrap())
}
