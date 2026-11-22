#![forbid(unsafe_code)]

//! A stand-in for `panel-agent` that serves the agent line protocol from
//! [`MockAdapter`], so the panel can be driven end to end (see `panel/e2e`)
//! on a machine without root, Nginx, or Docker. It touches nothing on the
//! host. Built only with the `test-utils` feature; never installed.
//!
//! [`MockAdapter`] answers every request with a fixed result. On top of it
//! this binary keeps a little pretend host state ([`Host`]) so screens that
//! read back what an action did have something to show: the services of a
//! deployed project, a deployment log, the run of a job started by hand,
//! stored backups, metrics, and an echoing terminal.
//!
//! A raw Compose file containing the comment `# e2e: slow` deploys for up to
//! 30 seconds, so a test can watch a running deployment and cancel it.

use chrono::Utc;
use panel_core::adapters::mock::MockAdapter;
use panel_core::adapters::{AdapterPort, BackupObject, TerminalOutput};
use panel_core::agent_protocol::{dispatch, AgentRequest, AgentResponse, StreamFrame};
use panel_core::error::CoreError;
use panel_core::metrics::{ContainerMetrics, DiskUsage, HostMetrics, LoadAvg, MetricsSnapshot};
use panel_core::schedule::{JobRunInfo, JobRunStatus, JobRunTrigger};
use panel_core::types::{ComposeServiceInfo, ComposeSource, DeploymentLogChunk};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use uuid::Uuid;

const SLOW_MARKER: &str = "# e2e: slow";
const DEPLOY_LOG: &str =
    "Pulling images\nCreating network\nStarting containers\nDeployment finished\n";

#[derive(Default)]
struct Host {
    /// Running services by project slug.
    services: Mutex<HashMap<String, Vec<ComposeServiceInfo>>>,
    /// Slow deployments in progress, woken by a cancellation.
    slow_deploys: Mutex<HashMap<Uuid, Arc<tokio::sync::Notify>>>,
    backups: Mutex<Vec<BackupObject>>,
    /// Base64 output waiting to be read, by terminal session.
    terminals: Mutex<HashMap<Uuid, Vec<String>>>,
}

struct Agent {
    mock: MockAdapter,
    host: Host,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let socket = std::env::args()
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("usage: scontrol-mock-agent <socket path>"))?;
    let _ = std::fs::remove_file(&socket);
    let listener = UnixListener::bind(&socket)?;
    eprintln!("mock agent listening on {socket}");
    let agent = Arc::new(Agent {
        mock: MockAdapter::new(),
        host: Host::default(),
    });
    loop {
        let (stream, _) = listener.accept().await?;
        let agent = agent.clone();
        tokio::spawn(async move {
            if let Err(error) = serve(stream, agent).await {
                eprintln!("mock agent connection error: {error}");
            }
        });
    }
}

/// Service names a deployment of `source` would start.
fn service_names(source: &ComposeSource) -> Vec<String> {
    match source {
        ComposeSource::Raw { compose_yaml } => {
            // Enough YAML for a test file: the keys directly under `services:`.
            let mut names = Vec::new();
            let mut inside = false;
            for line in compose_yaml.lines() {
                if line.trim_end() == "services:" {
                    inside = true;
                } else if inside && !line.starts_with(' ') && !line.trim().is_empty() {
                    inside = false;
                } else if inside {
                    let indent = line.len() - line.trim_start().len();
                    if let Some(name) = line.trim().strip_suffix(':') {
                        if indent == 2 && !name.starts_with('#') {
                            names.push(name.to_string());
                        }
                    }
                }
            }
            names
        }
        ComposeSource::Application(spec) => vec![spec.service_name.clone()],
        ComposeSource::Git { .. } => vec!["web".into()],
        ComposeSource::Database(_) => vec!["db".into()],
        ComposeSource::Static(_) => vec![],
    }
}

fn running_service(slug: &str, name: &str) -> ComposeServiceInfo {
    ComposeServiceInfo {
        name: name.to_string(),
        container: format!("{slug}-{name}-1"),
        image: "mock-image:latest".into(),
        state: "running".into(),
        status: "Up 1 second".into(),
        health: String::new(),
        exit_code: 0,
        ports: vec!["80/tcp".into()],
    }
}

fn metrics(host: &Host) -> MetricsSnapshot {
    const GIB: u64 = 1 << 30;
    let containers = host
        .services
        .lock()
        .unwrap()
        .iter()
        .flat_map(|(slug, services)| {
            services.iter().map(move |service| ContainerMetrics {
                container_id: format!("{slug}-{}", service.name),
                name: service.container.clone(),
                project: slug.clone(),
                service: service.name.clone(),
                cpu_pct: 3.5,
                mem_bytes: 96 << 20,
                mem_limit_bytes: Some(512 << 20),
                io_read_bps: 0.0,
                io_write_bps: 1024.0,
            })
        })
        .collect();
    MetricsSnapshot {
        sampled_at: Utc::now(),
        interval_secs: 10.0,
        sample_micros: 300,
        host: HostMetrics {
            cpu_pct: 12.5,
            cpu_count: 4,
            mem_total_bytes: 8 * GIB,
            mem_used_bytes: 3 * GIB,
            swap_total_bytes: 2 * GIB,
            swap_used_bytes: 0,
            load: LoadAvg {
                one: 0.4,
                five: 0.3,
                fifteen: 0.2,
            },
            disk_read_bps: 2048.0,
            disk_write_bps: 8192.0,
            disks: vec![DiskUsage {
                mount_point: "/".into(),
                fs_type: "ext4".into(),
                total_bytes: 100 * GIB,
                used_bytes: 40 * GIB,
            }],
        },
        containers,
    }
}

/// Standard base64 (the terminal protocol carries bytes this way).
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        for (i, shift) in [18, 12, 6, 0].into_iter().enumerate() {
            if i <= chunk.len() {
                out.push(ALPHABET[(n >> shift) as usize & 63] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Answers `request` from the pretend host where it has something better
/// than the mock's fixed result, and from the mock otherwise.
async fn respond(agent: &Agent, request: AgentRequest) -> AgentResponse {
    let host = &agent.host;
    match request {
        AgentRequest::DeployComposeProject(params) => {
            let slow = matches!(&params.source, ComposeSource::Raw { compose_yaml } if compose_yaml.contains(SLOW_MARKER));
            if slow && params.rollback.is_none() {
                let cancelled = Arc::new(tokio::sync::Notify::new());
                host.slow_deploys
                    .lock()
                    .unwrap()
                    .insert(params.deployment_id, cancelled.clone());
                let was_cancelled =
                    tokio::time::timeout(Duration::from_secs(30), cancelled.notified())
                        .await
                        .is_ok();
                host.slow_deploys
                    .lock()
                    .unwrap()
                    .remove(&params.deployment_id);
                if was_cancelled {
                    return AgentResponse::from_core_result::<()>(Err(CoreError::Cancelled(
                        "deployment cancelled".into(),
                    )));
                }
            }
            let slug = params.project_slug.clone();
            let services: Vec<_> = service_names(&params.source)
                .iter()
                .map(|name| running_service(&slug, name))
                .collect();
            let outcome = agent
                .mock
                .deploy_compose_project(params)
                .await
                .map(|mut outcome| {
                    outcome.services = services.clone();
                    outcome
                });
            if outcome.is_ok() {
                host.services.lock().unwrap().insert(slug, services);
            }
            AgentResponse::from_core_result(outcome)
        }
        AgentRequest::CancelDeployment(params) => {
            let running = host
                .slow_deploys
                .lock()
                .unwrap()
                .get(&params.deployment_id)
                .cloned();
            if let Some(running) = &running {
                running.notify_one();
            }
            AgentResponse::from_core_result(Ok(running.is_some()))
        }
        AgentRequest::StopComposeProject(params) => {
            host.services.lock().unwrap().remove(&params.project_slug);
            AgentResponse::from_core_result(agent.mock.stop_compose_project(params).await)
        }
        AgentRequest::RemoveComposeProject(params) => {
            host.services.lock().unwrap().remove(&params.project_slug);
            AgentResponse::from_core_result(agent.mock.remove_compose_project(params).await)
        }
        AgentRequest::ListComposeProjectServices(params) => {
            let services = host
                .services
                .lock()
                .unwrap()
                .get(&params.project_slug)
                .cloned();
            AgentResponse::from_core_result(Ok(services.unwrap_or_default()))
        }
        AgentRequest::ReadDeploymentLog(params) => {
            let offset = (params.offset as usize).min(DEPLOY_LOG.len());
            AgentResponse::from_core_result(Ok(DeploymentLogChunk {
                content: DEPLOY_LOG[offset..].to_string(),
                next_offset: DEPLOY_LOG.len() as u64,
                eof: true,
            }))
        }
        AgentRequest::RunScheduledJob(job) => {
            let now = Utc::now();
            agent.mock.job_runs.lock().unwrap().push(JobRunInfo {
                id: Uuid::new_v4(),
                job_id: job.job_id,
                trigger: JobRunTrigger::Manual,
                status: JobRunStatus::Succeeded,
                started_at: now,
                finished_at: Some(now),
                exit_code: Some(0),
                error_message: None,
                log_bytes: 16,
                log_truncated: false,
                updated_at: now,
            });
            AgentResponse::from_core_result(agent.mock.run_scheduled_job(job).await)
        }
        AgentRequest::BackupDatabase(params) => {
            let outcome = agent.mock.backup_database(params).await;
            remember_backup(host, outcome.as_ref().ok().map(|o| o.key.clone()));
            AgentResponse::from_core_result(outcome)
        }
        AgentRequest::BackupVolume(params) => {
            let outcome = agent.mock.backup_volume(params).await;
            remember_backup(host, outcome.as_ref().ok().map(|o| o.key.clone()));
            AgentResponse::from_core_result(outcome)
        }
        AgentRequest::ListBackups(params) => {
            let prefix = params.prefix.trim_end_matches('/').to_string();
            let objects: Vec<_> = host
                .backups
                .lock()
                .unwrap()
                .iter()
                .filter(|object| object.key.starts_with(&prefix))
                .cloned()
                .collect();
            AgentResponse::from_core_result(Ok(objects))
        }
        AgentRequest::MetricsSnapshot => AgentResponse::from_core_result(Ok(Some(metrics(host)))),
        AgentRequest::OpenTerminal(params) => {
            let opened = agent.mock.open_terminal(params).await;
            if let Ok(session) = &opened {
                host.terminals
                    .lock()
                    .unwrap()
                    .insert(session.session_id, vec![base64(b"mock-shell:/# ")]);
            }
            AgentResponse::from_core_result(opened)
        }
        AgentRequest::WriteTerminal(params) => {
            // Echo, like a terminal in cooked mode would.
            if let Some(pending) = host.terminals.lock().unwrap().get_mut(&params.session_id) {
                pending.push(params.data.clone());
            }
            AgentResponse::from_core_result(agent.mock.write_terminal(params).await)
        }
        AgentRequest::ReadTerminal(params) => {
            let next = host
                .terminals
                .lock()
                .unwrap()
                .get_mut(&params.session_id)
                .and_then(|pending| (!pending.is_empty()).then(|| pending.remove(0)));
            match next {
                Some(data) => {
                    AgentResponse::from_core_result(Ok(TerminalOutput { data, exited: None }))
                }
                None => {
                    tokio::time::sleep(Duration::from_millis(params.wait_ms.min(200))).await;
                    AgentResponse::from_core_result(Ok(TerminalOutput::default()))
                }
            }
        }
        AgentRequest::CloseTerminal(params) => {
            host.terminals.lock().unwrap().remove(&params.session_id);
            AgentResponse::from_core_result(agent.mock.close_terminal(params).await)
        }
        other => dispatch(&agent.mock, other).await,
    }
}

fn remember_backup(host: &Host, key: Option<String>) {
    if let Some(key) = key {
        let mut backups = host.backups.lock().unwrap();
        backups.retain(|object| object.key != key);
        backups.push(BackupObject {
            key,
            size_bytes: 2048,
            last_modified: Utc::now().to_rfc3339(),
        });
    }
}

async fn serve(stream: UnixStream, agent: Arc<Agent>) -> anyhow::Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).await? == 0 {
            return Ok(());
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // The mock records every call and request; keep a long-lived process bounded.
        agent.mock.calls.lock().unwrap().clear();
        agent.mock.deploys.lock().unwrap().clear();
        agent.mock.environment_writes.lock().unwrap().clear();
        let response = match serde_json::from_str::<AgentRequest>(trimmed) {
            Ok(AgentRequest::FollowComposeLogs(params)) => {
                let mut lines = match agent.mock.follow_compose_logs(params).await {
                    Ok(lines) => lines,
                    Err(error) => {
                        let response = AgentResponse::from_core_result::<()>(Err(error));
                        return write_json(&mut write_half, &response).await;
                    }
                };
                write_json(&mut write_half, &AgentResponse::ok(&())).await?;
                while let Some(item) = lines.recv().await {
                    match item {
                        Ok(text) => {
                            write_json(&mut write_half, &StreamFrame::Line { text }).await?
                        }
                        Err(error) => {
                            let end = StreamFrame::End {
                                error: Some(error.message().to_string()),
                            };
                            return write_json(&mut write_half, &end).await;
                        }
                    }
                }
                return write_json(&mut write_half, &StreamFrame::End { error: None }).await;
            }
            Ok(request) => respond(&agent, request).await,
            Err(error) => AgentResponse::Err {
                kind: "invalid_request".into(),
                message: format!("failed to parse request: {error}"),
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
    Ok(())
}
