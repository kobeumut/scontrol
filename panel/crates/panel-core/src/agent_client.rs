//! Shared agent RPC client implementing [`AdapterPort`] by talking to
//! `panel-agent`. Used by both `panel-api` (per-request, unprivileged) and
//! `panel-cli` (direct host administration). Never builds or runs shell
//! commands itself — see `panel-agent` for the privileged side.
//!
//! Two transports carry the same line protocol: the local Unix socket (also
//! used for an SSH-forwarded socket of a remote host) and mutual TLS to a
//! remote agent (see [`crate::pki`]). Over TLS the agent's certificate must
//! chain to the panel CA, name the configured host, and match one of the
//! fingerprints the panel recorded for that server.
//!
//! Over TLS a connection is kept open after a call and reused by the next
//! one ([`MAX_IDLE`] per client, idle at most [`IDLE_FOR`]), so a remote
//! call usually costs no handshake. Only a request that changes nothing is
//! retried when a reused connection turns out to be closed; anything else
//! reports the failure rather than risk running twice. Streamed requests
//! always use their own connection.

use crate::adapters::*;
use crate::agent_protocol::{AgentRequest, AgentResponse, StreamFrame};
use crate::error::{CoreError, CoreResult};
use crate::schedule::JobRunInfo;
use crate::types::{
    ComposeDeployOutcome, ComposeServiceInfo, ContainerInfo, DeploymentLogChunk, MailboxInfo,
    ScanResult,
};
use async_trait::async_trait;
use serde::de::DeserializeOwned;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::{TcpStream, UnixStream};

type BoxReader = BufReader<Box<dyn AsyncRead + Send + Unpin>>;
type BoxWriter = Box<dyn AsyncWrite + Send + Unpin>;

/// How long connecting (TCP plus TLS handshake) to a remote agent may take.
const REMOTE_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Idle remote connections kept per client.
pub const MAX_IDLE: usize = 4;
/// Idle remote connections older than this are closed instead of reused
/// (the agent closes them after five minutes).
pub const IDLE_FOR: Duration = Duration::from_secs(60);

type Connection = (BoxReader, BoxWriter);

#[derive(Clone)]
enum Transport {
    Unix(PathBuf),
    Tls(Arc<TlsTarget>),
}

/// A remote agent reached over mutual TLS.
pub struct TlsTarget {
    /// `host:port` to connect to.
    pub address: String,
    /// The host the agent certificate must name.
    pub server_name: String,
    pub config: Arc<rustls::ClientConfig>,
    /// SHA-256 fingerprints (hex) the agent's leaf certificate may have:
    /// the current one and, during a rotation, the previous one.
    pub pinned_fingerprints: Vec<String>,
}

#[derive(Clone)]
pub struct AgentClient {
    transport: Transport,
    /// Open connections ready for the next call (TLS only).
    idle: Arc<std::sync::Mutex<Vec<(Connection, std::time::Instant)>>>,
}

impl AgentClient {
    pub fn new(socket_path: impl AsRef<Path>) -> Self {
        Self {
            transport: Transport::Unix(socket_path.as_ref().to_path_buf()),
            idle: Default::default(),
        }
    }

    pub fn tls(target: TlsTarget) -> Self {
        Self {
            transport: Transport::Tls(Arc::new(target)),
            idle: Default::default(),
        }
    }

    /// A pooled connection that has not been idle too long.
    fn take_idle(&self) -> Option<Connection> {
        if !matches!(self.transport, Transport::Tls(_)) {
            return None;
        }
        let mut idle = self.idle.lock().unwrap_or_else(|e| e.into_inner());
        while let Some((connection, since)) = idle.pop() {
            if since.elapsed() < IDLE_FOR {
                return Some(connection);
            }
        }
        None
    }

    fn put_idle(&self, connection: Connection) {
        if !matches!(self.transport, Transport::Tls(_)) {
            return;
        }
        let mut idle = self.idle.lock().unwrap_or_else(|e| e.into_inner());
        idle.retain(|(_, since)| since.elapsed() < IDLE_FOR);
        if idle.len() < MAX_IDLE {
            idle.push((connection, std::time::Instant::now()));
        }
    }

    async fn connect(&self) -> CoreResult<(BoxReader, BoxWriter)> {
        match &self.transport {
            Transport::Unix(path) => {
                let stream = UnixStream::connect(path).await.map_err(|e| {
                    CoreError::External(format!("connecting to agent at {}: {e}", path.display()))
                })?;
                let (read_half, write_half) = stream.into_split();
                Ok((BufReader::new(Box::new(read_half)), Box::new(write_half)))
            }
            Transport::Tls(target) => {
                let stream = tokio::time::timeout(REMOTE_CONNECT_TIMEOUT, connect_tls(target))
                    .await
                    .map_err(|_| {
                        CoreError::External(format!(
                            "connecting to agent at {}: timed out",
                            target.address
                        ))
                    })??;
                let (read_half, write_half) = tokio::io::split(stream);
                Ok((BufReader::new(Box::new(read_half)), Box::new(write_half)))
            }
        }
    }

    /// Writes `request` on `connection` and reads the response line. Errors
    /// are transport failures only (the agent's own error answers are in
    /// the response); `Ok(None)` when the agent had closed the connection.
    async fn exchange(
        connection: &mut Connection,
        request: &AgentRequest,
    ) -> CoreResult<Option<AgentResponse>> {
        let (reader, write_half) = connection;
        let mut payload = serde_json::to_string(request)
            .map_err(|e| CoreError::External(format!("encoding agent request: {e}")))?;
        payload.push('\n');
        write_half
            .write_all(payload.as_bytes())
            .await
            .map_err(|e| CoreError::External(format!("writing to agent: {e}")))?;
        write_half
            .flush()
            .await
            .map_err(|e| CoreError::External(format!("writing to agent: {e}")))?;

        let mut line = String::new();
        let read = reader
            .read_line(&mut line)
            .await
            .map_err(|e| CoreError::External(format!("reading from agent: {e}")))?;
        if read == 0 {
            return Ok(None);
        }
        serde_json::from_str(line.trim())
            .map(Some)
            .map_err(|e| CoreError::External(format!("decoding agent response: {e}")))
    }

    fn closed() -> CoreError {
        CoreError::External("the agent closed the connection".into())
    }

    /// One request on a new connection, which is kept for reuse after a
    /// complete answer.
    async fn fresh_exchange(&self, request: &AgentRequest) -> CoreResult<AgentResponse> {
        let mut connection = self.connect().await?;
        let response = Self::exchange(&mut connection, request)
            .await?
            .ok_or_else(Self::closed)?;
        self.put_idle(connection);
        Ok(response)
    }

    /// Sends `request` on a new connection and reads the first response
    /// line. Returns the connection too, for streamed responses that
    /// continue on it (it is never pooled).
    async fn send(&self, request: AgentRequest) -> CoreResult<(serde_json::Value, Connection)> {
        let mut connection = self.connect().await?;
        let response = Self::exchange(&mut connection, &request)
            .await?
            .ok_or_else(Self::closed)?;
        Ok((response.into_core_result()?, connection))
    }

    async fn call<T: DeserializeOwned>(&self, request: AgentRequest) -> CoreResult<T> {
        let response = match self.take_idle() {
            Some(mut connection) => match Self::exchange(&mut connection, &request).await {
                Ok(Some(response)) => {
                    self.put_idle(connection);
                    response
                }
                // A reused connection that failed: only a request that
                // changes nothing is sent again, because for the rest the
                // agent may have acted before the connection went away.
                _ if is_read_only(&request) => self.fresh_exchange(&request).await?,
                Ok(None) => return Err(Self::closed()),
                Err(error) => return Err(error),
            },
            None => self.fresh_exchange(&request).await?,
        };
        let data = response.into_core_result()?;
        serde_json::from_value(data)
            .map_err(|e| CoreError::External(format!("decoding agent payload: {e}")))
    }

    /// Forwards [`StreamFrame`]s into a channel until `End`, an error, or
    /// the receiver is dropped; the connection is closed in every case,
    /// which is what stops the follower on the agent.
    async fn stream_lines(&self, request: AgentRequest) -> CoreResult<LogLines> {
        let (_, (mut reader, write_half)) = self.send(request).await?;
        let (tx, rx) = tokio::sync::mpsc::channel(LOG_LINES_CAPACITY);
        tokio::spawn(async move {
            let _write_half = write_half;
            let mut line = String::new();
            loop {
                line.clear();
                let read = tokio::select! {
                    read = reader.read_line(&mut line) => read,
                    _ = tx.closed() => return,
                };
                let frame = match read {
                    Ok(0) => Err(CoreError::External("agent closed the log stream".into())),
                    Ok(_) => serde_json::from_str::<StreamFrame>(line.trim()).map_err(|e| {
                        CoreError::External(format!("decoding agent stream frame: {e}"))
                    }),
                    Err(e) => Err(CoreError::External(format!("reading log stream: {e}"))),
                };
                let item = match frame {
                    Ok(StreamFrame::Line { text }) => Ok(text),
                    Ok(StreamFrame::End { error: None }) => return,
                    Ok(StreamFrame::End {
                        error: Some(message),
                    }) => Err(CoreError::External(message)),
                    Err(error) => Err(error),
                };
                let failed = item.is_err();
                if tx.send(item).await.is_err() || failed {
                    return;
                }
            }
        });
        Ok(rx)
    }
}

#[async_trait]
impl AdapterPort for AgentClient {
    async fn capabilities(&self) -> CoreResult<AgentCapabilities> {
        self.call(AgentRequest::Capabilities).await
    }

    async fn create_site(&self, params: CreateSiteParams) -> CoreResult<()> {
        self.call(AgentRequest::CreateSite(params)).await
    }

    async fn delete_site(&self, params: DeleteSiteParams) -> CoreResult<()> {
        self.call(AgentRequest::DeleteSite(params)).await
    }

    async fn reload_web_server(&self) -> CoreResult<()> {
        self.call(AgentRequest::ReloadWebServer).await
    }

    async fn issue_certificate(&self, params: IssueCertificateParams) -> CoreResult<()> {
        self.call(AgentRequest::IssueCertificate(params)).await
    }

    async fn create_database(&self, params: CreateDatabaseParams) -> CoreResult<()> {
        self.call(AgentRequest::CreateDatabase(params)).await
    }

    async fn delete_database(&self, params: DeleteDatabaseParams) -> CoreResult<()> {
        self.call(AgentRequest::DeleteDatabase(params)).await
    }

    async fn create_system_user(&self, params: CreateSystemUserParams) -> CoreResult<()> {
        self.call(AgentRequest::CreateSystemUser(params)).await
    }

    async fn delete_system_user(&self, params: DeleteSystemUserParams) -> CoreResult<()> {
        self.call(AgentRequest::DeleteSystemUser(params)).await
    }

    async fn apply_firewall_rule(&self, params: ApplyFirewallRuleParams) -> CoreResult<()> {
        self.call(AgentRequest::ApplyFirewallRule(params)).await
    }

    async fn upsert_cron_job(&self, params: UpsertCronJobParams) -> CoreResult<()> {
        self.call(AgentRequest::UpsertCronJob(params)).await
    }

    async fn remove_cron_job(&self, params: RemoveCronJobParams) -> CoreResult<()> {
        self.call(AgentRequest::RemoveCronJob(params)).await
    }

    async fn create_container(&self, params: CreateContainerParams) -> CoreResult<ContainerInfo> {
        self.call(AgentRequest::CreateContainer(params)).await
    }

    async fn start_container(&self, params: ContainerActionParams) -> CoreResult<()> {
        self.call(AgentRequest::StartContainer(params)).await
    }

    async fn stop_container(&self, params: ContainerActionParams) -> CoreResult<()> {
        self.call(AgentRequest::StopContainer(params)).await
    }

    async fn remove_container(&self, params: ContainerActionParams) -> CoreResult<()> {
        self.call(AgentRequest::RemoveContainer(params)).await
    }

    async fn list_containers(&self) -> CoreResult<Vec<ContainerInfo>> {
        self.call(AgentRequest::ListContainers).await
    }

    async fn write_compose_environment(
        &self,
        params: WriteComposeEnvironmentParams,
    ) -> CoreResult<()> {
        self.call(AgentRequest::WriteComposeEnvironment(params))
            .await
    }

    async fn deploy_compose_project(
        &self,
        params: DeployComposeProjectParams,
    ) -> CoreResult<ComposeDeployOutcome> {
        self.call(AgentRequest::DeployComposeProject(params)).await
    }

    async fn generate_deploy_key(&self, params: ComposeProjectActionParams) -> CoreResult<String> {
        self.call(AgentRequest::GenerateDeployKey(params)).await
    }

    async fn remove_deploy_key(&self, params: ComposeProjectActionParams) -> CoreResult<()> {
        self.call(AgentRequest::RemoveDeployKey(params)).await
    }

    async fn write_git_credentials(&self, params: WriteGitCredentialsParams) -> CoreResult<()> {
        self.call(AgentRequest::WriteGitCredentials(params)).await
    }

    async fn write_docker_config(&self, params: WriteDockerConfigParams) -> CoreResult<()> {
        self.call(AgentRequest::WriteDockerConfig(params)).await
    }

    async fn test_docker_registry(&self, params: TestDockerRegistryParams) -> CoreResult<()> {
        self.call(AgentRequest::TestDockerRegistry(params)).await
    }

    async fn stop_compose_project(&self, params: ComposeProjectActionParams) -> CoreResult<()> {
        self.call(AgentRequest::StopComposeProject(params)).await
    }

    async fn restart_compose_project(&self, params: ComposeProjectActionParams) -> CoreResult<()> {
        self.call(AgentRequest::RestartComposeProject(params)).await
    }

    async fn set_database_exposure(&self, params: SetDatabaseExposureParams) -> CoreResult<()> {
        self.call(AgentRequest::SetDatabaseExposure(params)).await
    }

    async fn remove_compose_project(&self, params: ComposeProjectActionParams) -> CoreResult<()> {
        self.call(AgentRequest::RemoveComposeProject(params)).await
    }

    async fn list_compose_project_services(
        &self,
        params: ComposeProjectActionParams,
    ) -> CoreResult<Vec<ComposeServiceInfo>> {
        self.call(AgentRequest::ListComposeProjectServices(params))
            .await
    }

    async fn compose_project_logs(&self, params: ComposeProjectLogsParams) -> CoreResult<String> {
        self.call(AgentRequest::ComposeProjectLogs(params)).await
    }

    async fn follow_compose_logs(&self, params: FollowComposeLogsParams) -> CoreResult<LogLines> {
        self.stream_lines(AgentRequest::FollowComposeLogs(params))
            .await
    }

    async fn read_deployment_log(
        &self,
        params: DeploymentLogParams,
    ) -> CoreResult<DeploymentLogChunk> {
        self.call(AgentRequest::ReadDeploymentLog(params)).await
    }

    async fn cancel_deployment(&self, params: CancelDeploymentParams) -> CoreResult<bool> {
        self.call(AgentRequest::CancelDeployment(params)).await
    }

    async fn upsert_scheduled_job(&self, params: UpsertScheduledJobParams) -> CoreResult<()> {
        self.call(AgentRequest::UpsertScheduledJob(params)).await
    }

    async fn remove_scheduled_job(&self, params: ScheduledJobRef) -> CoreResult<()> {
        self.call(AgentRequest::RemoveScheduledJob(params)).await
    }

    async fn run_scheduled_job(&self, params: ScheduledJobRef) -> CoreResult<()> {
        self.call(AgentRequest::RunScheduledJob(params)).await
    }

    async fn list_scheduled_job_runs(
        &self,
        params: ListScheduledJobRunsParams,
    ) -> CoreResult<Vec<JobRunInfo>> {
        self.call(AgentRequest::ListScheduledJobRuns(params)).await
    }

    async fn read_scheduled_job_run_log(
        &self,
        params: ScheduledJobRunLogParams,
    ) -> CoreResult<DeploymentLogChunk> {
        self.call(AgentRequest::ReadScheduledJobRunLog(params))
            .await
    }

    async fn read_nginx_stats(&self, params: ReadNginxStatsParams) -> CoreResult<NginxStatsChunk> {
        self.call(AgentRequest::ReadNginxStats(params)).await
    }

    async fn prune_docker(&self, params: PruneDockerParams) -> CoreResult<DockerPruneOutcome> {
        self.call(AgentRequest::PruneDocker(params)).await
    }

    async fn create_mailbox(&self, params: CreateMailboxParams) -> CoreResult<MailboxInfo> {
        self.call(AgentRequest::CreateMailbox(params)).await
    }

    async fn delete_mailbox(&self, params: DeleteMailboxParams) -> CoreResult<()> {
        self.call(AgentRequest::DeleteMailbox(params)).await
    }

    async fn scan_path(&self, params: ScanPathParams) -> CoreResult<ScanResult> {
        self.call(AgentRequest::ScanPath(params)).await
    }

    async fn test_s3_target(&self, params: TestS3Params) -> CoreResult<()> {
        self.call(AgentRequest::TestS3Target(params)).await
    }

    async fn list_backups(&self, params: ListBackupsParams) -> CoreResult<Vec<BackupObject>> {
        self.call(AgentRequest::ListBackups(params)).await
    }

    async fn backup_database(&self, params: BackupDatabaseParams) -> CoreResult<BackupOutcome> {
        self.call(AgentRequest::BackupDatabase(params)).await
    }

    async fn restore_database(&self, params: RestoreDatabaseParams) -> CoreResult<()> {
        self.call(AgentRequest::RestoreDatabase(params)).await
    }

    async fn backup_volume(&self, params: BackupVolumeParams) -> CoreResult<BackupOutcome> {
        self.call(AgentRequest::BackupVolume(params)).await
    }

    async fn restore_volume(&self, params: RestoreVolumeParams) -> CoreResult<()> {
        self.call(AgentRequest::RestoreVolume(params)).await
    }

    async fn metrics_snapshot(&self) -> CoreResult<Option<crate::metrics::MetricsSnapshot>> {
        self.call(AgentRequest::MetricsSnapshot).await
    }
}

/// Requests that only read state, so sending one twice is harmless.
fn is_read_only(request: &AgentRequest) -> bool {
    matches!(
        request,
        AgentRequest::Capabilities
            | AgentRequest::ListContainers
            | AgentRequest::ListComposeProjectServices(_)
            | AgentRequest::ComposeProjectLogs(_)
            | AgentRequest::ReadDeploymentLog(_)
            | AgentRequest::ListScheduledJobRuns(_)
            | AgentRequest::ReadScheduledJobRunLog(_)
            | AgentRequest::ReadNginxStats(_)
            | AgentRequest::ListBackups(_)
            | AgentRequest::MetricsSnapshot
    )
}

async fn connect_tls(target: &TlsTarget) -> CoreResult<tokio_rustls::client::TlsStream<TcpStream>> {
    let server_name = rustls::ServerName::try_from(target.server_name.as_str()).map_err(|_| {
        CoreError::Validation(format!("invalid agent host '{}'", target.server_name))
    })?;
    let tcp = TcpStream::connect(&target.address).await.map_err(|e| {
        CoreError::External(format!("connecting to agent at {}: {e}", target.address))
    })?;
    tcp.set_nodelay(true).ok();
    let stream = tokio_rustls::TlsConnector::from(target.config.clone())
        .connect(server_name, tcp)
        .await
        .map_err(|e| {
            CoreError::External(format!(
                "TLS handshake with agent at {}: {e}",
                target.address
            ))
        })?;
    let leaf = stream
        .get_ref()
        .1
        .peer_certificates()
        .and_then(|certs| certs.first())
        .ok_or_else(|| CoreError::External("agent presented no certificate".into()))?;
    let fingerprint = crate::pki::fingerprint_der(&leaf.0);
    if !target
        .pinned_fingerprints
        .iter()
        .any(|pin| pin.eq_ignore_ascii_case(&fingerprint))
    {
        return Err(CoreError::PermissionDenied(format!(
            "agent at {} presented a certificate the panel did not issue for this server",
            target.address
        )));
    }
    Ok(stream)
}

#[cfg(test)]
mod tests {
    //! A mock remote agent: a TLS 1.3 listener built with the same
    //! `pki::server_config` the real agent uses, answering the line protocol.
    use super::*;
    use crate::pki::{self, KeyedCert};
    use tokio::net::TcpListener;

    struct Pki {
        ca: KeyedCert,
        agent: KeyedCert,
        agent_fingerprint: String,
        client: KeyedCert,
    }

    fn pki_for(host: &str) -> Pki {
        let ca = pki::generate_ca().unwrap();
        let request = pki::generate_agent_csr(host).unwrap();
        let (cert_pem, info) =
            pki::sign_agent_csr(&ca, &request.cert_pem, host, uuid::Uuid::nil(), 1).unwrap();
        let (client, _) = pki::issue_client_cert(&ca, 1).unwrap();
        Pki {
            ca,
            agent: KeyedCert {
                cert_pem,
                key_pem: request.key_pem,
            },
            agent_fingerprint: info.fingerprint,
            client,
        }
    }

    struct MockAgent {
        address: String,
        /// Requests read.
        served: Arc<std::sync::atomic::AtomicUsize>,
        /// TLS connections accepted.
        connections: Arc<std::sync::atomic::AtomicUsize>,
    }

    /// Serves until dropped, like the agent: many requests per connection
    /// (or one, then a close, with `one_shot`). Answers `Capabilities`;
    /// everything else gets a `not_found` error answer.
    async fn mock_agent_with(ca_pem: &str, identity: &KeyedCert, one_shot: bool) -> MockAgent {
        use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
        let config = pki::server_config(ca_pem, identity).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let (served, connections) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let (served_count, connection_count) = (served.clone(), connections.clone());
        tokio::spawn(async move {
            let acceptor = tokio_rustls::TlsAcceptor::from(config);
            loop {
                let (tcp, _) = listener.accept().await.unwrap();
                let acceptor = acceptor.clone();
                let (served, connections) = (served_count.clone(), connection_count.clone());
                tokio::spawn(async move {
                    let Ok(stream) = acceptor.accept(tcp).await else {
                        return;
                    };
                    connections.fetch_add(1, SeqCst);
                    let (read_half, mut write_half) = tokio::io::split(stream);
                    let mut reader = BufReader::new(read_half);
                    loop {
                        let mut line = String::new();
                        if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                            return;
                        }
                        served.fetch_add(1, SeqCst);
                        let response = match serde_json::from_str::<AgentRequest>(line.trim()) {
                            Ok(AgentRequest::Capabilities) => {
                                AgentResponse::ok(&AgentCapabilities {
                                    docker: true,
                                    mail: false,
                                    antivirus: false,
                                    version: "remote-mock".into(),
                                    build_tools: Vec::new(),
                                })
                            }
                            _ => AgentResponse::Err {
                                kind: "not_found".into(),
                                message: "unsupported in mock".into(),
                            },
                        };
                        let mut out = serde_json::to_string(&response).unwrap();
                        out.push('\n');
                        write_half.write_all(out.as_bytes()).await.unwrap();
                        write_half.flush().await.unwrap();
                        if one_shot {
                            write_half.shutdown().await.ok();
                            return;
                        }
                    }
                });
            }
        });
        MockAgent {
            address,
            served,
            connections,
        }
    }

    async fn mock_agent(
        ca_pem: &str,
        identity: &KeyedCert,
    ) -> (String, Arc<std::sync::atomic::AtomicUsize>) {
        let agent = mock_agent_with(ca_pem, identity, false).await;
        (agent.address, agent.served)
    }

    fn client(
        address: &str,
        host: &str,
        ca_pem: &str,
        identity: &KeyedCert,
        pins: Vec<String>,
    ) -> AgentClient {
        AgentClient::tls(TlsTarget {
            address: address.to_string(),
            server_name: host.to_string(),
            config: pki::client_config(ca_pem, identity).unwrap(),
            pinned_fingerprints: pins,
        })
    }

    #[tokio::test]
    async fn mutual_tls_round_trip() {
        let pki = pki_for("127.0.0.1");
        let (address, served) = mock_agent(&pki.ca.cert_pem, &pki.agent).await;
        let agent = client(
            &address,
            "127.0.0.1",
            &pki.ca.cert_pem,
            &pki.client,
            vec![pki.agent_fingerprint.clone()],
        );
        let capabilities = agent.capabilities().await.unwrap();
        assert_eq!(capabilities.version, "remote-mock");
        // Errors travel back as the matching core error.
        let error = agent.list_containers().await.unwrap_err();
        assert!(matches!(error, CoreError::NotFound(_)));
        assert_eq!(served.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn an_unpinned_agent_certificate_is_refused() {
        let pki = pki_for("127.0.0.1");
        let (address, served) = mock_agent(&pki.ca.cert_pem, &pki.agent).await;
        // Valid chain and name, but not the certificate recorded for the server.
        let agent = client(
            &address,
            "127.0.0.1",
            &pki.ca.cert_pem,
            &pki.client,
            vec!["00".repeat(32)],
        );
        let error = agent.capabilities().await.unwrap_err();
        assert!(matches!(error, CoreError::PermissionDenied(_)), "{error}");
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(served.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn the_agent_must_name_the_configured_host() {
        let pki = pki_for("agent-a.example.com");
        let (address, _) = mock_agent(&pki.ca.cert_pem, &pki.agent).await;
        let agent = client(
            &address,
            "agent-b.example.com",
            &pki.ca.cert_pem,
            &pki.client,
            vec![pki.agent_fingerprint.clone()],
        );
        assert!(agent.capabilities().await.is_err());
    }

    #[tokio::test]
    async fn a_client_from_another_ca_is_rejected_by_the_agent() {
        let pki = pki_for("127.0.0.1");
        let (address, served) = mock_agent(&pki.ca.cert_pem, &pki.agent).await;
        let other_ca = pki::generate_ca().unwrap();
        let (foreign_client, _) = pki::issue_client_cert(&other_ca, 1).unwrap();
        // Trusts the right CA for the server, but presents a foreign identity.
        let agent = AgentClient::tls(TlsTarget {
            address: address.clone(),
            server_name: "127.0.0.1".into(),
            config: {
                let mut roots = rustls::RootCertStore::empty();
                for der in rustls_pemfile::certs(&mut pki.ca.cert_pem.as_bytes()).unwrap() {
                    roots.add(&rustls::Certificate(der)).unwrap();
                }
                let cert = rustls_pemfile::certs(&mut foreign_client.cert_pem.as_bytes()).unwrap();
                let key =
                    rustls_pemfile::pkcs8_private_keys(&mut foreign_client.key_pem.as_bytes())
                        .unwrap();
                Arc::new(
                    rustls::ClientConfig::builder()
                        .with_safe_defaults()
                        .with_root_certificates(roots)
                        .with_client_auth_cert(
                            cert.into_iter().map(rustls::Certificate).collect(),
                            rustls::PrivateKey(key[0].clone()),
                        )
                        .unwrap(),
                )
            },
            pinned_fingerprints: vec![pki.agent_fingerprint.clone()],
        });
        assert!(agent.capabilities().await.is_err());
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(served.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn an_agent_certificate_cannot_act_as_the_panel() {
        // A compromised agent host holds a CA-signed certificate; it carries
        // only `serverAuth`, so another agent refuses it as a client.
        let pki = pki_for("127.0.0.1");
        let (address, served) = mock_agent(&pki.ca.cert_pem, &pki.agent).await;
        let agent = client(
            &address,
            "127.0.0.1",
            &pki.ca.cert_pem,
            &pki.agent,
            vec![pki.agent_fingerprint.clone()],
        );
        assert!(agent.capabilities().await.is_err());
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(served.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn an_unreachable_agent_fails_cleanly() {
        let pki = pki_for("127.0.0.1");
        let agent = client(
            "127.0.0.1:9",
            "127.0.0.1",
            &pki.ca.cert_pem,
            &pki.client,
            vec![pki.agent_fingerprint],
        );
        assert!(matches!(
            agent.capabilities().await,
            Err(CoreError::External(_))
        ));
    }

    #[tokio::test]
    async fn remote_connections_are_reused() {
        use std::sync::atomic::Ordering::SeqCst;
        let pki = pki_for("127.0.0.1");
        let mock = mock_agent_with(&pki.ca.cert_pem, &pki.agent, false).await;
        let agent = client(
            &mock.address,
            "127.0.0.1",
            &pki.ca.cert_pem,
            &pki.client,
            vec![pki.agent_fingerprint.clone()],
        );
        for _ in 0..5 {
            agent.capabilities().await.unwrap();
        }
        // An error answer leaves the connection usable too.
        assert!(matches!(
            agent.list_containers().await,
            Err(CoreError::NotFound(_))
        ));
        agent.capabilities().await.unwrap();
        assert_eq!(mock.served.load(SeqCst), 7);
        assert_eq!(
            mock.connections.load(SeqCst),
            1,
            "one handshake for every call"
        );
    }

    #[tokio::test]
    async fn a_closed_pooled_connection_is_retried_only_for_reads() {
        use std::sync::atomic::Ordering::SeqCst;
        let pki = pki_for("127.0.0.1");
        // Closes every connection after one answer, like a restarted agent.
        let mock = mock_agent_with(&pki.ca.cert_pem, &pki.agent, true).await;
        let agent = client(
            &mock.address,
            "127.0.0.1",
            &pki.ca.cert_pem,
            &pki.client,
            vec![pki.agent_fingerprint.clone()],
        );
        agent.capabilities().await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        // The pooled connection is dead; a read goes again on a new one.
        agent.capabilities().await.unwrap();
        assert_eq!(mock.served.load(SeqCst), 2);
        assert_eq!(mock.connections.load(SeqCst), 2);

        // A change is not sent twice: the failure is reported instead.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let stop = agent
            .stop_compose_project(ComposeProjectActionParams {
                project_slug: "web".into(),
            })
            .await;
        assert!(matches!(stop, Err(CoreError::External(_))), "{stop:?}");
        assert_eq!(mock.served.load(SeqCst), 2, "the agent never saw the stop");
        // The next call opens a fresh connection and works.
        agent.capabilities().await.unwrap();
        assert_eq!(mock.connections.load(SeqCst), 3);
    }
}
