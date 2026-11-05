#![forbid(unsafe_code)]

//! panel-agent: root-level host operations server.
//!
//! Listens on a Unix domain socket and executes typed, pre-validated
//! requests from `panel-api` by shelling out exclusively via
//! `Command::new(..).arg(..)` (see `adapters::exec`). Never parses or
//! constructs shell command strings.

mod adapters;
mod config;
mod remote;

use adapters::LocalAdapter;
use clap::{Parser, Subcommand};
use config::AgentConfig;
use panel_core::adapters::{AdapterPort, LogLines};
use panel_core::agent_protocol::{AgentRequest, AgentResponse, StreamFrame};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;

#[derive(Parser, Debug)]
#[command(
    name = "panel-agent",
    about = "scontrol privileged host-operations agent"
)]
struct Args {
    /// Path to agent.toml
    #[arg(long, default_value = config::DEFAULT_CONFIG_PATH, global = true)]
    config: PathBuf,

    #[command(subcommand)]
    command: Option<HelperCommand>,
}

#[derive(Subcommand, Debug)]
enum HelperCommand {
    /// Runs one scheduled job; started by its generated systemd service.
    #[command(name = "run-scheduled-job", hide = true)]
    RunScheduledJob {
        project_slug: String,
        job_id: uuid::Uuid,
    },
    /// Creates this host's remote-agent key (0600) and prints a certificate
    /// signing request to paste into the panel's Servers screen.
    #[command(name = "remote-init")]
    RemoteInit {
        /// The hostname or IP address the panel connects to.
        #[arg(long)]
        host: String,
        #[arg(long, default_value = config::DEFAULT_REMOTE_DIR)]
        dir: PathBuf,
        /// Replace an existing key (certificate rotation).
        #[arg(long)]
        force: bool,
    },
}

fn main() -> anyhow::Result<()> {
    // Git runs this binary back as its askpass/ssh helper during private
    // clones (see `adapters::git_auth`). Checked before logging starts,
    // because an askpass helper answers on stdout.
    #[cfg(feature = "docker")]
    if let Some(result) = adapters::git_auth::run_as_git_helper() {
        return result;
    }
    let args = Args::parse();
    match args.command {
        Some(HelperCommand::RunScheduledJob {
            ref project_slug,
            job_id,
        }) => run_scheduled_job(&args.config, project_slug, job_id),
        Some(HelperCommand::RemoteInit {
            ref host,
            ref dir,
            force,
        }) => {
            let csr = remote::init(host, dir, force)?;
            println!("{csr}");
            eprintln!(
                "Key written to {}. Paste the request above into the panel, then save the \
                 returned certificate as agent.pem and the CA as ca.pem in the same directory.",
                dir.join("agent.key").display()
            );
            Ok(())
        }
        None => serve(args),
    }
}

/// Helper mode for a job's systemd service: one run, recorded next to its
/// log. Exits non-zero when the run failed, so the unit shows as failed.
fn run_scheduled_job(
    config_path: &std::path::Path,
    project_slug: &str,
    job_id: uuid::Uuid,
) -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env().add_directive("info".parse()?),
        )
        .init();
    #[cfg(feature = "docker")]
    {
        let config = AgentConfig::load(config_path)?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let status = runtime.block_on(adapters::scheduled_jobs::execute(
            &config,
            project_slug,
            job_id,
        ))?;
        tracing::info!(%job_id, %status, "scheduled job finished");
        // An error from `main` is a non-zero exit, so systemd marks the unit
        // failed; the run record and log carry the details.
        if status != panel_core::schedule::JobRunStatus::Succeeded {
            anyhow::bail!("scheduled job {job_id} {status}");
        }
        Ok(())
    }
    #[cfg(not(feature = "docker"))]
    {
        let _ = (config_path, project_slug, job_id);
        anyhow::bail!("scheduled jobs need an agent built with the 'docker' feature")
    }
}

#[tokio::main]
async fn serve(args: Args) -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env().add_directive("info".parse()?),
        )
        .json()
        .init();

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
    adapters::metrics::spawn(
        adapter.metrics.clone(),
        adapter.config.metrics_interval_secs,
    );

    if let Some(remote) = adapter.config.remote.clone() {
        let remote_adapter = adapter.clone();
        tokio::spawn(async move {
            let served = remote::serve(remote, move |stream| {
                let (read_half, write_half) = tokio::io::split(stream);
                handle_connection(
                    read_half,
                    write_half,
                    remote_adapter.clone(),
                    Some(remote::IDLE_TIMEOUT),
                )
            })
            .await;
            if let Err(error) = served {
                tracing::error!(%error, "remote listener stopped");
            }
        });
    }

    loop {
        let (stream, _addr) = listener.accept().await?;
        let adapter = adapter.clone();
        tokio::spawn(async move {
            let (read_half, write_half) = stream.into_split();
            if let Err(e) = handle_connection(read_half, write_half, adapter, None).await {
                tracing::warn!(error = %e, "connection handler error");
            }
        });
    }
}

/// Serves requests on one connection until the client closes it. With
/// `idle`, a connection that sends nothing for that long is closed (remote
/// clients keep connections open for reuse).
async fn handle_connection<R, W>(
    read_half: R,
    mut write_half: W,
    adapter: Arc<LocalAdapter>,
    idle: Option<std::time::Duration>,
) -> anyhow::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut reader = BufReader::new(read_half);
    let mut line = String::new();

    loop {
        line.clear();
        let n = match idle {
            Some(limit) => match tokio::time::timeout(limit, reader.read_line(&mut line)).await {
                Ok(read) => read?,
                Err(_) => return Ok(()),
            },
            None => reader.read_line(&mut line).await?,
        };
        if n == 0 {
            return Ok(());
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let response = match serde_json::from_str::<AgentRequest>(trimmed) {
            Ok(AgentRequest::FollowComposeLogs(params)) => {
                let lines = adapter.follow_compose_logs(params).await;
                return stream_lines(lines, reader, write_half).await;
            }
            Ok(request) => dispatch(&adapter, request).await,
            Err(e) => AgentResponse::Err {
                kind: "invalid_request".into(),
                message: format!("failed to parse request: {e}"),
            },
        };

        write_json(&mut write_half, &response).await?;
    }
}

async fn write_json<T: serde::Serialize, W: AsyncWrite + Unpin>(
    write_half: &mut W,
    value: &T,
) -> anyhow::Result<()> {
    let mut serialized = serde_json::to_string(value)?;
    serialized.push('\n');
    write_half.write_all(serialized.as_bytes()).await?;
    // TLS buffers writes; the Unix socket flush is a no-op.
    write_half.flush().await?;
    Ok(())
}

/// Writes a streamed response (see `agent_protocol`) and closes the
/// connection. A client that hangs up, detected by EOF on its side of the
/// socket or by a failed write, drops `lines`, which stops the follower.
async fn stream_lines<R, W>(
    lines: panel_core::error::CoreResult<LogLines>,
    mut reader: BufReader<R>,
    mut write_half: W,
) -> anyhow::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut lines = match lines {
        Ok(lines) => lines,
        Err(error) => {
            let response = AgentResponse::from_core_result::<()>(Err(error));
            return write_json(&mut write_half, &response).await;
        }
    };
    write_json(&mut write_half, &AgentResponse::ok(&())).await?;
    let mut ignored = String::new();
    let end = loop {
        let item = tokio::select! {
            item = lines.recv() => item,
            read = reader.read_line(&mut ignored) => match read {
                Ok(0) | Err(_) => return Ok(()),
                Ok(_) => { ignored.clear(); continue; }
            },
        };
        match item {
            Some(Ok(text)) => write_json(&mut write_half, &StreamFrame::Line { text }).await?,
            Some(Err(error)) => {
                break StreamFrame::End {
                    error: Some(error.message().to_string()),
                }
            }
            None => break StreamFrame::End { error: None },
        }
    };
    write_json(&mut write_half, &end).await
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
        AgentRequest::WriteComposeEnvironment(p) => {
            AgentResponse::from_core_result(adapter.write_compose_environment(p).await)
        }
        AgentRequest::DeployComposeProject(p) => {
            AgentResponse::from_core_result(adapter.deploy_compose_project(p).await)
        }
        AgentRequest::GenerateDeployKey(p) => {
            AgentResponse::from_core_result(adapter.generate_deploy_key(p).await)
        }
        AgentRequest::RemoveDeployKey(p) => {
            AgentResponse::from_core_result(adapter.remove_deploy_key(p).await)
        }
        AgentRequest::WriteGitCredentials(p) => {
            AgentResponse::from_core_result(adapter.write_git_credentials(p).await)
        }
        AgentRequest::WriteDockerConfig(p) => {
            AgentResponse::from_core_result(adapter.write_docker_config(p).await)
        }
        AgentRequest::TestDockerRegistry(p) => {
            AgentResponse::from_core_result(adapter.test_docker_registry(p).await)
        }
        AgentRequest::StopComposeProject(p) => {
            AgentResponse::from_core_result(adapter.stop_compose_project(p).await)
        }
        AgentRequest::RestartComposeProject(p) => {
            AgentResponse::from_core_result(adapter.restart_compose_project(p).await)
        }
        AgentRequest::SetDatabaseExposure(p) => {
            AgentResponse::from_core_result(adapter.set_database_exposure(p).await)
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
        // Streamed requests are handled in `handle_connection`.
        AgentRequest::FollowComposeLogs(_) => AgentResponse::Err {
            kind: "invalid_request".into(),
            message: "follow_compose_logs is a streamed request".into(),
        },
        AgentRequest::CancelDeployment(p) => {
            AgentResponse::from_core_result(adapter.cancel_deployment(p).await)
        }
        AgentRequest::ReadDeploymentLog(p) => {
            AgentResponse::from_core_result(adapter.read_deployment_log(p).await)
        }
        AgentRequest::UpsertScheduledJob(p) => {
            AgentResponse::from_core_result(adapter.upsert_scheduled_job(p).await)
        }
        AgentRequest::RemoveScheduledJob(p) => {
            AgentResponse::from_core_result(adapter.remove_scheduled_job(p).await)
        }
        AgentRequest::RunScheduledJob(p) => {
            AgentResponse::from_core_result(adapter.run_scheduled_job(p).await)
        }
        AgentRequest::ListScheduledJobRuns(p) => {
            AgentResponse::from_core_result(adapter.list_scheduled_job_runs(p).await)
        }
        AgentRequest::ReadScheduledJobRunLog(p) => {
            AgentResponse::from_core_result(adapter.read_scheduled_job_run_log(p).await)
        }
        AgentRequest::ReadNginxStats(p) => {
            AgentResponse::from_core_result(adapter.read_nginx_stats(p).await)
        }
        AgentRequest::PruneDocker(p) => {
            AgentResponse::from_core_result(adapter.prune_docker(p).await)
        }
        AgentRequest::CreateMailbox(p) => {
            AgentResponse::from_core_result(adapter.create_mailbox(p).await)
        }
        AgentRequest::DeleteMailbox(p) => {
            AgentResponse::from_core_result(adapter.delete_mailbox(p).await)
        }
        AgentRequest::ScanPath(p) => AgentResponse::from_core_result(adapter.scan_path(p).await),
        AgentRequest::TestS3Target(p) => {
            AgentResponse::from_core_result(adapter.test_s3_target(p).await)
        }
        AgentRequest::ListBackups(p) => {
            AgentResponse::from_core_result(adapter.list_backups(p).await)
        }
        AgentRequest::BackupDatabase(p) => {
            AgentResponse::from_core_result(adapter.backup_database(p).await)
        }
        AgentRequest::RestoreDatabase(p) => {
            AgentResponse::from_core_result(adapter.restore_database(p).await)
        }
        AgentRequest::BackupVolume(p) => {
            AgentResponse::from_core_result(adapter.backup_volume(p).await)
        }
        AgentRequest::RestoreVolume(p) => {
            AgentResponse::from_core_result(adapter.restore_volume(p).await)
        }
        AgentRequest::MetricsSnapshot => {
            AgentResponse::from_core_result(adapter.metrics_snapshot().await)
        }
        AgentRequest::WriteVhostSecrets(p) => {
            AgentResponse::from_core_result(adapter.write_vhost_secrets(p).await)
        }
        AgentRequest::OpenTerminal(p) => {
            AgentResponse::from_core_result(adapter.open_terminal(p).await)
        }
        AgentRequest::ReadTerminal(p) => {
            AgentResponse::from_core_result(adapter.read_terminal(p).await)
        }
        AgentRequest::WriteTerminal(p) => {
            AgentResponse::from_core_result(adapter.write_terminal(p).await)
        }
        AgentRequest::ResizeTerminal(p) => {
            AgentResponse::from_core_result(adapter.resize_terminal(p).await)
        }
        AgentRequest::CloseTerminal(p) => {
            AgentResponse::from_core_result(adapter.close_terminal(p).await)
        }
    }
}
