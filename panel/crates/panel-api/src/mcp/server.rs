use super::protocol::{JsonRpcRequest, JsonRpcResponse};
use super::tools;
use crate::state::AppState;
use serde_json::{json, Value};
use uuid::Uuid;

pub const PROTOCOL_VERSION: &str = "2024-11-05";

/// The panel identity an MCP request acts as. HTTP transport derives this
/// from the session cookie (see `routes::mcp`); stdio transport (typically
/// launched locally, already root-equivalent trust) acts as the configured
/// service account.
#[derive(Clone, Copy)]
pub struct McpIdentity {
    pub user_id: Uuid,
    pub is_admin: bool,
}

pub async fn handle_request(
    state: &AppState,
    identity: McpIdentity,
    request: JsonRpcRequest,
) -> JsonRpcResponse {
    let id = request.id.clone().unwrap_or(Value::Null);
    match request.method.as_str() {
        "initialize" => JsonRpcResponse::ok(
            id,
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "serverInfo": { "name": "scontrol", "version": env!("CARGO_PKG_VERSION") },
                "capabilities": { "tools": {} }
            }),
        ),
        "notifications/initialized" => JsonRpcResponse::ok(id, json!({})),
        "tools/list" => {
            let tool_list: Vec<Value> = tools::list_tool_defs()
                .into_iter()
                .map(|t| json!({ "name": t.name, "description": t.description, "inputSchema": t.input_schema }))
                .collect();
            JsonRpcResponse::ok(id, json!({ "tools": tool_list }))
        }
        "tools/call" => {
            let name = request
                .params
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let arguments = request
                .params
                .get("arguments")
                .cloned()
                .unwrap_or(json!({}));
            match tools::call_tool(state, identity.user_id, identity.is_admin, name, arguments)
                .await
            {
                Ok(result) => JsonRpcResponse::ok(
                    id,
                    json!({ "content": [{ "type": "text", "text": result.to_string() }], "isError": false }),
                ),
                Err(message) => JsonRpcResponse::ok(
                    id,
                    json!({ "content": [{ "type": "text", "text": message }], "isError": true }),
                ),
            }
        }
        other => JsonRpcResponse::err(id, -32601, format!("method not found: {other}")),
    }
}
