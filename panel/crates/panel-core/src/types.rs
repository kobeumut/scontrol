//! Shared domain types for the server control panel.
//!
//! These types are used across `panel-api` (business logic / persistence)
//! and `panel-agent` (host operations) as the wire format for RPC requests,
//! and by `panel-cli` for presentation.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;
use uuid::Uuid;

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
    pub source: ComposeSource,
    pub status: DeploymentStatus,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
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
}

/// What the agent reports after a successful Compose deployment.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ComposeDeployOutcome {
    pub services: Vec<ComposeServiceInfo>,
    pub commit_sha: Option<String>,
    /// Non-fatal problems, e.g. a certificate that could not be issued yet.
    pub warnings: Vec<String>,
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

    #[cfg(test)]
    mod tests {
        use super::*;

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
        fn username_rules() {
            assert!(is_valid_username("deploy"));
            assert!(is_valid_username("web-user_1"));
            assert!(!is_valid_username("ab")); // too short (min length 3)
            assert!(!is_valid_username("UpperCase"));
            assert!(!is_valid_username("has space"));
        }
    }
}
