use crate::auth::AdminUser;
use crate::db;
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;
use axum::extract::{Path, State};
use axum::routing::{delete, get};
use axum::{Json, Router};
use panel_core::adapters::{ApplyFirewallRuleParams, RemoveFirewallRuleParams};
use panel_core::types::{FirewallProtocol, FirewallRule};
use serde::Deserialize;
use uuid::Uuid;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(list).post(apply))
        .route("/:id", delete(remove))
}

async fn list(
    AdminUser(_): AdminUser,
    State(state): State<AppState>,
) -> ApiResult<Json<Vec<FirewallRule>>> {
    Ok(Json(db::firewall::list_all(&state.db).await?))
}

#[derive(Deserialize)]
struct ApplyRuleBody {
    port: u16,
    protocol: FirewallProtocol,
    allow: bool,
    description: Option<String>,
}

async fn apply(
    AdminUser(user): AdminUser,
    State(state): State<AppState>,
    Json(body): Json<ApplyRuleBody>,
) -> ApiResult<Json<FirewallRule>> {
    state
        .agent
        .apply_firewall_rule(ApplyFirewallRuleParams {
            port: body.port,
            protocol: body.protocol,
            allow: body.allow,
            description: body.description.clone().unwrap_or_default(),
        })
        .await?;
    let rule = db::firewall::upsert(
        &state.db,
        body.port,
        body.protocol,
        body.allow,
        &body.description.unwrap_or_default(),
    )
    .await?;
    db::audit::record(
        &state.db,
        Some(user.id),
        "user",
        "apply_firewall_rule",
        Some(&body.port.to_string()),
        serde_json::json!({ "allow": body.allow }),
    )
    .await
    .ok();
    Ok(Json(rule))
}

/// Removes a rule from UFW, then from the panel. The agent runs first, so a
/// rule UFW still enforces is never dropped from the list.
async fn remove(
    AdminUser(user): AdminUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<serde_json::Value>> {
    let rule = db::firewall::find(&state.db, id)
        .await?
        .ok_or_else(|| ApiError::NotFound("firewall rule not found".into()))?;
    state
        .agent
        .remove_firewall_rule(RemoveFirewallRuleParams {
            port: rule.port,
            protocol: rule.protocol,
            allow: rule.allow,
        })
        .await?;
    db::firewall::delete(&state.db, id).await?;
    db::audit::record(
        &state.db,
        Some(user.id),
        "user",
        "remove_firewall_rule",
        Some(&rule.port.to_string()),
        serde_json::json!({ "protocol": rule.protocol.to_string(), "allow": rule.allow }),
    )
    .await
    .ok();
    Ok(Json(serde_json::json!({ "deleted": true })))
}
