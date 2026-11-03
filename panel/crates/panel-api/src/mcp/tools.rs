//! MCP tool definitions exposing panel operations to AI assistants (Claude
//! Desktop, Cursor, OpenCode, Gemini CLI, ...).

use crate::db;
use crate::services::site_service::{self, CreateSiteRequest};
use crate::state::AppState;
use panel_core::adapters::{ApplyFirewallRuleParams, CreateDatabaseParams};
use panel_core::types::{DatabaseEngine, FirewallProtocol, Runtime};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

pub struct ToolDef {
    pub name: &'static str,
    pub description: &'static str,
    pub input_schema: Value,
}

pub fn list_tool_defs() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "list_sites",
            description: "List all sites managed by the panel.",
            input_schema: schema_for::<ListSitesInput>(),
        },
        ToolDef {
            name: "create_site",
            description: "Create a new PHP, Node.js, Python, static, or reverse-proxy site.",
            input_schema: schema_for::<CreateSiteInput>(),
        },
        ToolDef {
            name: "delete_site",
            description: "Delete a site by id.",
            input_schema: schema_for::<DeleteSiteInput>(),
        },
        ToolDef {
            name: "create_database",
            description: "Provision a Postgres or MySQL database for a site.",
            input_schema: schema_for::<CreateDatabaseInput>(),
        },
        ToolDef {
            name: "apply_firewall_rule",
            description: "Allow or deny a TCP/UDP port through the firewall.",
            input_schema: schema_for::<FirewallRuleInput>(),
        },
        ToolDef {
            name: "list_compose_projects",
            description: "List Dokploy-style Docker Compose projects and deployment status.",
            input_schema: schema_for::<EmptyInput>(),
        },
        ToolDef {
            name: "compose_project_services",
            description: "Inspect services and ports belonging to a Compose project.",
            input_schema: schema_for::<ComposeProjectIdInput>(),
        },
        ToolDef {
            name: "compose_project_logs",
            description: "Read a bounded tail of Docker Compose logs for a project.",
            input_schema: schema_for::<ComposeProjectLogsInput>(),
        },
        ToolDef {
            name: "agent_capabilities",
            description:
                "Report which optional features (docker, mail, antivirus) the agent has enabled.",
            input_schema: schema_for::<EmptyInput>(),
        },
    ]
}

fn schema_for<T: JsonSchema>() -> Value {
    serde_json::to_value(schemars::schema_for!(T)).unwrap_or(json!({}))
}

#[derive(Deserialize, JsonSchema)]
struct EmptyInput {}

#[derive(Deserialize, JsonSchema)]
struct ListSitesInput {}

#[derive(Deserialize, JsonSchema)]
struct CreateSiteInput {
    domain: String,
    owner_username: String,
    runtime: Runtime,
}

#[derive(Deserialize, JsonSchema)]
struct DeleteSiteInput {
    site_id: Uuid,
}

#[derive(Deserialize, JsonSchema)]
struct CreateDatabaseInput {
    site_id: Option<Uuid>,
    engine: DatabaseEngine,
    name: String,
    username: String,
}

#[derive(Deserialize, JsonSchema)]
struct FirewallRuleInput {
    port: u16,
    protocol: FirewallProtocol,
    allow: bool,
    description: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct ComposeProjectIdInput {
    project_id: Uuid,
}

#[derive(Deserialize, JsonSchema)]
struct ComposeProjectLogsInput {
    project_id: Uuid,
    tail_lines: Option<u16>,
}

/// Dispatches a tool call. `actor_user_id` is the panel user the MCP session
/// is authenticated as (MCP calls go through the same auth/audit path as the
/// REST API — see `routes::mcp`).
pub async fn call_tool(
    state: &AppState,
    actor_user_id: Uuid,
    is_admin: bool,
    name: &str,
    arguments: Value,
) -> Result<Value, String> {
    match name {
        "list_sites" => {
            let sites = if is_admin {
                db::sites::list_all(&state.db).await
            } else {
                db::sites::list_for_owner(&state.db, actor_user_id).await
            }
            .map_err(|e| e.to_string())?;
            Ok(json!({ "sites": sites }))
        }
        "create_site" => {
            let input: CreateSiteInput =
                serde_json::from_value(arguments).map_err(|e| e.to_string())?;
            let site = site_service::create_site(
                &state.db,
                state.agent.as_ref(),
                CreateSiteRequest {
                    domain: input.domain,
                    owner_user_id: actor_user_id,
                    owner_username: input.owner_username,
                    runtime: input.runtime,
                },
            )
            .await
            .map_err(|e| e.to_string())?;
            Ok(serde_json::to_value(site).unwrap())
        }
        "delete_site" => {
            let input: DeleteSiteInput =
                serde_json::from_value(arguments).map_err(|e| e.to_string())?;
            site_service::delete_site(&state.db, state.agent.as_ref(), input.site_id)
                .await
                .map_err(|e| e.to_string())?;
            Ok(json!({ "deleted": true }))
        }
        "create_database" => {
            if !is_admin {
                return Err("only admins may create databases via MCP".into());
            }
            let input: CreateDatabaseInput =
                serde_json::from_value(arguments).map_err(|e| e.to_string())?;
            if !panel_core::types::validate::is_valid_identifier(&input.name)
                || !panel_core::types::validate::is_valid_identifier(&input.username)
            {
                return Err("invalid database/user identifier".into());
            }
            let password: String = {
                use rand::Rng;
                let charset: &[u8] =
                    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
                let mut rng = rand::thread_rng();
                (0..24)
                    .map(|_| charset[rng.gen_range(0..charset.len())] as char)
                    .collect()
            };
            state
                .agent
                .create_database(CreateDatabaseParams {
                    engine: input.engine,
                    name: input.name.clone(),
                    username: input.username.clone(),
                    password: password.clone(),
                })
                .await
                .map_err(|e| e.to_string())?;
            let info = db::databases::create(
                &state.db,
                input.site_id,
                input.engine,
                &input.name,
                &input.username,
            )
            .await
            .map_err(|e| e.to_string())?;
            Ok(json!({ "database": info, "password": password }))
        }
        "apply_firewall_rule" => {
            if !is_admin {
                return Err("only admins may change firewall rules via MCP".into());
            }
            let input: FirewallRuleInput =
                serde_json::from_value(arguments).map_err(|e| e.to_string())?;
            state
                .agent
                .apply_firewall_rule(ApplyFirewallRuleParams {
                    port: input.port,
                    protocol: input.protocol,
                    allow: input.allow,
                    description: input.description.clone().unwrap_or_default(),
                })
                .await
                .map_err(|e| e.to_string())?;
            let rule = db::firewall::upsert(
                &state.db,
                input.port,
                input.protocol,
                input.allow,
                &input.description.unwrap_or_default(),
            )
            .await
            .map_err(|e| e.to_string())?;
            Ok(serde_json::to_value(rule).unwrap())
        }
        "list_compose_projects" => {
            if !is_admin {
                return Err("only admins may inspect Docker projects via MCP".into());
            }
            let projects = db::compose_projects::list_all(&state.db)
                .await
                .map_err(|e| e.to_string())?;
            Ok(json!({ "projects": projects }))
        }
        "compose_project_services" => {
            if !is_admin {
                return Err("only admins may inspect Docker projects via MCP".into());
            }
            let input: ComposeProjectIdInput =
                serde_json::from_value(arguments).map_err(|e| e.to_string())?;
            let project = db::compose_projects::find_by_id(&state.db, input.project_id)
                .await
                .map_err(|e| e.to_string())?
                .ok_or("Compose project not found")?;
            let services = state
                .agent
                .list_compose_project_services(panel_core::adapters::ComposeProjectActionParams {
                    project_slug: project.slug,
                })
                .await
                .map_err(|e| e.to_string())?;
            Ok(json!({ "services": services }))
        }
        "compose_project_logs" => {
            if !is_admin {
                return Err("only admins may inspect Docker project logs via MCP".into());
            }
            let input: ComposeProjectLogsInput =
                serde_json::from_value(arguments).map_err(|e| e.to_string())?;
            let project = db::compose_projects::find_by_id(&state.db, input.project_id)
                .await
                .map_err(|e| e.to_string())?
                .ok_or("Compose project not found")?;
            let logs = state
                .agent
                .compose_project_logs(panel_core::adapters::ComposeProjectLogsParams {
                    project_slug: project.slug,
                    tail_lines: input.tail_lines.unwrap_or(200),
                })
                .await
                .map_err(|e| e.to_string())?;
            Ok(json!({ "logs": logs }))
        }
        "agent_capabilities" => {
            let caps = state
                .agent
                .capabilities()
                .await
                .map_err(|e| e.to_string())?;
            Ok(serde_json::to_value(caps).unwrap())
        }
        other => Err(format!("unknown tool: {other}")),
    }
}
