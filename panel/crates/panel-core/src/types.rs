//! Shared domain types for the server control panel.
//!
//! These types are used across `panel-api` (business logic / persistence)
//! and `panel-agent` (host operations) as the wire format for RPC requests,
//! and by `panel-cli` for presentation.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;
use uuid::Uuid;

pub use crate::database_services::{
    DatabaseCredentials, DatabaseDeployment, DatabaseExposure, DatabaseServiceSpec,
    ManagedDatabaseEngine, DATABASE_SERVICE_NAME,
};

/// A supported application runtime for a hosted site.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Runtime {
    /// PHP served via PHP-FPM, fronted by Nginx.
    Php { version: String },
    /// Node.js application, run as a systemd unit behind an Nginx reverse proxy.
    NodeJs { version: String, entrypoint: String },
    /// Python (WSGI/ASGI) application, run via a systemd unit behind Nginx.
    Python { version: String, entrypoint: String },
    /// Plain static file hosting.
    Static,
    /// Reverse proxy to an arbitrary upstream (e.g. another local service).
    ReverseProxy { upstream: String },
}

impl fmt::Display for Runtime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Runtime::Php { version } => write!(f, "php:{version}"),
            Runtime::NodeJs { version, .. } => write!(f, "node:{version}"),
            Runtime::Python { version, .. } => write!(f, "python:{version}"),
            Runtime::Static => write!(f, "static"),
            Runtime::ReverseProxy { upstream } => write!(f, "proxy:{upstream}"),
        }
    }
}

/// Lifecycle status of a site.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SiteStatus {
    Provisioning,
    Active,
    Disabled,
    Failed,
}

impl fmt::Display for SiteStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            SiteStatus::Provisioning => "provisioning",
            SiteStatus::Active => "active",
            SiteStatus::Disabled => "disabled",
            SiteStatus::Failed => "failed",
        };
        write!(f, "{s}")
    }
}

impl std::str::FromStr for SiteStatus {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "provisioning" => Ok(SiteStatus::Provisioning),
            "active" => Ok(SiteStatus::Active),
            "disabled" => Ok(SiteStatus::Disabled),
            "failed" => Ok(SiteStatus::Failed),
            other => Err(format!("unknown site status: {other}")),
        }
    }
}

/// A hosted site/application.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Site {
    pub id: Uuid,
    pub domain: String,
    pub owner_user_id: Uuid,
    pub runtime: Runtime,
    pub document_root: String,
    pub ssl_enabled: bool,
    pub status: SiteStatus,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Supported database engines for per-site databases.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DatabaseEngine {
    Postgres,
    MySql,
}

impl fmt::Display for DatabaseEngine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            DatabaseEngine::Postgres => "postgres",
            DatabaseEngine::MySql => "mysql",
        };
        write!(f, "{s}")
    }
}

impl std::str::FromStr for DatabaseEngine {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "postgres" => Ok(DatabaseEngine::Postgres),
            "mysql" => Ok(DatabaseEngine::MySql),
            other => Err(format!("unknown database engine: {other}")),
        }
    }
}

/// Metadata about a provisioned per-site database (secrets are never stored in
/// plaintext outside of the initial creation response).
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DatabaseInfo {
    pub id: Uuid,
    pub site_id: Option<Uuid>,
    pub engine: DatabaseEngine,
    pub name: String,
    pub username: String,
    pub created_at: DateTime<Utc>,
}

/// A panel user account.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct User {
    pub id: Uuid,
    pub username: String,
    pub email: String,
    pub is_admin: bool,
    pub totp_enabled: bool,
    pub created_at: DateTime<Utc>,
}

/// A UFW firewall rule.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct FirewallRule {
    pub id: Uuid,
    pub port: u16,
    pub protocol: FirewallProtocol,
    pub description: String,
    pub allow: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FirewallProtocol {
    Tcp,
    Udp,
}

impl fmt::Display for FirewallProtocol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FirewallProtocol::Tcp => write!(f, "tcp"),
            FirewallProtocol::Udp => write!(f, "udp"),
        }
    }
}

impl std::str::FromStr for FirewallProtocol {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "tcp" => Ok(FirewallProtocol::Tcp),
            "udp" => Ok(FirewallProtocol::Udp),
            other => Err(format!("unknown firewall protocol: {other}")),
        }
    }
}

/// A cron job scoped to a site's owning system user.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CronJob {
    pub id: Uuid,
    pub site_id: Uuid,
    pub schedule: String,
    pub command: String,
    pub enabled: bool,
}

/// Optional install-time features. Kept as an explicit, closed set so the
/// installer, agent, and API all agree on what "optional" means.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OptionalFeature {
    Docker,
    Mail,
    Antivirus,
}

impl fmt::Display for OptionalFeature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            OptionalFeature::Docker => "docker",
            OptionalFeature::Mail => "mail",
            OptionalFeature::Antivirus => "antivirus",
        };
        write!(f, "{s}")
    }
}

impl OptionalFeature {
    pub const ALL: [OptionalFeature; 3] = [
        OptionalFeature::Docker,
        OptionalFeature::Mail,
        OptionalFeature::Antivirus,
    ];
}

/// Dokploy-inspired source for a Docker Compose project. Git source is
/// checked out by the root agent into an isolated, panel-owned directory;
/// raw Compose is written to that same directory. Environment values are
/// never included in this model or persisted in panel state.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ComposeSource {
    Git {
        repository_url: String,
        branch: String,
        compose_path: String,
    },
    Raw {
        compose_yaml: String,
    },
    /// A single-service application. The agent renders it into a generated
    /// Compose file, so applications share the Compose deploy engine
    /// (queue, logs, environment, domains, webhooks).
    Application(Box<ApplicationSpec>),
    /// Static files from a Git checkout (optionally built in a throwaway
    /// container), copied into a release directory and served by the host
    /// Nginx. No container runs once the deployment finishes.
    Static(Box<StaticSiteSpec>),
    /// A managed database (PostgreSQL, MySQL, MariaDB, MongoDB, Redis)
    /// rendered by the agent from a vetted template: loopback-only port,
    /// named volume, generated credentials. Created through the database
    /// services API, which also allocates its port and credentials.
    Database(Box<DatabaseServiceSpec>),
}

impl ComposeSource {
    /// Repository URL and branch for sources that deploy from Git.
    pub fn git_checkout(&self) -> Option<(&str, &str)> {
        match self {
            ComposeSource::Git {
                repository_url,
                branch,
                ..
            } => Some((repository_url, branch)),
            ComposeSource::Application(spec) => match &spec.build {
                ApplicationBuild::Dockerfile {
                    repository_url,
                    branch,
                    ..
                }
                | ApplicationBuild::Builder {
                    repository_url,
                    branch,
                    ..
                } => Some((repository_url, branch)),
                ApplicationBuild::Image { .. } => None,
            },
            ComposeSource::Static(spec) => Some((&spec.repository_url, &spec.branch)),
            ComposeSource::Raw { .. } | ComposeSource::Database(_) => None,
        }
    }

    /// Short label for audit records and listings.
    pub fn kind(&self) -> &'static str {
        match self {
            ComposeSource::Git { .. } => "git",
            ComposeSource::Raw { .. } => "raw",
            ComposeSource::Application(spec) => match spec.build {
                ApplicationBuild::Image { .. } => "image",
                ApplicationBuild::Dockerfile { .. } => "dockerfile",
                ApplicationBuild::Builder { builder, .. } => builder.as_str(),
            },
            ComposeSource::Static(_) => "static",
            ComposeSource::Database(spec) => spec.engine.as_str(),
        }
    }
}

/// An opt-in image builder for repositories without a Dockerfile. Each is a
/// pinned binary the installer adds only when asked, and the agent runs it
/// only when compiled with the `builders` feature.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BuildTool {
    /// `nixpacks build`.
    Nixpacks,
    /// `railpack prepare` plus `docker buildx build` with the Railpack
    /// BuildKit frontend.
    Railpack,
    /// Cloud Native Buildpacks through `pack build`.
    Buildpacks,
}

impl BuildTool {
    pub const ALL: [BuildTool; 3] = [
        BuildTool::Nixpacks,
        BuildTool::Railpack,
        BuildTool::Buildpacks,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            BuildTool::Nixpacks => "nixpacks",
            BuildTool::Railpack => "railpack",
            BuildTool::Buildpacks => "buildpacks",
        }
    }

    /// The program the agent runs for this builder.
    pub fn program(self) -> &'static str {
        match self {
            BuildTool::Nixpacks => "nixpacks",
            BuildTool::Railpack => "railpack",
            BuildTool::Buildpacks => "pack",
        }
    }
}

/// Image a builder produces for a project. It exists only in the local
/// Docker image store; the generated Compose file never pulls it.
pub fn built_image_tag(project_slug: &str) -> String {
    format!("scontrol-app/{project_slug}:latest")
}

/// Local tag that pins the image one service ran in a deployment, so a
/// rollback can start exactly that image again:
/// `<slug>/<service>:deploy-<deployment>`. `None` when the service name is
/// not a valid lowercase image path component (the release is then not
/// recorded rather than tagged under a different name).
pub fn deployment_image_tag(
    project_slug: &str,
    service: &str,
    deployment_id: Uuid,
) -> Option<String> {
    static COMPONENT_RE: once_cell::sync::Lazy<regex::Regex> = once_cell::sync::Lazy::new(|| {
        regex::Regex::new(r"^[a-z0-9]+(?:(?:[._]|__|-+)[a-z0-9]+)*$").unwrap()
    });
    if !validate::is_valid_project_slug(project_slug)
        || !validate::is_valid_service_name(service)
        || !COMPONENT_RE.is_match(service)
    {
        return None;
    }
    let tag = format!("{project_slug}/{service}:deploy-{deployment_id}");
    validate::is_valid_image_reference(&tag).then_some(tag)
}

/// One service image pinned by a successful deployment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PinnedImage {
    pub service: String,
    /// [`deployment_image_tag`] of the service in that deployment.
    pub tag: String,
    /// Content ID the tag pointed to when it was recorded (`sha256:<hex>`).
    pub image_id: String,
}

/// Non-secret record of what a successful deployment ran, stored with its
/// deployment row and used to roll back to it. It never holds a value of
/// the environment or any other credential: environment and build-arg
/// values, inline secret/config content, extension fields, and comments
/// are dropped from the Compose file, and every known secret value is
/// masked. A rollback reads the current environment file instead.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DeploymentSnapshot {
    /// Compose file path relative to the source tree.
    pub compose_path: String,
    /// SHA-256 (hex) of the deployed Compose file. The agent's own copy of
    /// the release must still match it for a rollback to run.
    pub compose_sha256: String,
    /// The deployed Compose file, sanitized as described above.
    pub compose_file: String,
    /// The generated override (loopback ports for domains), if any.
    #[serde(default)]
    pub override_file: Option<String>,
    /// Names of the environment variables the deployment had.
    #[serde(default)]
    pub env_keys: Vec<String>,
    /// Images pinned per service. Services without a running container
    /// (one-off tasks) and without a `build` section keep their Compose
    /// image.
    #[serde(default)]
    pub images: Vec<PinnedImage>,
}

/// The release a rollback deployment starts again. Built by panel-api from
/// the stored [`DeploymentSnapshot`]; the agent checks it against its own
/// release record before it runs anything.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RollbackTarget {
    pub deployment_id: Uuid,
    pub compose_sha256: String,
    pub images: Vec<PinnedImage>,
}

/// Where an application's container image comes from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ApplicationBuild {
    /// A published image, pulled by `docker compose up` (`--pull always` when
    /// the deployment asks for fresh images).
    Image { image: String },
    /// `docker build` of a Dockerfile in a Git checkout. The context and
    /// Dockerfile are relative to the repository root and must resolve
    /// inside it.
    Dockerfile {
        repository_url: String,
        branch: String,
        #[serde(default = "default_build_context")]
        context_path: String,
        #[serde(default = "default_dockerfile")]
        dockerfile_path: String,
        /// Multi-stage build target.
        #[serde(default)]
        target: Option<String>,
    },
    /// A Git checkout without a Dockerfile, turned into an image by an
    /// opt-in builder that detects the language. Build-time variables are
    /// not passed (the builders only take them on argv); the environment
    /// still reaches the running container through `env_file`.
    Builder {
        builder: BuildTool,
        repository_url: String,
        branch: String,
        /// Application directory relative to the repository root.
        #[serde(default = "default_build_context")]
        context_path: String,
    },
}

fn default_build_context() -> String {
    ".".into()
}

/// Optional build of a static site, run before publishing. Each command
/// runs as `docker run` of `image` with the checkout mounted at
/// `/workspace`, in exec form (no shell), with all capabilities dropped.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct StaticBuild {
    /// Toolchain image, e.g. `node:22-alpine`.
    pub image: String,
    /// Commands run in order, e.g. `[["npm", "ci"], ["npm", "run", "build"]]`.
    pub commands: Vec<Vec<String>>,
}

/// A static site deployed from Git and served by the host Nginx.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct StaticSiteSpec {
    pub repository_url: String,
    pub branch: String,
    /// Directory to publish, relative to the repository root (after the
    /// build, if any). Must resolve inside the checkout.
    #[serde(default = "default_build_context")]
    pub publish_path: String,
    #[serde(default)]
    pub build: Option<StaticBuild>,
    /// Serve `index.html` for unknown paths (single-page applications).
    #[serde(default)]
    pub spa: bool,
}

/// Name the domain editor uses for a static site's single "service".
pub const STATIC_SERVICE_NAME: &str = "static";

impl StaticSiteSpec {
    pub fn validate(&self) -> Result<(), String> {
        if !validate::is_valid_git_url(&self.repository_url) {
            return Err("invalid Git repository URL".into());
        }
        if !validate::is_valid_git_branch(&self.branch) {
            return Err("invalid Git branch".into());
        }
        if !validate::is_valid_relative_path(&self.publish_path, true) {
            return Err("invalid publish path".into());
        }
        if let Some(build) = &self.build {
            if !validate::is_valid_image_reference(&build.image) {
                return Err("invalid build image reference".into());
            }
            if build.commands.is_empty() || build.commands.len() > 8 {
                return Err("a static build needs 1..=8 commands".into());
            }
            if !build
                .commands
                .iter()
                .all(|c| validate::is_valid_exec_argv(c))
            {
                return Err("each build command must be 1..=32 non-empty arguments".into());
            }
        }
        Ok(())
    }
}

fn default_dockerfile() -> String {
    "Dockerfile".into()
}

#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "kebab-case")]
pub enum RestartPolicy {
    No,
    Always,
    #[default]
    UnlessStopped,
    OnFailure,
}

impl RestartPolicy {
    pub fn as_compose(self) -> &'static str {
        match self {
            RestartPolicy::No => "no",
            RestartPolicy::Always => "always",
            RestartPolicy::UnlessStopped => "unless-stopped",
            RestartPolicy::OnFailure => "on-failure",
        }
    }
}

/// Container health check. The command runs inside the application's own
/// container as an exec-form argv (`CMD`), never through a shell.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ApplicationHealthCheck {
    pub command: Vec<String>,
    #[serde(default = "default_health_interval")]
    pub interval_secs: u32,
    #[serde(default = "default_health_timeout")]
    pub timeout_secs: u32,
    #[serde(default = "default_health_retries")]
    pub retries: u32,
    #[serde(default)]
    pub start_period_secs: u32,
}

fn default_health_interval() -> u32 {
    30
}

fn default_health_timeout() -> u32 {
    10
}

fn default_health_retries() -> u32 {
    3
}

/// A Docker named volume mounted into the application. Host bind mounts are
/// not offered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ApplicationVolume {
    pub name: String,
    /// Absolute path inside the container.
    pub target: String,
}

/// A single-service application (Dokploy's "application"), rendered by the
/// agent into a generated Compose project. The project environment file is
/// passed to the container as `env_file`, so environment values stay
/// write-only exactly as for Compose projects.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ApplicationSpec {
    pub build: ApplicationBuild,
    /// Compose service name; domain bindings target it.
    #[serde(default = "default_application_service")]
    pub service_name: String,
    /// Port the application listens on inside the container (documentation
    /// for domain bindings; nothing is published by it).
    #[serde(default)]
    pub container_port: Option<u16>,
    #[serde(default)]
    pub restart: RestartPolicy,
    /// Memory limit in MiB.
    #[serde(default)]
    pub memory_limit_mb: Option<u32>,
    /// CPU limit in thousandths of a CPU (`500` = half a CPU).
    #[serde(default)]
    pub cpu_limit_millis: Option<u32>,
    #[serde(default)]
    pub health_check: Option<ApplicationHealthCheck>,
    #[serde(default)]
    pub volumes: Vec<ApplicationVolume>,
}

fn default_application_service() -> String {
    "app".into()
}

impl ApplicationSpec {
    /// Shared validation for panel-api and the agent. Returns a message
    /// that is safe to show to the user.
    pub fn validate(&self) -> Result<(), String> {
        match &self.build {
            ApplicationBuild::Image { image } => {
                if !validate::is_valid_image_reference(image) {
                    return Err("invalid image reference".into());
                }
            }
            ApplicationBuild::Dockerfile {
                repository_url,
                branch,
                context_path,
                dockerfile_path,
                target,
            } => {
                if !validate::is_valid_git_url(repository_url) {
                    return Err("invalid Git repository URL".into());
                }
                if !validate::is_valid_git_branch(branch) {
                    return Err("invalid Git branch".into());
                }
                if !validate::is_valid_relative_path(context_path, true) {
                    return Err("invalid build context path".into());
                }
                if !validate::is_valid_relative_path(dockerfile_path, false) {
                    return Err("invalid Dockerfile path".into());
                }
                if target
                    .as_deref()
                    .is_some_and(|t| !validate::is_valid_service_name(t))
                {
                    return Err("invalid build target".into());
                }
            }
            ApplicationBuild::Builder {
                repository_url,
                branch,
                context_path,
                ..
            } => {
                if !validate::is_valid_git_url(repository_url) {
                    return Err("invalid Git repository URL".into());
                }
                if !validate::is_valid_git_branch(branch) {
                    return Err("invalid Git branch".into());
                }
                if !validate::is_valid_relative_path(context_path, true) {
                    return Err("invalid build context path".into());
                }
            }
        }
        if !validate::is_valid_service_name(&self.service_name) {
            return Err("invalid service name".into());
        }
        if self.container_port == Some(0) {
            return Err("container port must be 1..=65535".into());
        }
        if self
            .memory_limit_mb
            .is_some_and(|mb| !(6..=1024 * 1024).contains(&mb))
        {
            return Err("memory limit must be 6 MiB..=1 TiB".into());
        }
        if self
            .cpu_limit_millis
            .is_some_and(|m| !(10..=512_000).contains(&m))
        {
            return Err("CPU limit must be 10..=512000 millicpus".into());
        }
        if let Some(check) = &self.health_check {
            if !validate::is_valid_exec_argv(&check.command) {
                return Err("health check command must be 1..=32 non-empty arguments".into());
            }
            if !(1..=3600).contains(&check.interval_secs)
                || !(1..=3600).contains(&check.timeout_secs)
                || !(1..=100).contains(&check.retries)
                || check.start_period_secs > 3600
            {
                return Err("health check timings out of range".into());
            }
        }
        if self.volumes.len() > 16 {
            return Err("at most 16 volumes".into());
        }
        let mut targets = std::collections::HashSet::new();
        for volume in &self.volumes {
            if !validate::is_valid_volume_name(&volume.name)
                || !validate::is_valid_container_path(&volume.target)
                || !targets.insert(volume.target.as_str())
            {
                return Err(format!("invalid volume {}", volume.name));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DeploymentStatus {
    Queued,
    Running,
    Succeeded,
    Failed,
    Stopped,
    Cancelled,
}

impl fmt::Display for DeploymentStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = match self {
            DeploymentStatus::Queued => "queued",
            DeploymentStatus::Running => "running",
            DeploymentStatus::Succeeded => "succeeded",
            DeploymentStatus::Failed => "failed",
            DeploymentStatus::Stopped => "stopped",
            DeploymentStatus::Cancelled => "cancelled",
        };
        write!(f, "{value}")
    }
}

impl std::str::FromStr for DeploymentStatus {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "queued" => Ok(Self::Queued),
            "running" => Ok(Self::Running),
            "succeeded" => Ok(Self::Succeeded),
            "failed" => Ok(Self::Failed),
            "stopped" => Ok(Self::Stopped),
            "cancelled" => Ok(Self::Cancelled),
            other => Err(format!("unknown deployment status: {other}")),
        }
    }
}

/// Metadata for a Docker Compose project. Its environment is deliberately
/// absent; environment secrets are write-only and live only in a 0600 file
/// under the project's agent-owned directory.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ComposeProject {
    pub id: Uuid,
    pub name: String,
    pub slug: String,
    pub owner_user_id: Uuid,
    /// The environment (inside a project) this service belongs to; its
    /// variables inherit project → environment → service.
    pub environment_id: Uuid,
    /// False for a service created before environment layering whose own
    /// variables the panel never saw: deployments keep its env file as is
    /// (no inheritance) until its environment is saved once.
    pub env_layered: bool,
    /// The server (agent) the service is placed on; the nil UUID is the
    /// panel's own host.
    #[serde(default)]
    pub server_id: Uuid,
    pub source: ComposeSource,
    pub status: DeploymentStatus,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// A group of environments (for example one product), owned by a user.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Project {
    pub id: Uuid,
    pub name: String,
    pub slug: String,
    pub owner_user_id: Uuid,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// A stage of a project (`production`, `staging`, ...) holding services.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Environment {
    pub id: Uuid,
    pub project_id: Uuid,
    pub name: String,
    pub slug: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Most variables one layer (project, environment, or service) may hold.
pub const MAX_ENV_VARS_PER_LAYER: usize = 256;
/// Most variables a merged service environment may hold (three full layers).
pub const MAX_MERGED_ENV_VARS: usize = 3 * MAX_ENV_VARS_PER_LAYER;

/// Where a variable is defined. Ordered from least to most specific: a
/// later layer overrides an earlier one on the same key.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum EnvLayer {
    Project,
    Environment,
    Service,
}

impl EnvLayer {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::Environment => "environment",
            Self::Service => "service",
        }
    }
}

/// A variable as the API shows it: the key and where it comes from, never
/// the value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct EnvKeyInfo {
    pub key: String,
    /// The layer whose value wins.
    pub source: EnvLayer,
    /// Less specific layers that define the key too and are overridden.
    pub overrides: Vec<EnvLayer>,
}

/// Key names of one layer as stored; values stay write-only.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct EnvLayerKeys {
    pub layer: EnvLayer,
    pub keys: Vec<String>,
}

/// Merges layers project → environment → service: on a shared key the most
/// specific layer wins. Layers may be given in any order. The result keeps
/// first-definition order of keys. Also returns values that were overridden
/// (still secrets: the agent masks them in logs too).
pub fn merge_env_layers(
    layers: &[(EnvLayer, Vec<(String, String)>)],
) -> (Vec<(String, String)>, Vec<String>) {
    let mut ordered: Vec<&(EnvLayer, Vec<(String, String)>)> = layers.iter().collect();
    ordered.sort_by_key(|(layer, _)| *layer);
    let mut merged: Vec<(String, String)> = Vec::new();
    let mut index: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut shadowed = Vec::new();
    for (_, vars) in ordered {
        for (key, value) in vars {
            match index.get(key) {
                Some(&at) => {
                    let previous = std::mem::replace(&mut merged[at].1, value.clone());
                    if previous != *value {
                        shadowed.push(previous);
                    }
                }
                None => {
                    index.insert(key.clone(), merged.len());
                    merged.push((key.clone(), value.clone()));
                }
            }
        }
    }
    (merged, shadowed)
}

/// Explains, key by key, where each merged variable comes from and which
/// layers it overrides. Takes key names only, so it never touches values.
pub fn describe_env_layers(layers: &[EnvLayerKeys]) -> Vec<EnvKeyInfo> {
    let mut ordered: Vec<&EnvLayerKeys> = layers.iter().collect();
    ordered.sort_by_key(|l| l.layer);
    let mut out: Vec<EnvKeyInfo> = Vec::new();
    for layer in ordered {
        for key in &layer.keys {
            match out.iter_mut().find(|info| &info.key == key) {
                Some(info) if info.source != layer.layer => {
                    info.overrides.push(info.source);
                    info.source = layer.layer;
                }
                Some(_) => {}
                None => out.push(EnvKeyInfo {
                    key: key.clone(),
                    source: layer.layer,
                    overrides: Vec::new(),
                }),
            }
        }
    }
    out.sort_by(|a, b| a.key.cmp(&b.key));
    out
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ComposeServiceInfo {
    /// Compose service name.
    pub name: String,
    /// Container name (`<project>-<service>-<n>`).
    #[serde(default)]
    pub container: String,
    pub image: String,
    /// `running`, `exited`, `restarting`, ...
    pub state: String,
    /// Human status, e.g. `Up 5 minutes (healthy)`.
    #[serde(default)]
    pub status: String,
    /// `healthy`, `unhealthy`, `starting`, or empty without a health check.
    #[serde(default)]
    pub health: String,
    #[serde(default)]
    pub exit_code: i32,
    /// `127.0.0.1:40000->80/tcp`, or `80/tcp` when not published.
    pub ports: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DeploymentInfo {
    pub id: Uuid,
    pub project_id: Uuid,
    pub status: DeploymentStatus,
    pub log_path: String,
    /// What queued it: `manual`, `api`, `mcp` (later: `webhook`).
    pub trigger: String,
    /// Git commit the deployment built from; `None` for raw Compose.
    pub commit_sha: Option<String>,
    pub queued_at: DateTime<Utc>,
    /// `None` while the job is still queued.
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub error_message: Option<String>,
    /// For a rollback: the deployment whose release it started again.
    #[serde(default)]
    pub rollback_of: Option<Uuid>,
    /// The deployment succeeded, recorded a snapshot, and the agent still
    /// keeps its pinned images, so it can be rolled back to.
    #[serde(default)]
    pub rollback_available: bool,
}

/// What the agent reports after a successful Compose deployment.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ComposeDeployOutcome {
    pub services: Vec<ComposeServiceInfo>,
    pub commit_sha: Option<String>,
    /// Non-fatal problems, e.g. a certificate that could not be issued yet.
    pub warnings: Vec<String>,
    /// The release recorded for this deployment (not for rollbacks, static
    /// sites, or database services).
    #[serde(default)]
    pub snapshot: Option<DeploymentSnapshot>,
    /// Deployments of the project whose releases the agent still keeps,
    /// newest first. Older ones were pruned with their image tags.
    #[serde(default)]
    pub retained_releases: Vec<Uuid>,
}

/// An explicit binding from a public domain to one Compose service port.
/// The agent publishes the container port on `127.0.0.1:<host_port>` only and
/// generates an Nginx reverse-proxy vhost for the domain; nothing is exposed
/// on a public interface by the binding itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ComposeDomainBinding {
    pub domain: String,
    pub service_name: String,
    pub container_port: u16,
    pub host_port: u16,
    pub https: bool,
}

/// Persisted form of [`ComposeDomainBinding`].
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ComposeDomain {
    pub id: Uuid,
    pub project_id: Uuid,
    pub domain: String,
    pub service_name: String,
    pub container_port: u16,
    pub host_port: u16,
    pub https: bool,
    pub created_at: DateTime<Utc>,
}

/// A slice of a deployment log, read incrementally by byte offset.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DeploymentLogChunk {
    pub content: String,
    pub next_offset: u64,
    /// True when `next_offset` reached the current end of the file. A running
    /// deployment can still append, so clients also check deployment status.
    pub eof: bool,
}

/// A Docker-managed container remains available as a low-level compatibility
/// operation, but the panel UI/API should use Compose projects instead.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ContainerInfo {
    pub id: String,
    pub name: String,
    pub image: String,
    pub status: String,
    pub ports: Vec<String>,
}

/// A mail domain + mailbox summary (only meaningful when the `mail` feature
/// is enabled on the agent).
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct MailboxInfo {
    pub address: String,
    pub domain: String,
    pub quota_mb: u32,
}

/// Result of an on-demand antivirus scan (only meaningful when the
/// `antivirus` feature is enabled on the agent).
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ScanResult {
    pub path: String,
    pub infected_files: Vec<String>,
    pub scanned_files: u64,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
}

/// Simple validation helpers reused by both API and agent so untrusted input
/// is rejected before it ever reaches a `Command`.
pub mod validate {
    use once_cell::sync::Lazy;
    use regex::Regex;

    static DOMAIN_RE: Lazy<Regex> = Lazy::new(|| {
        Regex::new(r"^(?:[a-zA-Z0-9](?:[a-zA-Z0-9-]{0,61}[a-zA-Z0-9])?\.)+[a-zA-Z]{2,63}$").unwrap()
    });
    static IDENT_RE: Lazy<Regex> =
        Lazy::new(|| Regex::new(r"^[a-zA-Z][a-zA-Z0-9_]{0,62}$").unwrap());
    static USERNAME_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"^[a-z][a-z0-9_-]{2,31}$").unwrap());
    static PROJECT_SLUG_RE: Lazy<Regex> =
        Lazy::new(|| Regex::new(r"^[a-z][a-z0-9-]{2,47}$").unwrap());
    static ENV_KEY_RE: Lazy<Regex> =
        Lazy::new(|| Regex::new(r"^[A-Za-z_][A-Za-z0-9_]{0,127}$").unwrap());
    static SERVICE_NAME_RE: Lazy<Regex> =
        Lazy::new(|| Regex::new(r"^[a-zA-Z0-9][a-zA-Z0-9_.-]{0,62}$").unwrap());
    static GIT_BRANCH_RE: Lazy<Regex> =
        Lazy::new(|| Regex::new(r"^[A-Za-z0-9][A-Za-z0-9._/-]{0,127}$").unwrap());

    /// Validates a domain name (used before it is ever passed to `Command`).
    pub fn is_valid_domain(domain: &str) -> bool {
        domain.len() <= 253 && DOMAIN_RE.is_match(domain)
    }

    /// Validates an identifier suitable for database names, system usernames
    /// with underscores, cron job names, etc. Deliberately conservative.
    pub fn is_valid_identifier(ident: &str) -> bool {
        IDENT_RE.is_match(ident)
    }

    /// Validates a POSIX-friendly login username for site system accounts.
    pub fn is_valid_username(username: &str) -> bool {
        USERNAME_RE.is_match(username)
    }

    /// Stable Docker Compose project ID; avoids collision-prone generated
    /// names and is safe for Docker's project-name argument.
    pub fn is_valid_project_slug(slug: &str) -> bool {
        PROJECT_SLUG_RE.is_match(slug)
    }

    /// Allows only a relative `.yml`/`.yaml` path without traversal.
    pub fn is_valid_compose_path(path: &str) -> bool {
        !path.is_empty()
            && path.len() <= 256
            && !path.starts_with('/')
            && !path
                .split('/')
                .any(|segment| segment.is_empty() || segment == "." || segment == "..")
            && (path.ends_with(".yml") || path.ends_with(".yaml"))
    }

    /// Compose service names as accepted by Docker Compose.
    pub fn is_valid_service_name(name: &str) -> bool {
        SERVICE_NAME_RE.is_match(name)
    }

    /// A Docker image reference: `[registry[:port]/]path[:tag][@sha256:digest]`
    /// with lowercase path components. No option-looking or whitespace input.
    pub fn is_valid_image_reference(image: &str) -> bool {
        image.len() <= 255 && IMAGE_REF_RE.is_match(image)
    }

    static IMAGE_REF_RE: Lazy<Regex> = Lazy::new(|| {
        Regex::new(concat!(
            r"^(?:[a-zA-Z0-9](?:[a-zA-Z0-9-]*[a-zA-Z0-9])?(?:\.[a-zA-Z0-9](?:[a-zA-Z0-9-]*[a-zA-Z0-9])?)*(?::[0-9]{1,5})?/)?",
            r"[a-z0-9]+(?:(?:[._]|__|-+)[a-z0-9]+)*(?:/[a-z0-9]+(?:(?:[._]|__|-+)[a-z0-9]+)*)*",
            r"(?::[A-Za-z0-9_][A-Za-z0-9_.-]{0,127})?(?:@sha256:[a-f0-9]{64})?$"
        ))
        .unwrap()
    });

    /// A path relative to a source tree: no leading `/`, no empty, `.` or
    /// `..` segments. `allow_root` additionally accepts `.` for the tree
    /// itself (a build context).
    pub fn is_valid_relative_path(path: &str, allow_root: bool) -> bool {
        if allow_root && path == "." {
            return true;
        }
        !path.is_empty()
            && path.len() <= 256
            && !path.starts_with('/')
            && !path.starts_with('-')
            && !path.contains(['\0', '\n', '\\'])
            && !path
                .split('/')
                .any(|segment| segment.is_empty() || segment == "." || segment == "..")
    }

    /// An exec-form command (health check, static build step): 1..=32
    /// non-empty arguments without NUL or newlines. It never reaches a shell.
    pub fn is_valid_exec_argv(command: &[String]) -> bool {
        !command.is_empty()
            && command.len() <= 32
            && command
                .iter()
                .all(|arg| !arg.is_empty() && arg.len() <= 1024 && !arg.contains(['\0', '\n']))
    }

    /// Docker named volume name.
    pub fn is_valid_volume_name(name: &str) -> bool {
        VOLUME_NAME_RE.is_match(name)
    }

    static VOLUME_NAME_RE: Lazy<Regex> =
        Lazy::new(|| Regex::new(r"^[a-zA-Z0-9][a-zA-Z0-9_.-]{0,62}$").unwrap());

    /// Absolute path inside a container (a mount target). Rejects `:` so a
    /// value can never smuggle a mount mode or second path.
    pub fn is_valid_container_path(path: &str) -> bool {
        path.len() > 1
            && path.len() <= 256
            && path.starts_with('/')
            && !path.contains([':', '\0', '\n', '\\', ','])
            && !path[1..]
                .split('/')
                .any(|segment| segment.is_empty() || segment == "." || segment == "..")
    }

    pub fn is_valid_env_key(key: &str) -> bool {
        ENV_KEY_RE.is_match(key)
    }

    pub fn is_valid_git_branch(branch: &str) -> bool {
        GIT_BRANCH_RE.is_match(branch) && !branch.contains("..") && !branch.ends_with('/')
    }

    /// Permit HTTPS and SSH GitHub/GitLab/Gitea-style remote URLs, but not
    /// local paths, URL credentials, whitespace, or shell metacharacters.
    /// SSH remotes may name a login user (`ssh://git@host/org/repo.git` or
    /// `git@host:org/repo.git`); HTTPS remotes may not, because a user part
    /// there is how credentials leak into URLs.
    pub fn is_valid_git_url(url: &str) -> bool {
        url.len() <= 512 && !url.contains("..") && git_remote(url).is_some()
    }

    /// How the agent authenticates to a Git remote.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum GitTransport {
        Https,
        Ssh,
    }

    /// Transport and lower-cased host of a valid Git remote URL.
    pub fn git_remote(url: &str) -> Option<(GitTransport, String)> {
        if url.len() > 512 || url.contains("..") {
            return None;
        }
        let (transport, captures) = match GIT_HTTPS_RE.captures(url) {
            Some(c) => (GitTransport::Https, c),
            None => (
                GitTransport::Ssh,
                GIT_SSH_RE
                    .captures(url)
                    .or_else(|| GIT_SCP_RE.captures(url))?,
            ),
        };
        let host = captures.name("host")?.as_str();
        is_valid_git_host(host).then(|| (transport, host.to_ascii_lowercase()))
    }

    // A host is a DNS name (dots optional, so `localhost`-style LAN names
    // work); it never starts with `-`, so ssh cannot read it as an option.
    static GIT_HTTPS_RE: Lazy<Regex> = Lazy::new(|| {
        Regex::new(r"^https://(?P<host>[A-Za-z0-9.-]+)(?::[0-9]{1,5})?(?:/[A-Za-z0-9._~%+-]+)+/?$")
            .unwrap()
    });
    static GIT_SSH_RE: Lazy<Regex> = Lazy::new(|| {
        Regex::new(
            r"^ssh://(?:[A-Za-z0-9][A-Za-z0-9._-]{0,31}@)?(?P<host>[A-Za-z0-9.-]+)(?::[0-9]{1,5})?(?:/[A-Za-z0-9._~%+-]+)+/?$",
        )
        .unwrap()
    });
    static GIT_SCP_RE: Lazy<Regex> = Lazy::new(|| {
        Regex::new(
            r"^[A-Za-z0-9][A-Za-z0-9._-]{0,31}@(?P<host>[A-Za-z0-9.-]+):[A-Za-z0-9._~+][A-Za-z0-9._~%+/-]*$",
        )
        .unwrap()
    });

    /// A Git remote host name as accepted in remote URLs.
    pub fn is_valid_git_host(host: &str) -> bool {
        host.len() <= 253
            && host.split('.').all(|label| {
                !label.is_empty()
                    && label.len() <= 63
                    && !label.starts_with('-')
                    && !label.ends_with('-')
            })
    }

    /// A Git HTTPS username (`x-access-token`, `oauth2`, a login name).
    pub fn is_valid_git_username(username: &str) -> bool {
        GIT_USERNAME_RE.is_match(username)
    }

    static GIT_USERNAME_RE: Lazy<Regex> =
        Lazy::new(|| Regex::new(r"^[A-Za-z0-9][A-Za-z0-9._@+-]{0,99}$").unwrap());

    /// Key under which Docker stores Docker Hub credentials.
    pub const DOCKER_HUB_AUTH_KEY: &str = "https://index.docker.io/v1/";

    /// Normalizes a registry address to the key used in `config.json`
    /// `auths`: `host` or `host:port`, lowercase, no scheme or path. Docker
    /// Hub's names collapse to [`DOCKER_HUB_AUTH_KEY`]. `None` rejects
    /// anything else (userinfo, paths, whitespace, bad ports).
    pub fn registry_auth_key(server: &str) -> Option<String> {
        let trimmed = server.trim().to_ascii_lowercase();
        let host_port = trimmed.strip_prefix("https://").unwrap_or(&trimmed);
        let host_port = host_port.strip_suffix("/v1/").unwrap_or(host_port);
        let host_port = host_port.strip_suffix('/').unwrap_or(host_port);
        let (host, port) = match host_port.rsplit_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (host_port, None),
        };
        if host.is_empty()
            || !host
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
            || !is_valid_git_host(host)
        {
            return None;
        }
        if let Some(port) = port {
            if !(1..=5).contains(&port.len())
                || !port.chars().all(|c| c.is_ascii_digit())
                || port
                    .parse::<u32>()
                    .ok()
                    .filter(|p| (1..=65535).contains(p))
                    .is_none()
            {
                return None;
            }
        }
        if port.is_none()
            && matches!(
                host,
                "docker.io"
                    | "index.docker.io"
                    | "registry-1.docker.io"
                    | "registry.hub.docker.com"
            )
        {
            return Some(DOCKER_HUB_AUTH_KEY.to_string());
        }
        Some(host_port.to_string())
    }

    /// A registry password or access token: printable, no whitespace.
    pub fn is_valid_registry_secret(secret: &str) -> bool {
        !secret.is_empty()
            && secret.len() <= 4096
            && !secret.chars().any(|c| c.is_control() || c.is_whitespace())
    }

    /// `https://host[:port]` (or `http://` for a local MinIO), with no path,
    /// userinfo, query, or fragment. Returns the normalized endpoint.
    pub fn normalize_s3_endpoint(endpoint: &str) -> Option<String> {
        let trimmed = endpoint.trim().trim_end_matches('/');
        let (scheme, rest) = trimmed
            .strip_prefix("https://")
            .map(|rest| ("https", rest))
            .or_else(|| trimmed.strip_prefix("http://").map(|rest| ("http", rest)))?;
        let (host, port) = match rest.rsplit_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (rest, None),
        };
        if host.is_empty()
            || !host
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
            || !is_valid_git_host(host)
        {
            return None;
        }
        if let Some(port) = port {
            if port.parse::<u16>().ok().filter(|p| *p > 0).is_none()
                || !port.chars().all(|c| c.is_ascii_digit())
            {
                return None;
            }
        }
        Some(format!("{scheme}://{}", rest.to_ascii_lowercase()))
    }

    static S3_BUCKET_RE: Lazy<Regex> =
        Lazy::new(|| Regex::new(r"^[a-z0-9][a-z0-9.-]{1,61}[a-z0-9]$").unwrap());
    static S3_REGION_RE: Lazy<Regex> =
        Lazy::new(|| Regex::new(r"^[a-z0-9][a-z0-9-]{0,30}$").unwrap());
    static S3_KEY_ID_RE: Lazy<Regex> =
        Lazy::new(|| Regex::new(r"^[A-Za-z0-9._-]{1,128}$").unwrap());
    static BACKUP_PATH_RE: Lazy<Regex> = Lazy::new(|| {
        Regex::new(r"^[A-Za-z0-9][A-Za-z0-9._-]*(/[A-Za-z0-9][A-Za-z0-9._-]*)*$").unwrap()
    });

    pub fn is_valid_s3_bucket(bucket: &str) -> bool {
        S3_BUCKET_RE.is_match(bucket)
    }

    /// `auto` (Cloudflare R2) and `us-east-1` style names.
    pub fn is_valid_s3_region(region: &str) -> bool {
        S3_REGION_RE.is_match(region)
    }

    pub fn is_valid_s3_access_key(key: &str) -> bool {
        S3_KEY_ID_RE.is_match(key)
    }

    /// An S3 secret key: printable, no whitespace.
    pub fn is_valid_s3_secret_key(secret: &str) -> bool {
        !secret.is_empty()
            && secret.len() <= 256
            && !secret.chars().any(|c| c.is_control() || c.is_whitespace())
    }

    /// A backup key prefix or object key: `/`-separated segments of
    /// `[A-Za-z0-9._-]`, none starting with `.` or `-`, so no `..`, no empty
    /// segments, and no leading slash.
    pub fn is_valid_backup_path(path: &str) -> bool {
        path.len() <= 512 && BACKUP_PATH_RE.is_match(path) && !path.contains("..")
    }

    /// A database server host or IP literal. Never starts with `-`, so it
    /// cannot be read as an option by a dump tool.
    pub fn is_valid_db_host(host: &str) -> bool {
        !host.is_empty()
            && host.len() <= 253
            && host
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphanumeric() || c == ':')
            && host
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':'))
    }

    /// A database password: printable, no whitespace, at most 256 bytes.
    pub fn is_valid_db_password(password: &str) -> bool {
        !password.is_empty()
            && password.len() <= 256
            && !password
                .chars()
                .any(|c| c.is_control() || c.is_whitespace())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn backup_inputs_are_validated() {
            assert_eq!(
                normalize_s3_endpoint("https://ACC.r2.cloudflarestorage.com/").as_deref(),
                Some("https://acc.r2.cloudflarestorage.com")
            );
            assert_eq!(
                normalize_s3_endpoint("http://127.0.0.1:9000").as_deref(),
                Some("http://127.0.0.1:9000")
            );
            for bad in [
                "",
                "ftp://x.com",
                "https://u@x.com",
                "https://x.com/p",
                "https://x.com:0",
                "https://x.com:abc",
                "x.com",
            ] {
                assert_eq!(normalize_s3_endpoint(bad), None, "{bad:?}");
            }
            assert!(is_valid_s3_bucket("my-backups.01"));
            assert!(!is_valid_s3_bucket("Bad_Bucket"));
            assert!(is_valid_s3_region("auto") && is_valid_s3_region("eu-west-1"));
            assert!(is_valid_backup_path("scontrol/db-main"));
            for bad in ["", "/abs", "a//b", "a/../b", "a/.hidden", "-x", "a b", "a/"] {
                assert!(!is_valid_backup_path(bad), "{bad:?}");
            }
            assert!(
                is_valid_db_host("db.internal")
                    && is_valid_db_host("::1")
                    && is_valid_db_host("10.0.0.5")
            );
            assert!(!is_valid_db_host("-oProxyCommand") && !is_valid_db_host("a b"));
        }

        #[test]
        fn registry_addresses_normalize_to_docker_auth_keys() {
            assert_eq!(registry_auth_key("GHCR.io").as_deref(), Some("ghcr.io"));
            assert_eq!(
                registry_auth_key("https://registry.gitlab.com/").as_deref(),
                Some("registry.gitlab.com")
            );
            assert_eq!(
                registry_auth_key("123.dkr.ecr.eu-west-1.amazonaws.com").as_deref(),
                Some("123.dkr.ecr.eu-west-1.amazonaws.com")
            );
            assert_eq!(
                registry_auth_key("registry.local:5000").as_deref(),
                Some("registry.local:5000")
            );
            for hub in [
                "docker.io",
                "index.docker.io",
                "https://index.docker.io/v1/",
            ] {
                assert_eq!(registry_auth_key(hub).as_deref(), Some(DOCKER_HUB_AUTH_KEY));
            }
            for bad in [
                "",
                "user@ghcr.io",
                "ghcr.io/org",
                "ghcr.io:0",
                "ghcr.io:99999",
                "ghcr.io:x",
                "gh cr.io",
                "-ghcr.io",
                "http://ghcr.io",
            ] {
                assert_eq!(registry_auth_key(bad), None, "{bad:?}");
            }
        }

        #[test]
        fn accepts_valid_domains() {
            assert!(is_valid_domain("example.com"));
            assert!(is_valid_domain("sub.example.co.uk"));
        }

        #[test]
        fn rejects_invalid_domains() {
            assert!(!is_valid_domain(""));
            assert!(!is_valid_domain("not a domain"));
            assert!(!is_valid_domain("-leading-dash.com"));
            assert!(!is_valid_domain("no-tld"));
            // Shell-metacharacter injection attempts must never validate.
            assert!(!is_valid_domain("example.com; rm -rf /"));
            assert!(!is_valid_domain("$(whoami).com"));
        }

        #[test]
        fn identifier_rejects_sql_metacharacters() {
            assert!(is_valid_identifier("my_db_1"));
            assert!(!is_valid_identifier("my db"));
            assert!(!is_valid_identifier("db'; DROP TABLE users; --"));
            assert!(!is_valid_identifier("1leading_digit"));
        }

        #[test]
        fn git_urls_allow_ssh_users_but_not_https_credentials() {
            for (url, transport, host) in [
                (
                    "https://github.com/org/repo.git",
                    GitTransport::Https,
                    "github.com",
                ),
                (
                    "https://Git.Example.com:8443/a/b",
                    GitTransport::Https,
                    "git.example.com",
                ),
                (
                    "ssh://git@github.com/org/repo.git",
                    GitTransport::Ssh,
                    "github.com",
                ),
                (
                    "ssh://gitea.lan:2222/org/repo",
                    GitTransport::Ssh,
                    "gitea.lan",
                ),
                (
                    "git@gitlab.com:group/sub/repo.git",
                    GitTransport::Ssh,
                    "gitlab.com",
                ),
            ] {
                assert_eq!(
                    git_remote(url),
                    Some((transport, host.to_string())),
                    "{url}"
                );
                assert!(is_valid_git_url(url), "{url}");
            }
            for url in [
                "https://user:token@github.com/org/repo.git",
                "https://token@github.com/org/repo.git",
                "http://github.com/org/repo.git",
                "file:///etc/passwd",
                "/srv/repo.git",
                "ext::sh -c touch% /tmp/pwned",
                "ssh://-oProxyCommand=touch/x/y",
                "git@-oProxyCommand=x:repo",
                "-u@host:repo",
                "git@host:-repo",
                "git@host:../repo",
                "https://github.com/org/repo.git?x=1",
                "https://github.com/org/repo .git",
                "https://github.com",
                "git@a@host:repo",
            ] {
                assert!(!is_valid_git_url(url), "{url}");
            }
        }

        #[test]
        fn image_references() {
            for image in [
                "nginx",
                "nginx:1.27-alpine",
                "library/redis:7",
                "ghcr.io/org/app:v1.2.3",
                "registry.example.com:5000/team/app",
                "localhost:5000/app:latest",
                "my_org/my-app__x:tag",
                "postgres@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            ] {
                assert!(is_valid_image_reference(image), "{image}");
            }
            for image in [
                "",
                "-v",
                "--privileged",
                "Nginx",
                "nginx:latest --rm",
                "nginx;id",
                "nginx:-x",
                "nginx:$(id)",
                "/nginx",
                "nginx/",
                "a//b",
                "nginx@sha256:short",
            ] {
                assert!(!is_valid_image_reference(image), "{image}");
            }
        }

        #[test]
        fn relative_and_container_paths() {
            assert!(is_valid_relative_path(".", true));
            assert!(!is_valid_relative_path(".", false));
            assert!(is_valid_relative_path("docker/web.Dockerfile", false));
            for bad in ["", "/etc", "../x", "a/../b", "a//b", "-f", "a/./b", "a\\b"] {
                assert!(!is_valid_relative_path(bad, true), "{bad}");
            }
            assert!(is_valid_container_path("/var/lib/data"));
            for bad in ["/", "data", "/a/../b", "/a:/b", "/a,ro", "/a//b"] {
                assert!(!is_valid_container_path(bad), "{bad}");
            }
        }

        #[test]
        fn username_rules() {
            assert!(is_valid_username("deploy"));
            assert!(is_valid_username("web-user_1"));
            assert!(!is_valid_username("ab")); // too short (min length 3)
            assert!(!is_valid_username("UpperCase"));
            assert!(!is_valid_username("has space"));
        }
    }
}

#[cfg(test)]
mod env_layer_tests {
    use super::*;

    fn vars(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn most_specific_layer_wins_regardless_of_input_order() {
        let layers = vec![
            (
                EnvLayer::Service,
                vars(&[("DB_URL", "svc-db"), ("ONLY_SVC", "s")]),
            ),
            (
                EnvLayer::Project,
                vars(&[("DB_URL", "proj-db"), ("LOG", "info"), ("REGION", "eu")]),
            ),
            (
                EnvLayer::Environment,
                vars(&[("LOG", "debug"), ("DB_URL", "env-db")]),
            ),
        ];
        let (merged, shadowed) = merge_env_layers(&layers);
        assert_eq!(
            merged,
            vars(&[
                ("DB_URL", "svc-db"),
                ("LOG", "debug"),
                ("REGION", "eu"),
                ("ONLY_SVC", "s"),
            ])
        );
        let mut shadowed = shadowed;
        shadowed.sort();
        assert_eq!(shadowed, vec!["env-db", "info", "proj-db"]);
    }

    #[test]
    fn empty_layers_merge_to_the_remaining_ones() {
        let (merged, shadowed) = merge_env_layers(&[
            (EnvLayer::Project, vars(&[("A", "1")])),
            (EnvLayer::Environment, Vec::new()),
        ]);
        assert_eq!(merged, vars(&[("A", "1")]));
        assert!(shadowed.is_empty());
        assert!(merge_env_layers(&[]).0.is_empty());
    }

    #[test]
    fn describes_inherited_and_overridden_keys_without_values() {
        let info = describe_env_layers(&[
            EnvLayerKeys {
                layer: EnvLayer::Service,
                keys: vec!["DB_URL".into()],
            },
            EnvLayerKeys {
                layer: EnvLayer::Project,
                keys: vec!["DB_URL".into(), "REGION".into()],
            },
            EnvLayerKeys {
                layer: EnvLayer::Environment,
                keys: vec!["DB_URL".into()],
            },
        ]);
        assert_eq!(
            info,
            vec![
                EnvKeyInfo {
                    key: "DB_URL".into(),
                    source: EnvLayer::Service,
                    overrides: vec![EnvLayer::Project, EnvLayer::Environment],
                },
                EnvKeyInfo {
                    key: "REGION".into(),
                    source: EnvLayer::Project,
                    overrides: vec![],
                },
            ]
        );
    }
}

#[cfg(test)]
mod spec_tests {
    use super::*;

    fn site() -> StaticSiteSpec {
        StaticSiteSpec {
            repository_url: "https://github.com/acme/site.git".into(),
            branch: "main".into(),
            publish_path: "dist".into(),
            build: Some(StaticBuild {
                image: "node:22-alpine".into(),
                commands: vec![
                    vec!["npm".into(), "ci".into()],
                    vec!["npm".into(), "run".into(), "build".into()],
                ],
            }),
            spa: true,
        }
    }

    #[test]
    fn static_site_specs_are_validated() {
        site().validate().unwrap();
        let mut spec = site();
        spec.publish_path = "../outside".into();
        assert!(spec.validate().is_err());
        spec.publish_path = "/etc".into();
        assert!(spec.validate().is_err());
        let mut spec = site();
        spec.build.as_mut().unwrap().image = "node --privileged".into();
        assert!(spec.validate().is_err());
        let mut spec = site();
        spec.build.as_mut().unwrap().commands = vec![];
        assert!(spec.validate().is_err());
        let mut spec = site();
        spec.build.as_mut().unwrap().commands = vec![vec!["sh".into(), "-c\nid".into()]];
        assert!(spec.validate().is_err());
        let source = ComposeSource::Static(Box::new(site()));
        assert_eq!(source.kind(), "static");
        assert_eq!(
            source.git_checkout(),
            Some(("https://github.com/acme/site.git", "main"))
        );
    }

    #[test]
    fn builder_applications_deploy_from_git() {
        let json = serde_json::json!({
            "build": { "type": "builder", "builder": "railpack",
                       "repository_url": "git@github.com:acme/api.git", "branch": "main" }
        });
        let spec: ApplicationSpec = serde_json::from_value(json).unwrap();
        spec.validate().unwrap();
        let source = ComposeSource::Application(Box::new(spec.clone()));
        assert_eq!(source.kind(), "railpack");
        assert!(source.git_checkout().is_some());
        let mut bad = spec;
        bad.build = ApplicationBuild::Builder {
            builder: BuildTool::Nixpacks,
            repository_url: "https://github.com/acme/api.git".into(),
            branch: "main".into(),
            context_path: "../..".into(),
        };
        assert!(bad.validate().is_err());
        assert_eq!(built_image_tag("shop"), "scontrol-app/shop:latest");
        assert!(validate::is_valid_image_reference(&built_image_tag("shop")));
    }

    #[test]
    fn deployment_image_tags_are_valid_references_or_refused() {
        let id = Uuid::parse_str("0b7c3f1e-8a51-4d0c-9b8e-2f4a6c1d9e70").unwrap();
        let tag = deployment_image_tag("shop", "web", id).unwrap();
        assert_eq!(tag, "shop/web:deploy-0b7c3f1e-8a51-4d0c-9b8e-2f4a6c1d9e70");
        assert!(validate::is_valid_image_reference(&tag));
        assert!(deployment_image_tag("shop", "api_worker-2", id).is_some());
        // Not a lowercase image path component, or an invalid slug: refused
        // rather than tagged under a rewritten name.
        for service in ["Web", "web.", "-web", "a b", "web:latest", "../x", ""] {
            assert!(
                deployment_image_tag("shop", service, id).is_none(),
                "{service}"
            );
        }
        assert!(deployment_image_tag("Shop", "web", id).is_none());
        assert!(deployment_image_tag("shop/x", "web", id).is_none());
    }
}
