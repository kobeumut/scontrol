//! Shared Unix-socket RPC client implementing [`AdapterPort`] by talking to
//! `panel-agent`. Used by both `panel-api` (per-request, unprivileged) and
//! `panel-cli` (direct host administration). Never builds or runs shell
//! commands itself — see `panel-agent` for the privileged side.

use crate::adapters::*;
use crate::agent_protocol::{AgentRequest, AgentResponse};
use crate::error::{CoreError, CoreResult};
use crate::types::{ComposeServiceInfo, ContainerInfo, MailboxInfo, ScanResult};
use async_trait::async_trait;
use serde::de::DeserializeOwned;
use std::path::{Path, PathBuf};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
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

    async fn call<T: DeserializeOwned>(&self, request: AgentRequest) -> CoreResult<T> {
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

        let data = response.into_core_result()?;
        serde_json::from_value(data)
            .map_err(|e| CoreError::External(format!("decoding agent payload: {e}")))
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

    async fn deploy_compose_project(
        &self,
        params: DeployComposeProjectParams,
    ) -> CoreResult<Vec<ComposeServiceInfo>> {
        self.call(AgentRequest::DeployComposeProject(params)).await
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

    async fn create_mailbox(&self, params: CreateMailboxParams) -> CoreResult<MailboxInfo> {
        self.call(AgentRequest::CreateMailbox(params)).await
    }

    async fn delete_mailbox(&self, params: DeleteMailboxParams) -> CoreResult<()> {
        self.call(AgentRequest::DeleteMailbox(params)).await
    }

    async fn scan_path(&self, params: ScanPathParams) -> CoreResult<ScanResult> {
        self.call(AgentRequest::ScanPath(params)).await
    }
}
