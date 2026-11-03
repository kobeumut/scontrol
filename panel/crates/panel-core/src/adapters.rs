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
    /// Names the per-deployment log file the agent streams output into.
    pub deployment_id: Uuid,
    pub project_slug: String,
    pub source: ComposeSource,
    pub pull_images: bool,
    /// Complete desired set of domain bindings. Bindings that existed on the
    /// previous deployment but are absent here are removed.
    #[serde(default)]
    pub domains: Vec<ComposeDomainBinding>,
    /// Let's Encrypt account email, required when any binding has `https`.
    #[serde(default)]
    pub acme_email: Option<String>,
    /// Short-lived HTTPS credentials for this deployment only, e.g. a GitHub
    /// App installation token minted by panel-api when the job starts. They
    /// take precedence over a stored token, are masked in the deployment log,
    /// and are deleted from the agent when the deployment ends. They are
    /// never part of a queue row.
    #[serde(default)]
    pub git_credentials: Option<GitHttpCredentials>,
}

/// Replaces a project's write-only environment file. Sent separately from
/// deployments so queued deployment jobs never carry secret values.
#[derive(Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WriteComposeEnvironmentParams {
    pub project_slug: String,
    /// The merged (project → environment → service) variables.
    pub environment: Vec<(String, String)>,
    /// Values of inherited variables that a more specific layer overrides.
    /// They are not part of the environment, but they are still secrets, so
    /// the agent keeps them (0600) only to mask them in logs.
    #[serde(default)]
    pub masked_values: Vec<String>,
}

/// Prints key names only, so a stray `{:?}` never logs a value.
impl std::fmt::Debug for WriteComposeEnvironmentParams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let keys: Vec<&str> = self.environment.iter().map(|(k, _)| k.as_str()).collect();
        f.debug_struct("WriteComposeEnvironmentParams")
            .field("project_slug", &self.project_slug)
            .field("environment_keys", &keys)
            .field("masked_values", &self.masked_values.len())
            .finish()
    }
}

/// HTTPS credentials for a project's private Git remote, usually a
/// personal access or deploy token. Write-only like the environment: the
/// agent keeps them in a 0600 file and hands them to Git through an askpass
/// helper, never argv, and only for the remote's own host.
#[derive(Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct GitHttpCredentials {
    pub username: String,
    pub token: String,
}

impl std::fmt::Debug for GitHttpCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitHttpCredentials")
            .field("username", &self.username)
            .field("token", &"********")
            .finish()
    }
}

/// Replaces (`Some`) or removes (`None`) a project's Git HTTPS credentials.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WriteGitCredentialsParams {
    pub project_slug: String,
    /// Host of the project's HTTPS remote. The agent offers the token only
    /// when Git asks on behalf of this host, so a redirect elsewhere never
    /// receives it.
    pub host: String,
    pub credentials: Option<GitHttpCredentials>,
}

/// One private registry login. `server` is the user-facing registry address
/// (`ghcr.io`, `123.dkr.ecr.eu-west-1.amazonaws.com`, `docker.io`); the agent
/// maps it to the key Docker stores credentials under.
#[derive(Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DockerRegistryAuth {
    pub server: String,
    pub username: String,
    pub password: String,
}

impl std::fmt::Debug for DockerRegistryAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DockerRegistryAuth")
            .field("server", &self.server)
            .field("username", &self.username)
            .field("password", &"********")
            .finish()
    }
}

/// Replaces a project's isolated `DOCKER_CONFIG` (`<project root>/.docker`).
/// An empty `registries` list removes the credentials. The panel sends this
/// right before a deployment and removes it right after, so registry
/// passwords exist on disk only while an image pull can need them.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WriteDockerConfigParams {
    pub project_slug: String,
    pub registries: Vec<DockerRegistryAuth>,
}

/// Checks a registry login with `docker login` (password on stdin, throwaway
/// `DOCKER_CONFIG`) without touching any project.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TestDockerRegistryParams {
    pub auth: DockerRegistryAuth,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CancelDeploymentParams {
    pub deployment_id: Uuid,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DeploymentLogParams {
    pub project_slug: String,
    pub deployment_id: Uuid,
    /// Byte offset to resume from; clients poll with the previous
    /// `next_offset` to tail a running deployment.
    pub offset: u64,
    /// Bounded by the agent to 1..=256 KiB.
    pub max_bytes: u32,
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

/// Follow a Compose project's container output live. The agent redacts
/// environment values before applying `search`, so a search can never
/// probe for a secret.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct FollowComposeLogsParams {
    pub project_slug: String,
    /// One service, or every service of the project when `None`.
    #[serde(default)]
    pub service: Option<String>,
    /// History to send before following; bounded by the agent to 0..=2,000.
    pub tail_lines: u16,
    /// Case-insensitive substring; only matching lines are sent.
    #[serde(default)]
    pub search: Option<String>,
    #[serde(default)]
    pub timestamps: bool,
}

/// Lines of a followed log, oldest first. The stream ends when the
/// containers stop, the agent's follow limit elapses, or on an `Err` item;
/// dropping the receiver stops the follower on the agent.
pub type LogLines = tokio::sync::mpsc::Receiver<CoreResult<String>>;

/// Channel capacity for [`LogLines`]: a slow reader applies backpressure to
/// the follower instead of growing memory.
pub const LOG_LINES_CAPACITY: usize = 256;

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
    /// Image builders the agent was compiled with (`builders` feature) and
    /// found installed. Empty for older agents.
    #[serde(default)]
    pub build_tools: Vec<BuildTool>,
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
    async fn write_compose_environment(
        &self,
        params: WriteComposeEnvironmentParams,
    ) -> CoreResult<()>;
    async fn deploy_compose_project(
        &self,
        params: DeployComposeProjectParams,
    ) -> CoreResult<ComposeDeployOutcome>;
    /// Creates (or replaces) the project's SSH deploy key and returns the
    /// public half. The private key never leaves the agent.
    async fn generate_deploy_key(&self, params: ComposeProjectActionParams) -> CoreResult<String>;
    async fn remove_deploy_key(&self, params: ComposeProjectActionParams) -> CoreResult<()>;
    async fn write_git_credentials(&self, params: WriteGitCredentialsParams) -> CoreResult<()>;
    /// Writes (or, with no registries, removes) the project's private
    /// `DOCKER_CONFIG/config.json`.
    async fn write_docker_config(&self, params: WriteDockerConfigParams) -> CoreResult<()>;
    async fn test_docker_registry(&self, params: TestDockerRegistryParams) -> CoreResult<()>;
    async fn stop_compose_project(&self, params: ComposeProjectActionParams) -> CoreResult<()>;
    async fn remove_compose_project(&self, params: ComposeProjectActionParams) -> CoreResult<()>;
    async fn list_compose_project_services(
        &self,
        params: ComposeProjectActionParams,
    ) -> CoreResult<Vec<ComposeServiceInfo>>;
    async fn compose_project_logs(&self, params: ComposeProjectLogsParams) -> CoreResult<String>;
    async fn follow_compose_logs(&self, params: FollowComposeLogsParams) -> CoreResult<LogLines>;
    async fn read_deployment_log(
        &self,
        params: DeploymentLogParams,
    ) -> CoreResult<DeploymentLogChunk>;
    /// Asks the agent to stop a running deployment. Returns `false` if the
    /// agent is not running that deployment (finished, or not started yet).
    async fn cancel_deployment(&self, params: CancelDeploymentParams) -> CoreResult<bool>;

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
        pub fail_next: Mutex<Option<(String, crate::error::CoreError)>>,
        /// Every `write_compose_environment` request, so tests can check
        /// what reached the agent (only tests ever see these values).
        pub environment_writes: Mutex<Vec<WriteComposeEnvironmentParams>>,
        /// Every deployment request received, for tests that inspect what
        /// panel-api sent the agent.
        pub deploys: Mutex<Vec<DeployComposeProjectParams>>,
    }

    impl MockAdapter {
        pub fn new() -> Self {
            Self::default()
        }

        pub fn fail_next_call(&self, method: &str) {
            self.fail_next_call_with(
                method,
                crate::error::CoreError::External(format!("mock failure injected for {method}")),
            );
        }

        /// Makes the next call of `method` return exactly `error`.
        pub fn fail_next_call_with(&self, method: &str, error: crate::error::CoreError) {
            *self.fail_next.lock().unwrap() = Some((method.to_string(), error));
        }

        fn record(&self, method: &str) -> CoreResult<()> {
            self.calls.lock().unwrap().push(method.to_string());
            let mut guard = self.fail_next.lock().unwrap();
            if guard.as_ref().is_some_and(|(m, _)| m == method) {
                return Err(guard.take().expect("checked above").1);
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
                build_tools: BuildTool::ALL.to_vec(),
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

        async fn write_compose_environment(
            &self,
            params: WriteComposeEnvironmentParams,
        ) -> CoreResult<()> {
            self.record("write_compose_environment")?;
            self.environment_writes.lock().unwrap().push(params);
            Ok(())
        }

        async fn deploy_compose_project(
            &self,
            params: DeployComposeProjectParams,
        ) -> CoreResult<ComposeDeployOutcome> {
            self.record("deploy_compose_project")?;
            self.deploys.lock().unwrap().push(params.clone());
            Ok(ComposeDeployOutcome {
                services: vec![ComposeServiceInfo {
                    name: params.project_slug,
                    container: String::new(),
                    image: "mock-image".into(),
                    state: "running".into(),
                    status: String::new(),
                    health: String::new(),
                    exit_code: 0,
                    ports: vec![],
                }],
                commit_sha: None,
                warnings: vec![],
            })
        }

        async fn generate_deploy_key(
            &self,
            params: ComposeProjectActionParams,
        ) -> CoreResult<String> {
            self.record("generate_deploy_key")?;
            Ok(format!(
                "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIMockKey scontrol-{}",
                params.project_slug
            ))
        }

        async fn remove_deploy_key(&self, _params: ComposeProjectActionParams) -> CoreResult<()> {
            self.record("remove_deploy_key")
        }

        async fn write_git_credentials(
            &self,
            _params: WriteGitCredentialsParams,
        ) -> CoreResult<()> {
            self.record("write_git_credentials")
        }

        async fn write_docker_config(&self, _params: WriteDockerConfigParams) -> CoreResult<()> {
            self.record("write_docker_config")
        }

        async fn test_docker_registry(&self, _params: TestDockerRegistryParams) -> CoreResult<()> {
            self.record("test_docker_registry")
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

        async fn follow_compose_logs(
            &self,
            params: FollowComposeLogsParams,
        ) -> CoreResult<LogLines> {
            self.record("follow_compose_logs")?;
            let (tx, rx) = tokio::sync::mpsc::channel(LOG_LINES_CAPACITY);
            let needle = params.search.map(|s| s.to_lowercase());
            for line in ["web-1  | listening on :80", "web-1  | GET / 200"] {
                if needle
                    .as_ref()
                    .is_none_or(|n| line.to_lowercase().contains(n))
                {
                    tx.try_send(Ok(line.to_string())).ok();
                }
            }
            Ok(rx)
        }

        async fn read_deployment_log(
            &self,
            params: DeploymentLogParams,
        ) -> CoreResult<DeploymentLogChunk> {
            self.record("read_deployment_log")?;
            Ok(DeploymentLogChunk {
                content: String::new(),
                next_offset: params.offset,
                eof: true,
            })
        }

        async fn cancel_deployment(&self, _params: CancelDeploymentParams) -> CoreResult<bool> {
            self.record("cancel_deployment")?;
            Ok(true)
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
