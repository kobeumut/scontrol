//! Shared Unix-socket RPC client implementing [`AdapterPort`] by talking to
//! `panel-agent`. Used by both `panel-api` (per-request, unprivileged) and
//! `panel-cli` (direct host administration). Never builds or runs shell
//! commands itself — see `panel-agent` for the privileged side.

use crate::adapters::*;
use crate::agent_protocol::{AgentRequest, AgentResponse, StreamFrame};
use crate::error::{CoreError, CoreResult};
use crate::types::{
    ComposeDeployOutcome, ComposeServiceInfo, ContainerInfo, DeploymentLogChunk, MailboxInfo,
    ScanResult,
};
use async_trait::async_trait;
use serde::de::DeserializeOwned;
use std::path::{Path, PathBuf};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;

#[derive(Clone)]
pub struct AgentClient {
    socket_path: PathBuf,
}

impl AgentClient {
    pub fn new(socket_path: impl AsRef<Path>) -> Self {
        Self {
            socket_path: socket_path.as_ref().to_path_buf(),
        }
    }

    /// Sends `request` and reads the first response line. Returns the
    /// reader too, for streamed responses that continue on the connection.
    async fn send(
        &self,
        request: AgentRequest,
    ) -> CoreResult<(serde_json::Value, BufReader<OwnedReadHalf>, OwnedWriteHalf)> {
        let stream = UnixStream::connect(&self.socket_path).await.map_err(|e| {
            CoreError::External(format!(
                "connecting to agent at {}: {e}",
                self.socket_path.display()
            ))
        })?;
        let (read_half, mut write_half) = stream.into_split();

        let mut payload = serde_json::to_string(&request)
            .map_err(|e| CoreError::External(format!("encoding agent request: {e}")))?;
        payload.push('\n');
        write_half
            .write_all(payload.as_bytes())
            .await
            .map_err(|e| CoreError::External(format!("writing to agent: {e}")))?;

        let mut reader = BufReader::new(read_half);
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
}
