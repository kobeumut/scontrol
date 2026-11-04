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
}

impl AgentClient {
    pub fn new(socket_path: impl AsRef<Path>) -> Self {
        Self {
            transport: Transport::Unix(socket_path.as_ref().to_path_buf()),
        }
    }

    pub fn tls(target: TlsTarget) -> Self {
        Self {
            transport: Transport::Tls(Arc::new(target)),
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

    /// Sends `request` and reads the first response line. Returns the
    /// reader too, for streamed responses that continue on the connection.
    async fn send(
        &self,
        request: AgentRequest,
    ) -> CoreResult<(serde_json::Value, BoxReader, BoxWriter)> {
        let (mut reader, mut write_half) = self.connect().await?;

        let mut payload = serde_json::to_string(&request)
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
        reader
            .read_line(&mut line)
            .await
            .map_err(|e| CoreError::External(format!("reading from agent: {e}")))?;

        let response: AgentResponse = serde_json::from_str(line.trim())
            .map_err(|e| CoreError::External(format!("decoding agent response: {e}")))?;
        Ok((response.into_core_result()?, reader, write_half))
    }

    async fn call<T: DeserializeOwned>(&self, request: AgentRequest) -> CoreResult<T> {
        let (data, _, _) = self.send(request).await?;
        serde_json::from_value(data)
            .map_err(|e| CoreError::External(format!("decoding agent payload: {e}")))
    }

    /// Forwards [`StreamFrame`]s into a channel until `End`, an error, or
    /// the receiver is dropped; the connection is closed in every case,
    /// which is what stops the follower on the agent.
    async fn stream_lines(&self, request: AgentRequest) -> CoreResult<LogLines> {
        let (_, mut reader, write_half) = self.send(request).await?;
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

    /// Serves until dropped; answers `Capabilities` and records whether a
    /// request ever got through the handshake.
    async fn mock_agent(
        ca_pem: &str,
        identity: &KeyedCert,
    ) -> (String, Arc<std::sync::atomic::AtomicUsize>) {
        let config = pki::server_config(ca_pem, identity).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let served = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = served.clone();
        tokio::spawn(async move {
            let acceptor = tokio_rustls::TlsAcceptor::from(config);
            loop {
                let (tcp, _) = listener.accept().await.unwrap();
                let acceptor = acceptor.clone();
                let counter = counter.clone();
                tokio::spawn(async move {
                    let Ok(stream) = acceptor.accept(tcp).await else {
                        return;
                    };
                    let (read_half, mut write_half) = tokio::io::split(stream);
                    let mut reader = BufReader::new(read_half);
                    let mut line = String::new();
                    if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                        return;
                    }
                    counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let response = match serde_json::from_str::<AgentRequest>(line.trim()) {
                        Ok(AgentRequest::Capabilities) => AgentResponse::ok(&AgentCapabilities {
                            docker: true,
                            mail: false,
                            antivirus: false,
                            version: "remote-mock".into(),
                            build_tools: Vec::new(),
                        }),
                        _ => AgentResponse::Err {
                            kind: "not_found".into(),
                            message: "unsupported in mock".into(),
                        },
                    };
                    let mut out = serde_json::to_string(&response).unwrap();
                    out.push('\n');
                    write_half.write_all(out.as_bytes()).await.unwrap();
                    write_half.flush().await.unwrap();
                });
            }
        });
        (address, served)
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
}
