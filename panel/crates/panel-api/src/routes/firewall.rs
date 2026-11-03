use crate::auth::AdminUser;
use crate::db;
use crate::error::ApiResult;
use crate::state::AppState;
use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use panel_core::adapters::ApplyFirewallRuleParams;
use panel_core::types::{FirewallProtocol, FirewallRule};
use serde::Deserialize;

pub fn router() -> Router<AppState> {
    Router::new().route("/", get(list).post(apply))
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
