#![forbid(unsafe_code)]

//! panel-agent: root-level host operations server.
//!
//! Listens on a Unix domain socket and executes typed, pre-validated
//! requests from `panel-api` by shelling out exclusively via
//! `Command::new(..).arg(..)` (see `adapters::exec`). Never parses or
//! constructs shell command strings.

mod adapters;
mod config;

use adapters::LocalAdapter;
use clap::Parser;
use config::AgentConfig;
use panel_core::adapters::AdapterPort;
use panel_core::agent_protocol::{AgentRequest, AgentResponse};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

#[derive(Parser, Debug)]
#[command(
    name = "panel-agent",
    about = "scontrol privileged host-operations agent"
)]
struct Args {
    /// Path to agent.toml
    #[arg(long, default_value = "/etc/scontrol/agent.toml")]
    config: PathBuf,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env().add_directive("info".parse()?),
        )
        .json()
        .init();

    let args = Args::parse();
    let config = AgentConfig::load(&args.config)?;
    tracing::info!(socket = %config.socket_path.display(), "starting panel-agent");

    if let Some(parent) = config.socket_path.parent() {
        tokio::fs::create_dir_all(parent).await.ok();
    }
    let _ = tokio::fs::remove_file(&config.socket_path).await;

    let listener = UnixListener::bind(&config.socket_path)?;
    // Restrict the socket to root + the `scontrol` group (panel-api runs as
    // that group). 0660: rw for owner/group, nothing for others.
    tokio::fs::set_permissions(&config.socket_path, std::fs::Permissions::from_mode(0o660)).await?;

    let adapter = Arc::new(LocalAdapter::new(config));

    loop {
        let (stream, _addr) = listener.accept().await?;
        let adapter = adapter.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_connection(stream, adapter).await {
                tracing::warn!(error = %e, "connection handler error");
            }
        });
    }
}

async fn handle_connection(stream: UnixStream, adapter: Arc<LocalAdapter>) -> anyhow::Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let mut line = String::new();

    loop {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            return Ok(());
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let response = match serde_json::from_str::<AgentRequest>(trimmed) {
            Ok(request) => dispatch(&adapter, request).await,
            Err(e) => AgentResponse::Err {
                kind: "invalid_request".into(),
                message: format!("failed to parse request: {e}"),
            },
        };

        let mut serialized = serde_json::to_string(&response)?;
        serialized.push('\n');
        write_half.write_all(serialized.as_bytes()).await?;
    }
}

async fn dispatch(adapter: &LocalAdapter, request: AgentRequest) -> AgentResponse {
    match request {
        AgentRequest::Capabilities => AgentResponse::from_core_result(adapter.capabilities().await),
        AgentRequest::CreateSite(p) => {
            AgentResponse::from_core_result(adapter.create_site(p).await)
        }
        AgentRequest::DeleteSite(p) => {
            AgentResponse::from_core_result(adapter.delete_site(p).await)
        }
        AgentRequest::ReloadWebServer => {
            AgentResponse::from_core_result(adapter.reload_web_server().await)
        }
        AgentRequest::IssueCertificate(p) => {
            AgentResponse::from_core_result(adapter.issue_certificate(p).await)
        }
        AgentRequest::CreateDatabase(p) => {
            AgentResponse::from_core_result(adapter.create_database(p).await)
        }
        AgentRequest::DeleteDatabase(p) => {
            AgentResponse::from_core_result(adapter.delete_database(p).await)
        }
        AgentRequest::CreateSystemUser(p) => {
            AgentResponse::from_core_result(adapter.create_system_user(p).await)
        }
        AgentRequest::DeleteSystemUser(p) => {
            AgentResponse::from_core_result(adapter.delete_system_user(p).await)
        }
        AgentRequest::ApplyFirewallRule(p) => {
            AgentResponse::from_core_result(adapter.apply_firewall_rule(p).await)
        }
        AgentRequest::UpsertCronJob(p) => {
            AgentResponse::from_core_result(adapter.upsert_cron_job(p).await)
        }
        AgentRequest::RemoveCronJob(p) => {
            AgentResponse::from_core_result(adapter.remove_cron_job(p).await)
        }
        AgentRequest::CreateContainer(p) => {
            AgentResponse::from_core_result(adapter.create_container(p).await)
        }
        AgentRequest::StartContainer(p) => {
            AgentResponse::from_core_result(adapter.start_container(p).await)
        }
        AgentRequest::StopContainer(p) => {
            AgentResponse::from_core_result(adapter.stop_container(p).await)
        }
        AgentRequest::RemoveContainer(p) => {
            AgentResponse::from_core_result(adapter.remove_container(p).await)
        }
        AgentRequest::ListContainers => {
            AgentResponse::from_core_result(adapter.list_containers().await)
        }
        AgentRequest::DeployComposeProject(p) => {
            AgentResponse::from_core_result(adapter.deploy_compose_project(p).await)
        }
        AgentRequest::StopComposeProject(p) => {
            AgentResponse::from_core_result(adapter.stop_compose_project(p).await)
        }
        AgentRequest::RemoveComposeProject(p) => {
            AgentResponse::from_core_result(adapter.remove_compose_project(p).await)
        }
        AgentRequest::ListComposeProjectServices(p) => {
            AgentResponse::from_core_result(adapter.list_compose_project_services(p).await)
        }
        AgentRequest::ComposeProjectLogs(p) => {
            AgentResponse::from_core_result(adapter.compose_project_logs(p).await)
        }
        AgentRequest::CreateMailbox(p) => {
            AgentResponse::from_core_result(adapter.create_mailbox(p).await)
        }
        AgentRequest::DeleteMailbox(p) => {
            AgentResponse::from_core_result(adapter.delete_mailbox(p).await)
        }
        AgentRequest::ScanPath(p) => AgentResponse::from_core_result(adapter.scan_path(p).await),
    }
}
