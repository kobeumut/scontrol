use crate::auth::AuthUser;
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
/// API (session cookie), so an MCP client acts with exactly the calling
/// user's permissions — no separate, wider-scoped credential.
async fn handle(
    AuthUser(user): AuthUser,
    State(state): State<AppState>,
    Json(request): Json<JsonRpcRequest>,
) -> Json<Value> {
    let identity = McpIdentity {
        user_id: user.id,
        is_admin: user.is_admin,
    };
    let response = mcp::handle_request(&state, identity, request).await;
    Json(serde_json::to_value(response).unwrap())
}
