//! The [`AdapterPort`] trait is the single boundary between business logic
//! (in `panel-api`) and privileged host operations (implemented by
//! `panel-agent`, and by `MockAdapter` in tests).
//!
//! Every method takes a small, explicitly-typed, pre-validated params struct.
//! Implementations MUST NOT build shell strings from these fields; they must
//! be passed straight into `std::process::Command::new(..).arg(..)` calls.
//! See CLAUDE.md "Shell Command Rule".

use crate::error::CoreResult;
use crate::observability::RequestBucket;
use crate::schedule::{JobRunInfo, ScheduledJobSpec};
use crate::types::*;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Reads the next part of the Nginx JSON access log. The agent keeps no
/// cursor: panel-api passes the position it stored last time.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ReadNginxStatsParams {
    /// Identity (inode) of the log file the offset belongs to. When the file
    /// is now a different one (rotation), reading restarts at 0.
    pub file_id: Option<u64>,
    pub offset: u64,
    /// Bounded by the agent to 64 KiB..=8 MiB.
    pub max_bytes: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct NginxStatsChunk {
    pub buckets: Vec<RequestBucket>,
    pub file_id: Option<u64>,
    /// Where to continue: just after the last complete line read.
    pub next_offset: u64,
    /// Valid entries aggregated.
    pub lines: u64,
    /// Lines that were not valid entries.
    pub skipped: u64,
    /// False while the log file does not exist (Nginx not yet logging).
    pub log_present: bool,
}

/// A Docker cleanup. Only the age is variable; the commands are fixed.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PruneDockerParams {
    pub older_than_hours: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DockerPruneOutcome {
    pub images_freed_bytes: u64,
    pub builder_freed_bytes: u64,
}

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
    /// Loopback port and generated credentials of a database service
    /// (`ComposeSource::Database` only). Unsealed by the queue worker for
    /// this request; never part of a queue row. The agent writes the
    /// credentials to 0600 files and masks them in logs.
    #[serde(default)]
    pub database: Option<DatabaseDeployment>,
    /// Start a recorded release again instead of preparing the source: no
    /// Git fetch, no build, no pull. The agent checks the target against its
    /// own release record and runs the pinned images with the current
    /// environment file and domains.
    #[serde(default)]
    pub rollback: Option<RollbackTarget>,
}

/// Opens (`Some`) or closes (`None`) outside access to a database service:
/// a host Nginx `stream` proxy from `public_port` to the service's loopback
/// port, plus the matching UFW allow rules. Replaces any previous exposure
/// of the same service.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SetDatabaseExposureParams {
    pub project_slug: String,
    pub loopback_port: u16,
    pub exposure: Option<DatabaseExposure>,
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

/// Installs or replaces a job's systemd service and timer
/// (`scontrol-job-<job_id>.service/.timer`). The agent validates `spec`
/// again and checks every `OnCalendar=` value with `systemd-analyze`.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct UpsertScheduledJobParams {
    pub job_id: Uuid,
    pub project_slug: String,
    pub spec: ScheduledJobSpec,
    /// A disabled job keeps its units but its timer is stopped; "run now"
    /// still works.
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ScheduledJobRef {
    pub project_slug: String,
    pub job_id: Uuid,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ListScheduledJobRunsParams {
    /// Jobs to report on; runs of other jobs are never read.
    pub jobs: Vec<ScheduledJobRef>,
    /// Only runs whose record changed after this instant; all retained
    /// runs when `None`.
    #[serde(default)]
    pub since: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ScheduledJobRunLogParams {
    pub project_slug: String,
    pub job_id: Uuid,
    pub run_id: Uuid,
    pub offset: u64,
    /// Bounded by the agent to 1..=256 KiB.
    pub max_bytes: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CreateContainerParams {
    pub name: String,
    pub image: String,
    pub ports: Vec<(u16, u16)>,
    pub env: Vec<(String, String)>,
}

/// Secrets of one project domain's vhost options. They are written to 0600
/// files under the project's directory and never read back.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WriteVhostSecretsParams {
    pub project_slug: String,
    pub domain: String,
    #[serde(default)]
    pub basic_auth: crate::vhost::SecretChange<Vec<crate::vhost::BasicAuthUser>>,
    #[serde(default)]
    pub certificate: crate::vhost::SecretChange<crate::vhost::CustomCertificate>,
}

/// The only shells a web terminal may start inside a container.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TerminalShell {
    Sh,
    Bash,
    Ash,
}

impl TerminalShell {
    pub fn program(self) -> &'static str {
        match self {
            TerminalShell::Sh => "sh",
            TerminalShell::Bash => "bash",
            TerminalShell::Ash => "ash",
        }
    }
}

/// Opens an interactive terminal in a service container of a Compose project
/// (`docker exec -it`). There is no way to name a host shell or a container
/// outside a project.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct OpenTerminalParams {
    pub project_slug: String,
    pub service_name: String,
    pub shell: TerminalShell,
    pub cols: u16,
    pub rows: u16,
    /// Who opened it, for the agent's own log line.
    pub actor: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TerminalSessionInfo {
    pub session_id: uuid::Uuid,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TerminalReadParams {
    pub session_id: uuid::Uuid,
    /// Wait up to this long for output when none is buffered.
    pub wait_ms: u64,
}

/// Output since the previous read. `data` is base64.
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TerminalOutput {
    pub data: String,
    /// Set once the process exited and everything was read.
    pub exited: Option<i32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TerminalWriteParams {
    pub session_id: uuid::Uuid,
    /// Base64 keystrokes.
    pub data: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TerminalResizeParams {
    pub session_id: uuid::Uuid,
    pub cols: u16,
    pub rows: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TerminalCloseParams {
    pub session_id: uuid::Uuid,
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

/// An S3-compatible bucket (AWS S3, Cloudflare R2, MinIO, ...). Requests
/// use path-style addressing: `<endpoint>/<bucket>/<key>`. The keys exist
/// only in memory and on the agent socket; `Debug` redacts them.
#[derive(Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct S3Target {
    /// `https://host[:port]` (plain `http://` is accepted for a local MinIO).
    pub endpoint: String,
    pub region: String,
    pub bucket: String,
    pub access_key: String,
    pub secret_key: String,
}

impl std::fmt::Debug for S3Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("S3Target")
            .field("endpoint", &self.endpoint)
            .field("region", &self.region)
            .field("bucket", &self.bucket)
            .field("access_key", &"********")
            .field("secret_key", &"********")
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BackupEngine {
    Postgres,
    Mysql,
    Mongodb,
}

impl BackupEngine {
    pub fn as_str(self) -> &'static str {
        match self {
            BackupEngine::Postgres => "postgres",
            BackupEngine::Mysql => "mysql",
            BackupEngine::Mongodb => "mongodb",
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            BackupEngine::Mongodb => "archive.gz",
            _ => "sql.gz",
        }
    }
}

/// How to reach a database server. The password goes to the dump tool
/// through its environment (or a 0600 config file for MongoDB), never argv.
#[derive(Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DatabaseConnection {
    pub engine: BackupEngine,
    pub host: String,
    pub port: u16,
    pub database: String,
    pub username: String,
    pub password: String,
}

impl std::fmt::Debug for DatabaseConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DatabaseConnection")
            .field("engine", &self.engine)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("database", &self.database)
            .field("username", &self.username)
            .field("password", &"********")
            .finish()
    }
}

/// Streams a database dump (gzip) straight into a multipart S3 upload,
/// then prunes backups of the same database beyond `retention_count`
/// (0 keeps everything).
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct BackupDatabaseParams {
    pub target: S3Target,
    pub prefix: String,
    pub connection: DatabaseConnection,
    pub retention_count: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RestoreDatabaseParams {
    pub target: S3Target,
    pub key: String,
    pub connection: DatabaseConnection,
}

/// Streams `tar | gzip` of a Docker named volume into S3. With
/// `stop_service`, the service is stopped for the copy and started again
/// afterwards (even when the copy fails).
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct BackupVolumeParams {
    pub target: S3Target,
    pub prefix: String,
    pub project_slug: String,
    pub service: Option<String>,
    pub volume: String,
    pub stop_service: bool,
    pub retention_count: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RestoreVolumeParams {
    pub target: S3Target,
    pub key: String,
    pub project_slug: String,
    pub service: Option<String>,
    pub volume: String,
    pub stop_service: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct BackupOutcome {
    pub key: String,
    pub size_bytes: u64,
    /// Older backups deleted by the retention policy.
    pub pruned: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TestS3Params {
    pub target: S3Target,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ListBackupsParams {
    pub target: S3Target,
    pub prefix: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
pub struct BackupObject {
    pub key: String,
    pub size_bytes: u64,
    pub last_modified: String,
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
    /// `docker compose restart` of the last deployed project model.
    async fn restart_compose_project(&self, params: ComposeProjectActionParams) -> CoreResult<()>;
    async fn set_database_exposure(&self, params: SetDatabaseExposureParams) -> CoreResult<()>;
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

    // Scheduled jobs (Docker): systemd timers running `docker compose exec`.
    async fn upsert_scheduled_job(&self, params: UpsertScheduledJobParams) -> CoreResult<()>;
    /// Stops and removes a job's units and run logs. Idempotent.
    async fn remove_scheduled_job(&self, params: ScheduledJobRef) -> CoreResult<()>;
    /// Starts a run now. `AlreadyExists` while a run of the job is active.
    async fn run_scheduled_job(&self, params: ScheduledJobRef) -> CoreResult<()>;
    async fn list_scheduled_job_runs(
        &self,
        params: ListScheduledJobRunsParams,
    ) -> CoreResult<Vec<JobRunInfo>>;
    async fn read_scheduled_job_run_log(
        &self,
        params: ScheduledJobRunLogParams,
    ) -> CoreResult<DeploymentLogChunk>;

    // Optional: Mail
    async fn create_mailbox(&self, params: CreateMailboxParams) -> CoreResult<MailboxInfo>;
    async fn delete_mailbox(&self, params: DeleteMailboxParams) -> CoreResult<()>;

    // Optional: Antivirus
    async fn scan_path(&self, params: ScanPathParams) -> CoreResult<ScanResult>;

    // Data: streaming S3 backups
    async fn test_s3_target(&self, params: TestS3Params) -> CoreResult<()>;
    async fn list_backups(&self, params: ListBackupsParams) -> CoreResult<Vec<BackupObject>>;
    async fn backup_database(&self, params: BackupDatabaseParams) -> CoreResult<BackupOutcome>;
    async fn restore_database(&self, params: RestoreDatabaseParams) -> CoreResult<()>;
    async fn backup_volume(&self, params: BackupVolumeParams) -> CoreResult<BackupOutcome>;
    async fn restore_volume(&self, params: RestoreVolumeParams) -> CoreResult<()>;

    // Observability: request statistics and Docker cleanup.
    async fn read_nginx_stats(&self, params: ReadNginxStatsParams) -> CoreResult<NginxStatsChunk>;
    /// `docker image prune -a` and `docker builder prune -a` with an
    /// `until=` filter; reports the freed space (Docker feature).
    async fn prune_docker(&self, params: PruneDockerParams) -> CoreResult<DockerPruneOutcome>;

    // Observability: the agent samples `/proc` and cgroup v2 in-process.
    /// The sampler's latest snapshot; `None` until it has two readings to
    /// compute rates from.
    async fn metrics_snapshot(&self) -> CoreResult<Option<crate::metrics::MetricsSnapshot>>;

    // Delivery extras: vhost option secrets and the web terminal.
    async fn write_vhost_secrets(&self, params: WriteVhostSecretsParams) -> CoreResult<()>;
    async fn open_terminal(&self, params: OpenTerminalParams) -> CoreResult<TerminalSessionInfo>;
    async fn read_terminal(&self, params: TerminalReadParams) -> CoreResult<TerminalOutput>;
    async fn write_terminal(&self, params: TerminalWriteParams) -> CoreResult<()>;
    async fn resize_terminal(&self, params: TerminalResizeParams) -> CoreResult<()>;
    async fn close_terminal(&self, params: TerminalCloseParams) -> CoreResult<()>;
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
        /// Snapshot the next non-rollback deployment reports instead of the
        /// default one.
        pub next_snapshot: Mutex<Option<DeploymentSnapshot>>,
        /// Releases "kept" so far, newest (or rolled back to) first.
        pub releases: Mutex<Vec<Uuid>>,
        /// Every `set_database_exposure` request.
        pub exposures: Mutex<Vec<SetDatabaseExposureParams>>,
        /// Every scheduled-job install request received.
        pub job_upserts: Mutex<Vec<UpsertScheduledJobParams>>,
        /// Runs `list_scheduled_job_runs` reports (filtered by job and
        /// `since`), so tests can simulate timer runs.
        pub job_runs: Mutex<Vec<JobRunInfo>>,
        /// Chunks `read_nginx_stats` hands out, front first. Empty yields an
        /// empty chunk at the requested offset.
        pub nginx_chunks: Mutex<std::collections::VecDeque<NginxStatsChunk>>,
        /// What `prune_docker` reports, and every request received.
        /// Every `write_vhost_secrets` request and `open_terminal` request.
        pub vhost_secret_writes: Mutex<Vec<WriteVhostSecretsParams>>,
        pub terminals: Mutex<Vec<OpenTerminalParams>>,
        pub prune_outcome: Mutex<Option<DockerPruneOutcome>>,
        pub prunes: Mutex<Vec<PruneDockerParams>>,
        /// What `metrics_snapshot` returns, so tests can drive the collector.
        pub metrics: Mutex<Option<crate::metrics::MetricsSnapshot>>,
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
            let snapshot = self.next_snapshot.lock().unwrap().take();
            Ok(ComposeDeployOutcome {
                services: vec![ComposeServiceInfo {
                    name: params.project_slug.clone(),
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
                // Like the agent: rollbacks record no new release.
                snapshot: params.rollback.is_none().then(|| {
                    snapshot.unwrap_or(DeploymentSnapshot {
                        compose_path: "docker-compose.yml".into(),
                        compose_sha256: format!("{:064x}", params.deployment_id.as_u128()),
                        compose_file: "services:\n  web:\n    image: nginx\n".into(),
                        override_file: None,
                        env_keys: vec![],
                        images: vec![PinnedImage {
                            service: "web".into(),
                            tag: deployment_image_tag(
                                &params.project_slug,
                                "web",
                                params.deployment_id,
                            )
                            .unwrap_or_default(),
                            image_id: format!("sha256:{:064x}", params.deployment_id.as_u128()),
                        }],
                    })
                }),
                retained_releases: {
                    // Newest first, like the agent; nothing is pruned.
                    let mut releases = self.releases.lock().unwrap();
                    let current = params
                        .rollback
                        .as_ref()
                        .map_or(params.deployment_id, |t| t.deployment_id);
                    releases.retain(|id| *id != current);
                    releases.insert(0, current);
                    releases.clone()
                },
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

        async fn restart_compose_project(
            &self,
            _params: ComposeProjectActionParams,
        ) -> CoreResult<()> {
            self.record("restart_compose_project")
        }

        async fn set_database_exposure(&self, params: SetDatabaseExposureParams) -> CoreResult<()> {
            self.record("set_database_exposure")?;
            self.exposures.lock().unwrap().push(params);
            Ok(())
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

        async fn upsert_scheduled_job(&self, params: UpsertScheduledJobParams) -> CoreResult<()> {
            self.record("upsert_scheduled_job")?;
            self.job_upserts.lock().unwrap().push(params);
            Ok(())
        }

        async fn remove_scheduled_job(&self, _params: ScheduledJobRef) -> CoreResult<()> {
            self.record("remove_scheduled_job")
        }

        async fn run_scheduled_job(&self, _params: ScheduledJobRef) -> CoreResult<()> {
            self.record("run_scheduled_job")
        }

        async fn list_scheduled_job_runs(
            &self,
            params: ListScheduledJobRunsParams,
        ) -> CoreResult<Vec<JobRunInfo>> {
            self.record("list_scheduled_job_runs")?;
            Ok(self
                .job_runs
                .lock()
                .unwrap()
                .iter()
                .filter(|run| params.jobs.iter().any(|job| job.job_id == run.job_id))
                .filter(|run| params.since.is_none_or(|since| run.updated_at > since))
                .cloned()
                .collect())
        }

        async fn read_scheduled_job_run_log(
            &self,
            params: ScheduledJobRunLogParams,
        ) -> CoreResult<DeploymentLogChunk> {
            self.record("read_scheduled_job_run_log")?;
            let content = if params.offset == 0 {
                "mock job output\n"
            } else {
                ""
            };
            Ok(DeploymentLogChunk {
                content: content.into(),
                next_offset: params.offset + content.len() as u64,
                eof: true,
            })
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
        async fn test_s3_target(&self, _params: TestS3Params) -> CoreResult<()> {
            self.record("test_s3_target")
        }

        async fn list_backups(&self, _params: ListBackupsParams) -> CoreResult<Vec<BackupObject>> {
            self.record("list_backups")?;
            Ok(vec![])
        }

        async fn backup_database(&self, params: BackupDatabaseParams) -> CoreResult<BackupOutcome> {
            self.record("backup_database")?;
            Ok(BackupOutcome {
                key: format!("{}/mock.sql.gz", params.prefix),
                size_bytes: 0,
                pruned: 0,
            })
        }

        async fn restore_database(&self, _params: RestoreDatabaseParams) -> CoreResult<()> {
            self.record("restore_database")
        }

        async fn backup_volume(&self, params: BackupVolumeParams) -> CoreResult<BackupOutcome> {
            self.record("backup_volume")?;
            Ok(BackupOutcome {
                key: format!("{}/mock.tar.gz", params.prefix),
                size_bytes: 0,
                pruned: 0,
            })
        }

        async fn restore_volume(&self, _params: RestoreVolumeParams) -> CoreResult<()> {
            self.record("restore_volume")
        }

        async fn read_nginx_stats(
            &self,
            params: ReadNginxStatsParams,
        ) -> CoreResult<NginxStatsChunk> {
            self.record("read_nginx_stats")?;
            Ok(self
                .nginx_chunks
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(NginxStatsChunk {
                    buckets: Vec::new(),
                    file_id: params.file_id,
                    next_offset: params.offset,
                    lines: 0,
                    skipped: 0,
                    log_present: true,
                }))
        }

        async fn prune_docker(&self, params: PruneDockerParams) -> CoreResult<DockerPruneOutcome> {
            self.record("prune_docker")?;
            self.prunes.lock().unwrap().push(params);
            Ok(self
                .prune_outcome
                .lock()
                .unwrap()
                .clone()
                .unwrap_or(DockerPruneOutcome {
                    images_freed_bytes: 0,
                    builder_freed_bytes: 0,
                }))
        }

        async fn metrics_snapshot(&self) -> CoreResult<Option<crate::metrics::MetricsSnapshot>> {
            self.record("metrics_snapshot")?;
            Ok(self.metrics.lock().unwrap().clone())
        }

        async fn write_vhost_secrets(&self, params: WriteVhostSecretsParams) -> CoreResult<()> {
            self.record("write_vhost_secrets")?;
            self.vhost_secret_writes.lock().unwrap().push(params);
            Ok(())
        }

        async fn open_terminal(
            &self,
            params: OpenTerminalParams,
        ) -> CoreResult<TerminalSessionInfo> {
            self.record("open_terminal")?;
            self.terminals.lock().unwrap().push(params);
            Ok(TerminalSessionInfo {
                session_id: Uuid::new_v4(),
            })
        }

        async fn read_terminal(&self, _params: TerminalReadParams) -> CoreResult<TerminalOutput> {
            self.record("read_terminal")?;
            Ok(TerminalOutput::default())
        }

        async fn write_terminal(&self, _params: TerminalWriteParams) -> CoreResult<()> {
            self.record("write_terminal")
        }

        async fn resize_terminal(&self, _params: TerminalResizeParams) -> CoreResult<()> {
            self.record("resize_terminal")
        }

        async fn close_terminal(&self, _params: TerminalCloseParams) -> CoreResult<()> {
            self.record("close_terminal")
        }
    }
}
