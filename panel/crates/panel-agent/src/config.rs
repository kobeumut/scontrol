//! Agent configuration, loaded from `/etc/scontrol/agent.toml`.
//!
//! Optional features (`docker`, `mail`, `antivirus`) are gated in two ways:
//! 1. At compile time via Cargo features (see `Cargo.toml`) — code for a
//!    feature that was never selected at install time is not even built
//!    into the binary.
//! 2. At runtime via this config, in case an operator wants to temporarily
//!    disable an installed feature without rebuilding.

use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Clone, Deserialize)]
pub struct AgentConfig {
    /// Path to the Unix domain socket the API server connects to.
    #[serde(default = "default_socket_path")]
    pub socket_path: PathBuf,

    /// Root directory under which per-site document roots and system users'
    /// home directories are created.
    #[serde(default = "default_sites_root")]
    pub sites_root: PathBuf,

    #[serde(default = "default_nginx_available")]
    pub nginx_sites_available: PathBuf,
    #[serde(default = "default_nginx_enabled")]
    pub nginx_sites_enabled: PathBuf,

    #[serde(default = "default_systemd_dir")]
    pub systemd_unit_dir: PathBuf,

    /// Agent-owned, per-project Compose source, environment, and logs.
    /// Only read in a Docker-feature build.
    #[allow(dead_code)]
    #[serde(default = "default_docker_projects_root")]
    pub docker_projects_root: PathBuf,

    /// Where Docker keeps named volumes (`<root>/<project>_<volume>/_data`).
    /// Read by volume backups.
    #[allow(dead_code)]
    #[serde(default = "default_docker_volumes_root")]
    pub docker_volumes_root: PathBuf,

    /// Webroot served for `/.well-known/acme-challenge/` on every
    /// panel-generated Compose domain vhost (certbot `--webroot`).
    #[allow(dead_code)]
    #[serde(default = "default_acme_webroot")]
    pub acme_webroot: PathBuf,

    /// Where certbot stores live certificates (`<dir>/<domain>/fullchain.pem`).
    #[allow(dead_code)]
    #[serde(default = "default_letsencrypt_live_dir")]
    pub letsencrypt_live_dir: PathBuf,

    /// Upper bound for one Compose build/deploy (`docker compose up`).
    #[allow(dead_code)]
    #[serde(default = "default_deploy_timeout_secs")]
    pub deploy_timeout_secs: u64,

    /// Per-deployment log cap; output beyond it is dropped with a marker.
    #[allow(dead_code)]
    #[serde(default = "default_deployment_log_max_bytes")]
    pub deployment_log_max_bytes: u64,

    /// Deployment log files kept per project; older ones are pruned.
    #[allow(dead_code)]
    #[serde(default = "default_deployment_logs_retained")]
    pub deployment_logs_retained: usize,

    /// Compose releases kept per project for rollbacks (pinned image tags
    /// plus the deployed Compose file); older ones are pruned with their
    /// tags. `0` records no releases. Capped at 20.
    #[allow(dead_code)]
    #[serde(default = "default_rollback_releases_retained")]
    pub rollback_releases_retained: usize,

    /// Release directories of static sites, served by Nginx directly
    /// (`<root>/<slug>/releases/<deployment>/`). Every ancestor must be
    /// traversable by the Nginx worker user, so this lives under `/srv`
    /// rather than the private `/var/lib/scontrol`.
    #[allow(dead_code)]
    #[serde(default = "default_static_sites_root")]
    pub static_sites_root: PathBuf,

    /// Pinned Railpack BuildKit frontend image (`BUILDKIT_SYNTAX`). The
    /// installer writes the tag matching the installed `railpack` binary.
    #[allow(dead_code)]
    #[serde(default = "default_railpack_frontend")]
    pub railpack_frontend: String,

    /// Cloud Native Buildpacks builder image for `pack build`.
    #[allow(dead_code)]
    #[serde(default = "default_buildpacks_builder")]
    pub buildpacks_builder: String,

    /// Directory of generated Nginx `stream` servers that expose database
    /// services. The installer includes it from a top-level `stream {}`
    /// block; the agent refuses to expose anything when it is not loaded.
    #[allow(dead_code)]
    #[serde(default = "default_nginx_stream_dir")]
    pub nginx_stream_dir: PathBuf,

    /// Per-domain Basic Auth hashes and uploaded certificates. Outside the
    /// root-only data directory so the Nginx worker can reach its htpasswd.
    #[allow(dead_code)]
    #[serde(default = "default_nginx_vhost_secrets_dir")]
    pub nginx_vhost_secrets_dir: PathBuf,
    /// Main Nginx config, read for the worker `user` directive.
    #[allow(dead_code)]
    #[serde(default = "default_nginx_main_conf")]
    pub nginx_main_conf: PathBuf,
    #[allow(dead_code)]
    #[serde(default = "default_passwd_file")]
    pub passwd_file: PathBuf,
    #[allow(dead_code)]
    #[serde(default = "default_group_file")]
    pub group_file: PathBuf,

    /// The Nginx JSON access log (`scontrol_json` format) that request
    /// statistics are read from.
    #[serde(default = "default_nginx_stats_log")]
    pub nginx_stats_log: PathBuf,

    /// Seconds between metrics samples, clamped to 10–30.
    #[serde(default = "default_metrics_interval_secs")]
    pub metrics_interval_secs: u64,

    /// `/proc` as the metrics sampler reads it (overridable for tests).
    #[serde(default = "default_metrics_proc_root")]
    pub metrics_proc_root: PathBuf,

    /// `/sys/block`: block devices listed here are whole disks.
    #[serde(default = "default_metrics_sys_block_dir")]
    pub metrics_sys_block_dir: PathBuf,

    /// The cgroup v2 mount, holding container `docker-<id>.scope` groups.
    #[serde(default = "default_cgroup_root")]
    pub cgroup_root: PathBuf,

    /// Docker's data root; Compose labels are read from
    /// `<root>/containers/<id>/config.v2.json`.
    #[serde(default = "default_docker_data_root")]
    pub docker_data_root: PathBuf,

    #[serde(default)]
    pub runtime_features: RuntimeFeatures,

    /// Serve the same RPC to a remote panel over mutual TLS. Absent on the
    /// panel's own host, which uses only the Unix socket.
    #[serde(default)]
    pub remote: Option<RemoteConfig>,

    /// The file this configuration was loaded from. Generated scheduled-job
    /// units pass it back to the agent's helper mode.
    #[allow(dead_code)]
    #[serde(skip)]
    pub config_path: PathBuf,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RuntimeFeatures {
    #[serde(default = "default_true")]
    pub docker: bool,
    #[serde(default = "default_true")]
    pub mail: bool,
    #[serde(default = "default_true")]
    pub antivirus: bool,
}

/// Matches the per-field serde defaults, so an `agent.toml` without a
/// `[runtime_features]` section keeps compiled-in features enabled. (A
/// derived `Default` would turn every feature off in that case.)
impl Default for RuntimeFeatures {
    fn default() -> Self {
        Self {
            docker: true,
            mail: true,
            antivirus: true,
        }
    }
}

/// `[remote]`: the mutual-TLS listener of an agent on a remote host. The
/// key never leaves this host: `panel-agent remote-init` writes it with a
/// CSR, the panel signs the CSR, and the operator installs the returned
/// certificate and the panel CA next to it. A new certificate file is picked
/// up on the next connection, so rotation needs no restart.
#[derive(Debug, Clone, Deserialize)]
pub struct RemoteConfig {
    /// `address:port` to listen on, for example `0.0.0.0:7843`.
    pub listen: String,
    #[serde(default = "default_remote_ca")]
    pub ca_cert: PathBuf,
    #[serde(default = "default_remote_cert")]
    pub cert: PathBuf,
    /// Must be a regular file with mode 0600 (or stricter).
    #[serde(default = "default_remote_key")]
    pub key: PathBuf,
}

pub const DEFAULT_REMOTE_DIR: &str = "/etc/scontrol/remote";

fn default_remote_ca() -> PathBuf {
    PathBuf::from(DEFAULT_REMOTE_DIR).join("ca.pem")
}

fn default_remote_cert() -> PathBuf {
    PathBuf::from(DEFAULT_REMOTE_DIR).join("agent.pem")
}

fn default_remote_key() -> PathBuf {
    PathBuf::from(DEFAULT_REMOTE_DIR).join("agent.key")
}

/// Where the installer writes `agent.toml`.
pub const DEFAULT_CONFIG_PATH: &str = "/etc/scontrol/agent.toml";

fn default_true() -> bool {
    true
}

fn default_socket_path() -> PathBuf {
    PathBuf::from("/run/scontrol/agent.sock")
}

fn default_sites_root() -> PathBuf {
    PathBuf::from("/srv/scontrol/sites")
}

fn default_nginx_available() -> PathBuf {
    PathBuf::from("/etc/nginx/sites-available")
}

fn default_nginx_enabled() -> PathBuf {
    PathBuf::from("/etc/nginx/sites-enabled")
}

fn default_systemd_dir() -> PathBuf {
    PathBuf::from("/etc/systemd/system")
}

fn default_nginx_vhost_secrets_dir() -> PathBuf {
    PathBuf::from("/etc/nginx/scontrol-vhost")
}

fn default_nginx_main_conf() -> PathBuf {
    PathBuf::from("/etc/nginx/nginx.conf")
}

fn default_passwd_file() -> PathBuf {
    PathBuf::from("/etc/passwd")
}

fn default_group_file() -> PathBuf {
    PathBuf::from("/etc/group")
}

fn default_docker_projects_root() -> PathBuf {
    PathBuf::from("/var/lib/scontrol/docker")
}

fn default_docker_volumes_root() -> PathBuf {
    PathBuf::from("/var/lib/docker/volumes")
}

fn default_acme_webroot() -> PathBuf {
    PathBuf::from("/var/lib/scontrol/acme")
}

fn default_letsencrypt_live_dir() -> PathBuf {
    PathBuf::from("/etc/letsencrypt/live")
}

fn default_deploy_timeout_secs() -> u64 {
    30 * 60
}

fn default_deployment_log_max_bytes() -> u64 {
    5 * 1024 * 1024
}

fn default_deployment_logs_retained() -> usize {
    20
}

fn default_rollback_releases_retained() -> usize {
    5
}

fn default_static_sites_root() -> PathBuf {
    PathBuf::from("/srv/scontrol/static")
}

fn default_nginx_stats_log() -> PathBuf {
    PathBuf::from("/var/log/nginx/scontrol-access.json.log")
}

fn default_nginx_stream_dir() -> PathBuf {
    PathBuf::from("/etc/nginx/scontrol-stream")
}

fn default_metrics_interval_secs() -> u64 {
    panel_core::metrics::DEFAULT_SAMPLE_INTERVAL_SECS
}

fn default_metrics_proc_root() -> PathBuf {
    PathBuf::from("/proc")
}

fn default_metrics_sys_block_dir() -> PathBuf {
    PathBuf::from("/sys/block")
}

fn default_cgroup_root() -> PathBuf {
    PathBuf::from("/sys/fs/cgroup")
}

fn default_docker_data_root() -> PathBuf {
    PathBuf::from("/var/lib/docker")
}

fn default_railpack_frontend() -> String {
    "ghcr.io/railwayapp/railpack-frontend:v0.43.0".into()
}

fn default_buildpacks_builder() -> String {
    "paketobuildpacks/builder-jammy-base".into()
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            socket_path: default_socket_path(),
            sites_root: default_sites_root(),
            nginx_sites_available: default_nginx_available(),
            nginx_sites_enabled: default_nginx_enabled(),
            systemd_unit_dir: default_systemd_dir(),
            nginx_vhost_secrets_dir: default_nginx_vhost_secrets_dir(),
            nginx_main_conf: default_nginx_main_conf(),
            passwd_file: default_passwd_file(),
            group_file: default_group_file(),
            docker_projects_root: default_docker_projects_root(),
            docker_volumes_root: default_docker_volumes_root(),
            acme_webroot: default_acme_webroot(),
            letsencrypt_live_dir: default_letsencrypt_live_dir(),
            deploy_timeout_secs: default_deploy_timeout_secs(),
            deployment_log_max_bytes: default_deployment_log_max_bytes(),
            deployment_logs_retained: default_deployment_logs_retained(),
            rollback_releases_retained: default_rollback_releases_retained(),
            static_sites_root: default_static_sites_root(),
            railpack_frontend: default_railpack_frontend(),
            buildpacks_builder: default_buildpacks_builder(),
            nginx_stream_dir: default_nginx_stream_dir(),
            nginx_stats_log: default_nginx_stats_log(),
            metrics_interval_secs: default_metrics_interval_secs(),
            metrics_proc_root: default_metrics_proc_root(),
            metrics_sys_block_dir: default_metrics_sys_block_dir(),
            cgroup_root: default_cgroup_root(),
            docker_data_root: default_docker_data_root(),
            runtime_features: RuntimeFeatures {
                docker: true,
                mail: true,
                antivirus: true,
            },
            remote: None,
            config_path: PathBuf::from(DEFAULT_CONFIG_PATH),
        }
    }
}

impl AgentConfig {
    pub fn load(path: &std::path::Path) -> anyhow::Result<Self> {
        let config_path = std::path::absolute(path)?;
        if !path.exists() {
            tracing::warn!(path = %path.display(), "agent config not found, using defaults");
            return Ok(Self {
                config_path,
                ..Self::default()
            });
        }
        let raw = std::fs::read_to_string(path)?;
        let mut cfg: AgentConfig = toml::from_str(&raw)?;
        cfg.config_path = config_path;
        Ok(cfg)
    }

    pub fn docker_enabled(&self) -> bool {
        cfg!(feature = "docker") && self.runtime_features.docker
    }

    pub fn mail_enabled(&self) -> bool {
        cfg!(feature = "mail") && self.runtime_features.mail
    }

    pub fn antivirus_enabled(&self) -> bool {
        cfg!(feature = "antivirus") && self.runtime_features.antivirus
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_features_default_on_with_or_without_the_section() {
        let cfg: AgentConfig = toml::from_str("").unwrap();
        assert!(cfg.runtime_features.docker && cfg.runtime_features.mail);
        assert!(cfg.runtime_features.antivirus);
        let cfg: AgentConfig = toml::from_str("[runtime_features]\nmail = false\n").unwrap();
        assert!(cfg.runtime_features.docker && !cfg.runtime_features.mail);
    }
}
