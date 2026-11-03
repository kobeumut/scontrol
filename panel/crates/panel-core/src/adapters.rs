//! The [`AdapterPort`] trait is the single boundary between business logic
//! (in `panel-api`) and privileged host operations (implemented by
//! `panel-agent`, and by `MockAdapter` in tests).
//!
//! Every method takes a small, explicitly-typed, pre-validated params struct.
//! Implementations MUST NOT build shell strings from these fields; they must
//! be passed straight into `std::process::Command::new(..).arg(..)` calls.
//! See CLAUDE.md "Shell Command Rule".

use crate::error::CoreResult;
use crate::types::*;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CreateSiteParams {
    pub site_id: Uuid,
    pub domain: String,
    pub owner_username: String,
    pub runtime: Runtime,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DeleteSiteParams {
    pub site_id: Uuid,
    pub domain: String,
    pub owner_username: String,
    /// Present when the site ran on PHP-FPM, so the agent can also remove
    /// the matching pool config.
    pub php_version: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct IssueCertificateParams {
    pub domain: String,
    pub admin_email: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CreateDatabaseParams {
    pub engine: DatabaseEngine,
    pub name: String,
    pub username: String,
    pub password: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DeleteDatabaseParams {
    pub engine: DatabaseEngine,
    pub name: String,
    pub username: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CreateSystemUserParams {
    pub username: String,
    pub home_dir: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DeleteSystemUserParams {
    pub username: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ApplyFirewallRuleParams {
    pub port: u16,
    pub protocol: FirewallProtocol,
    pub allow: bool,
    pub description: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct UpsertCronJobParams {
    pub owner_username: String,
    pub job_id: Uuid,
    pub schedule: String,
    pub command: String,
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RemoveCronJobParams {
    pub owner_username: String,
    pub job_id: Uuid,
}

// ---- Optional feature params -------------------------------------------

/// Request to materialize and deploy a Dokploy-style Compose project. The
/// `environment` values are intentionally write-only: the API accepts them
/// for deployment but neither the agent response nor persistence models
/// return them.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DeployComposeProjectParams {
    pub project_id: Uuid,
    pub project_slug: String,
    pub source: ComposeSource,
    pub environment: Vec<(String, String)>,
    pub pull_images: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ComposeProjectActionParams {
    pub project_slug: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ComposeProjectLogsParams {
    pub project_slug: String,
    /// Bounded by the agent to 1..=2,000 lines.
    pub tail_lines: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CreateContainerParams {
    pub name: String,
    pub image: String,
    pub ports: Vec<(u16, u16)>,
    pub env: Vec<(String, String)>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ContainerActionParams {
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CreateMailboxParams {
    pub address: String,
    pub password: String,
    pub quota_mb: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DeleteMailboxParams {
    pub address: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ScanPathParams {
    pub path: String,
}

/// Capabilities the running agent supports, derived from the optional
/// features it was compiled and configured with. `panel-api` queries this at
/// startup (and caches it) so it can return clear "not installed" errors
/// instead of failing deep inside an RPC call.
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AgentCapabilities {
    pub docker: bool,
    pub mail: bool,
    pub antivirus: bool,
    pub version: String,
}

/// Boundary trait between panel-api business logic and privileged host
/// operations. Implemented by the real agent RPC client in panel-api and by
/// `panel-agent`'s local dispatcher, and mocked in tests via [`MockAdapter`].
#[async_trait]
pub trait AdapterPort: Send + Sync {
    async fn capabilities(&self) -> CoreResult<AgentCapabilities>;

    async fn create_site(&self, params: CreateSiteParams) -> CoreResult<()>;
    async fn delete_site(&self, params: DeleteSiteParams) -> CoreResult<()>;
    async fn reload_web_server(&self) -> CoreResult<()>;
    async fn issue_certificate(&self, params: IssueCertificateParams) -> CoreResult<()>;

    async fn create_database(&self, params: CreateDatabaseParams) -> CoreResult<()>;
    async fn delete_database(&self, params: DeleteDatabaseParams) -> CoreResult<()>;

    async fn create_system_user(&self, params: CreateSystemUserParams) -> CoreResult<()>;
    async fn delete_system_user(&self, params: DeleteSystemUserParams) -> CoreResult<()>;

    async fn apply_firewall_rule(&self, params: ApplyFirewallRuleParams) -> CoreResult<()>;

    async fn upsert_cron_job(&self, params: UpsertCronJobParams) -> CoreResult<()>;
    async fn remove_cron_job(&self, params: RemoveCronJobParams) -> CoreResult<()>;

    // Optional: Docker
    async fn create_container(&self, params: CreateContainerParams) -> CoreResult<ContainerInfo>;
    async fn start_container(&self, params: ContainerActionParams) -> CoreResult<()>;
    async fn stop_container(&self, params: ContainerActionParams) -> CoreResult<()>;
    async fn remove_container(&self, params: ContainerActionParams) -> CoreResult<()>;
    async fn list_containers(&self) -> CoreResult<Vec<ContainerInfo>>;

    // Optional: Docker PaaS-style Compose projects
    async fn deploy_compose_project(
        &self,
        params: DeployComposeProjectParams,
    ) -> CoreResult<Vec<ComposeServiceInfo>>;
    async fn stop_compose_project(&self, params: ComposeProjectActionParams) -> CoreResult<()>;
    async fn remove_compose_project(&self, params: ComposeProjectActionParams) -> CoreResult<()>;
    async fn list_compose_project_services(
        &self,
        params: ComposeProjectActionParams,
    ) -> CoreResult<Vec<ComposeServiceInfo>>;
    async fn compose_project_logs(&self, params: ComposeProjectLogsParams) -> CoreResult<String>;

    // Optional: Mail
    async fn create_mailbox(&self, params: CreateMailboxParams) -> CoreResult<MailboxInfo>;
    async fn delete_mailbox(&self, params: DeleteMailboxParams) -> CoreResult<()>;

    // Optional: Antivirus
    async fn scan_path(&self, params: ScanPathParams) -> CoreResult<ScanResult>;
}

#[cfg(feature = "test-utils")]
pub mod mock {
    use super::*;
    use std::sync::Mutex;

    /// An in-memory [`AdapterPort`] used by unit tests to exercise business
    /// logic (including rollback paths) without touching a real host.
    #[derive(Default)]
    pub struct MockAdapter {
        pub calls: Mutex<Vec<String>>,
        pub fail_next: Mutex<Option<String>>,
    }

    impl MockAdapter {
        pub fn new() -> Self {
            Self::default()
        }

        pub fn fail_next_call(&self, method: &str) {
            *self.fail_next.lock().unwrap() = Some(method.to_string());
        }

        fn record(&self, method: &str) -> CoreResult<()> {
            self.calls.lock().unwrap().push(method.to_string());
            let mut guard = self.fail_next.lock().unwrap();
            if guard.as_deref() == Some(method) {
                *guard = None;
                return Err(crate::error::CoreError::External(format!(
                    "mock failure injected for {method}"
                )));
            }
            Ok(())
        }
    }

    #[async_trait]
    impl AdapterPort for MockAdapter {
        async fn capabilities(&self) -> CoreResult<AgentCapabilities> {
            self.record("capabilities")?;
            Ok(AgentCapabilities {
                docker: true,
                mail: true,
                antivirus: true,
                version: "mock".into(),
            })
        }

        async fn create_site(&self, _params: CreateSiteParams) -> CoreResult<()> {
            self.record("create_site")
        }

        async fn delete_site(&self, _params: DeleteSiteParams) -> CoreResult<()> {
            self.record("delete_site")
        }

        async fn reload_web_server(&self) -> CoreResult<()> {
            self.record("reload_web_server")
        }

        async fn issue_certificate(&self, _params: IssueCertificateParams) -> CoreResult<()> {
            self.record("issue_certificate")
        }

        async fn create_database(&self, _params: CreateDatabaseParams) -> CoreResult<()> {
            self.record("create_database")
        }

        async fn delete_database(&self, _params: DeleteDatabaseParams) -> CoreResult<()> {
            self.record("delete_database")
        }

        async fn create_system_user(&self, _params: CreateSystemUserParams) -> CoreResult<()> {
            self.record("create_system_user")
        }

        async fn delete_system_user(&self, _params: DeleteSystemUserParams) -> CoreResult<()> {
            self.record("delete_system_user")
        }

        async fn apply_firewall_rule(&self, _params: ApplyFirewallRuleParams) -> CoreResult<()> {
            self.record("apply_firewall_rule")
        }

        async fn upsert_cron_job(&self, _params: UpsertCronJobParams) -> CoreResult<()> {
            self.record("upsert_cron_job")
        }

        async fn remove_cron_job(&self, _params: RemoveCronJobParams) -> CoreResult<()> {
            self.record("remove_cron_job")
        }

        async fn create_container(
            &self,
            params: CreateContainerParams,
        ) -> CoreResult<ContainerInfo> {
            self.record("create_container")?;
            Ok(ContainerInfo {
                id: "mock-id".into(),
                name: params.name,
                image: params.image,
                status: "running".into(),
                ports: vec![],
            })
        }

        async fn start_container(&self, _params: ContainerActionParams) -> CoreResult<()> {
            self.record("start_container")
        }

        async fn stop_container(&self, _params: ContainerActionParams) -> CoreResult<()> {
            self.record("stop_container")
        }

        async fn remove_container(&self, _params: ContainerActionParams) -> CoreResult<()> {
            self.record("remove_container")
        }

        async fn list_containers(&self) -> CoreResult<Vec<ContainerInfo>> {
            self.record("list_containers")?;
            Ok(vec![])
        }

        async fn deploy_compose_project(
            &self,
            params: DeployComposeProjectParams,
        ) -> CoreResult<Vec<ComposeServiceInfo>> {
            self.record("deploy_compose_project")?;
            Ok(vec![ComposeServiceInfo {
                name: params.project_slug,
                image: "mock-image".into(),
                state: "running".into(),
                ports: vec![],
            }])
        }

        async fn stop_compose_project(
            &self,
            _params: ComposeProjectActionParams,
        ) -> CoreResult<()> {
            self.record("stop_compose_project")
        }

        async fn remove_compose_project(
            &self,
            _params: ComposeProjectActionParams,
        ) -> CoreResult<()> {
            self.record("remove_compose_project")
        }

        async fn list_compose_project_services(
            &self,
            _params: ComposeProjectActionParams,
        ) -> CoreResult<Vec<ComposeServiceInfo>> {
            self.record("list_compose_project_services")?;
            Ok(vec![])
        }

        async fn compose_project_logs(
            &self,
            _params: ComposeProjectLogsParams,
        ) -> CoreResult<String> {
            self.record("compose_project_logs")?;
            Ok("mock deployment log".into())
        }

        async fn create_mailbox(&self, params: CreateMailboxParams) -> CoreResult<MailboxInfo> {
            self.record("create_mailbox")?;
            let domain = params.address.split('@').nth(1).unwrap_or("").to_string();
            Ok(MailboxInfo {
                address: params.address,
                domain,
                quota_mb: params.quota_mb,
            })
        }

        async fn delete_mailbox(&self, _params: DeleteMailboxParams) -> CoreResult<()> {
            self.record("delete_mailbox")
        }

        async fn scan_path(&self, params: ScanPathParams) -> CoreResult<ScanResult> {
            self.record("scan_path")?;
            let now = chrono::Utc::now();
            Ok(ScanResult {
                path: params.path,
                infected_files: vec![],
                scanned_files: 0,
                started_at: now,
                finished_at: now,
            })
        }
    }
}
