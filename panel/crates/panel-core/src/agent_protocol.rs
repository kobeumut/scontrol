//! Wire protocol between `panel-agent` and its clients (`panel-api`,
//! `panel-cli`). Defined once, here, so every party (server and clients)
//! agrees on the exact shape of the protocol.
//!
//! Transport: newline-delimited JSON over a Unix domain socket
//! (`/run/scontrol/agent.sock`, mode 0660, group `scontrol`). Each line is a
//! single [`AgentRequest`] and elicits exactly one [`AgentResponse`] line.
//!
//! The one exception is [`AgentRequest::FollowComposeLogs`]: after an `Ok`
//! response the agent writes [`StreamFrame`] lines until an `End` frame,
//! then closes the connection. Closing it from the client side stops the
//! follower.

use crate::adapters::*;
use crate::error::CoreError;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", content = "params", rename_all = "snake_case")]
pub enum AgentRequest {
    Capabilities,
    CreateSite(CreateSiteParams),
    DeleteSite(DeleteSiteParams),
    ReloadWebServer,
    IssueCertificate(IssueCertificateParams),
    CreateDatabase(CreateDatabaseParams),
    DeleteDatabase(DeleteDatabaseParams),
    CreateSystemUser(CreateSystemUserParams),
    DeleteSystemUser(DeleteSystemUserParams),
    ApplyFirewallRule(ApplyFirewallRuleParams),
    UpsertCronJob(UpsertCronJobParams),
    RemoveCronJob(RemoveCronJobParams),
    CreateContainer(CreateContainerParams),
    StartContainer(ContainerActionParams),
    StopContainer(ContainerActionParams),
    RemoveContainer(ContainerActionParams),
    ListContainers,
    WriteComposeEnvironment(WriteComposeEnvironmentParams),
    DeployComposeProject(DeployComposeProjectParams),
    GenerateDeployKey(ComposeProjectActionParams),
    RemoveDeployKey(ComposeProjectActionParams),
    WriteGitCredentials(WriteGitCredentialsParams),
    WriteDockerConfig(WriteDockerConfigParams),
    TestDockerRegistry(TestDockerRegistryParams),
    StopComposeProject(ComposeProjectActionParams),
    RestartComposeProject(ComposeProjectActionParams),
    SetDatabaseExposure(SetDatabaseExposureParams),
    RemoveComposeProject(RemoveComposeProjectParams),
    ListComposeProjectServices(ComposeProjectActionParams),
    ComposeProjectLogs(ComposeProjectLogsParams),
    FollowComposeLogs(FollowComposeLogsParams),
    ReadDeploymentLog(DeploymentLogParams),
    CancelDeployment(CancelDeploymentParams),
    UpsertScheduledJob(UpsertScheduledJobParams),
    RemoveScheduledJob(ScheduledJobRef),
    RunScheduledJob(ScheduledJobRef),
    ListScheduledJobRuns(ListScheduledJobRunsParams),
    ReadScheduledJobRunLog(ScheduledJobRunLogParams),
    ReadNginxStats(ReadNginxStatsParams),
    PruneDocker(PruneDockerParams),
    CreateMailbox(CreateMailboxParams),
    DeleteMailbox(DeleteMailboxParams),
    ScanPath(ScanPathParams),
    TestS3Target(TestS3Params),
    ListBackups(ListBackupsParams),
    BackupDatabase(BackupDatabaseParams),
    RestoreDatabase(RestoreDatabaseParams),
    BackupVolume(BackupVolumeParams),
    RestoreVolume(RestoreVolumeParams),
    MetricsSnapshot,
    WriteVhostSecrets(WriteVhostSecretsParams),
    OpenTerminal(OpenTerminalParams),
    ReadTerminal(TerminalReadParams),
    WriteTerminal(TerminalWriteParams),
    ResizeTerminal(TerminalResizeParams),
    CloseTerminal(TerminalCloseParams),
}

/// One line of a streamed response (see the module docs).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "frame", rename_all = "snake_case")]
pub enum StreamFrame {
    Line {
        text: String,
    },
    /// Last frame. `error` is set when the stream stopped on a failure.
    End {
        error: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum AgentResponse {
    Ok { data: serde_json::Value },
    Err { message: String, kind: String },
}

impl AgentResponse {
    pub fn ok<T: Serialize>(value: &T) -> Self {
        AgentResponse::Ok {
            data: serde_json::to_value(value).unwrap_or(serde_json::Value::Null),
        }
    }

    pub fn from_core_result<T: Serialize>(result: crate::error::CoreResult<T>) -> Self {
        match result {
            Ok(v) => Self::ok(&v),
            Err(e) => AgentResponse::Err {
                kind: error_kind(&e),
                message: e.message().to_string(),
            },
        }
    }

    pub fn into_core_result(self) -> crate::error::CoreResult<serde_json::Value> {
        match self {
            AgentResponse::Ok { data } => Ok(data),
            AgentResponse::Err { message, kind } => Err(match kind.as_str() {
                "validation" => CoreError::Validation(message),
                "not_found" => CoreError::NotFound(message),
                "already_exists" => CoreError::AlreadyExists(message),
                "permission_denied" => CoreError::PermissionDenied(message),
                "io" => CoreError::Io(message),
                "cancelled" => CoreError::Cancelled(message),
                _ => CoreError::External(message),
            }),
        }
    }
}

fn error_kind(e: &CoreError) -> String {
    match e {
        CoreError::Validation(_) => "validation",
        CoreError::NotFound(_) => "not_found",
        CoreError::AlreadyExists(_) => "already_exists",
        CoreError::PermissionDenied(_) => "permission_denied",
        CoreError::Io(_) => "io",
        CoreError::Cancelled(_) => "cancelled",
        CoreError::External(_) => "external",
    }
    .to_string()
}

/// Runs one non-streamed request against `adapter`. The agent calls this
/// after taking its own locks; a mock agent can serve the same protocol
/// from any [`AdapterPort`].
pub async fn dispatch(adapter: &dyn AdapterPort, request: AgentRequest) -> AgentResponse {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::CreateSiteParams;
    use crate::types::Runtime;

    #[test]
    fn request_round_trips_through_json() {
        let req = AgentRequest::CreateSite(CreateSiteParams {
            site_id: uuid::Uuid::nil(),
            domain: "example.com".into(),
            owner_username: "deploy".into(),
            runtime: Runtime::Static,
        });
        let json = serde_json::to_string(&req).unwrap();
        let decoded: AgentRequest = serde_json::from_str(&json).unwrap();
        match decoded {
            AgentRequest::CreateSite(p) => assert_eq!(p.domain, "example.com"),
            _ => panic!("wrong variant decoded"),
        }
    }

    #[test]
    fn error_response_maps_back_to_matching_core_error_variant() {
        let resp = AgentResponse::Err {
            message: "nope".into(),
            kind: "not_found".into(),
        };
        let err = resp.into_core_result().unwrap_err();
        assert!(matches!(err, CoreError::NotFound(_)));
    }

    #[test]
    fn ok_response_round_trips_payload() {
        let resp = AgentResponse::ok(&42u32);
        let value = resp.into_core_result().unwrap();
        assert_eq!(value, serde_json::json!(42));
    }

    #[test]
    fn error_message_is_not_prefixed_twice() {
        let original = CoreError::Cancelled("deployment cancelled".into());
        let decoded = AgentResponse::from_core_result::<()>(Err(original.clone()))
            .into_core_result()
            .unwrap_err();
        assert_eq!(decoded.to_string(), original.to_string());
        assert_eq!(decoded.to_string(), "cancelled: deployment cancelled");
    }
}
