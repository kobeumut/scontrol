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
    RemoveComposeProject(ComposeProjectActionParams),
    ListComposeProjectServices(ComposeProjectActionParams),
    ComposeProjectLogs(ComposeProjectLogsParams),
    FollowComposeLogs(FollowComposeLogsParams),
    ReadDeploymentLog(DeploymentLogParams),
    CancelDeployment(CancelDeploymentParams),
    CreateMailbox(CreateMailboxParams),
    DeleteMailbox(DeleteMailboxParams),
    ScanPath(ScanPathParams),
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
