//! Dokploy-inspired, Compose-project deployment adapter.
//!
//! This is deliberately not a general `docker run` control surface. Each
//! deployment has a stable, validated project slug and an isolated directory:
//! `<docker_projects_root>/<slug>/`. The agent accepts only a Git source or
//! raw Compose YAML, keeps the write-only environment in a 0600 file outside
//! the source tree, applies a conservative Compose policy, and invokes Docker
//! with a fixed argv vector.
//!
//! Layout of a project directory (mode 0700):
//!
//! ```text
//! code/                     checked-out Git source or raw Compose file
//! env                       0600 merged environment file (`--env-file`)
//! env.masked                0600 overridden inherited values, masked in logs
//! scontrol.override.yml     generated loopback port publishing for domains
//! .scontrol-compose-path    Compose file path relative to code/
//! domains.json              domains whose vhosts this project owns
//! logs/<deployment>.log     streamed, redacted, bounded deployment logs
//! deploy_key, known_hosts,  private Git access (see `git_auth`)
//! git-credentials
//! build/                    builder output that is not an image (Railpack plan)
//! static-release            static sites only: the release being served
//! db.env                    database services: 0600 generated credentials
//! redis.conf                Redis services: 0400 config holding the password
//! exposure.json             database services: applied outside access
//! jobs/<id>/                scheduled job definition, run records and logs
//!                           (see `scheduled_jobs`)
//! ```
//!
//! A database service (`ComposeSource::Database`) is rendered from a vetted
//! template: the port is published on `127.0.0.1` only, data lives in a
//! named volume, and credentials come from `db.env` (see
//! `panel_core::database_services`).
//!
//! A static site (`ComposeSource::Static`) runs no container: its files are
//! copied into `<static_sites_root>/<slug>/releases/<deployment>/`, which the
//! generated Nginx vhost serves directly.

use super::db_exposure;
use super::deploy_log::{self, maskable_secrets, redact, DeployLog};
use super::docker_config;
use super::exec::{follow_lines, run, run_allow_nonzero, run_streamed};
use super::git_auth;
use super::nginx::{self, VhostTarget};
use super::releases;
use crate::config::AgentConfig;
use panel_core::adapters::{
    ComposeProjectActionParams, ComposeProjectLogsParams, DeployComposeProjectParams,
    DeploymentLogParams, FollowComposeLogsParams, LogLines, WriteComposeEnvironmentParams,
    LOG_LINES_CAPACITY,
};
use panel_core::error::{CoreError, CoreResult};
use panel_core::types::{
    validate, ApplicationBuild, ApplicationSpec, ComposeDeployOutcome, ComposeDomainBinding,
    ComposeServiceInfo, ComposeSource, DatabaseDeployment, DatabaseServiceSpec, DeploymentLogChunk,
    StaticSiteSpec, DATABASE_SERVICE_NAME, MAX_MERGED_ENV_VARS,
};
use serde_yaml::Value as Yaml;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;

const DEFAULT_COMPOSE_FILE: &str = "docker-compose.yml";
/// Generated Compose file for a Dockerfile application, inside the checkout
/// so the build context resolves relative to the repository root.
const APPLICATION_COMPOSE_FILE: &str = ".scontrol-app.yml";
const ENV_FILE: &str = "env";
/// Overridden inherited values (0600), read only to mask them in logs.
const MASKED_VALUES_FILE: &str = "env.masked";
const OVERRIDE_FILE: &str = "scontrol.override.yml";
/// Generated credentials of a database service (0600), the last `env_file`.
const DATABASE_ENV_FILE: &str = "db.env";
/// Redis configuration holding the password, mounted as a Compose secret.
const REDIS_CONFIG_FILE: &str = "redis.conf";
/// Where the Redis secret appears inside the container.
const REDIS_SECRET_NAME: &str = "redis_conf";
/// UID and GID of the `redis` user in the official images; the server reads
/// its config after dropping root, so the 0400 file must belong to it.
const REDIS_CONTAINER_UID: u32 = 999;
const COMPOSE_PATH_MARKER: &str = ".scontrol-compose-path";
const DOMAINS_FILE: &str = "domains.json";
/// Present only for static sites; holds the deployment ID being served.
const STATIC_MARKER: &str = "static-release";
/// Static releases kept per site, the served one included.
const STATIC_RELEASES_RETAINED: usize = 3;
const MAX_DOMAIN_BINDINGS: usize = 32;
const GIT_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const CERTBOT_TIMEOUT: Duration = Duration::from_secs(5 * 60);

pub async fn deploy(
    cfg: &AgentConfig,
    params: DeployComposeProjectParams,
    cancel: watch::Receiver<bool>,
) -> CoreResult<ComposeDeployOutcome> {
    validate_project_params(&params)?;
    let root = project_root(cfg, &params.project_slug);
    create_private_dir(&root).await?;

    let env_path = root.join(ENV_FILE);
    adopt_legacy_env_file(&root, &env_path).await;
    let mut secrets = environment_secrets(&root).await?;
    secrets.extend(git_auth::stored_secrets(&root).await);
    secrets.extend(docker_config::stored_secrets(&root).await);
    secrets.extend(params.git_credentials.iter().map(|c| c.token.clone()));
    if let Some(database) = &params.database {
        secrets.extend(database.credentials.secrets());
    }
    let mut log = DeployLog::create(
        &root.join("logs"),
        params.deployment_id,
        cfg.deployment_log_max_bytes,
        secrets,
        cancel,
    )
    .await?;

    let result = match &params.source {
        ComposeSource::Static(spec) => deploy_static(cfg, &params, spec, &root, &mut log).await,
        _ => deploy_steps(cfg, &params, &root, &env_path, &mut log).await,
    };
    // Registry logins live only for the deployment, whatever its outcome.
    tokio::fs::remove_dir_all(docker_config::config_dir(&root))
        .await
        .ok();
    git_auth::clear_ephemeral(&root).await;
    match &result {
        Ok(outcome) => {
            for warning in &outcome.warnings {
                log.step(&format!("warning: {warning}")).await.ok();
            }
            log.step("deployment finished").await.ok();
        }
        Err(CoreError::Cancelled(_)) => {
            log.step("deployment cancelled").await.ok();
        }
        Err(error) => {
            log.step(&format!("deployment failed: {error}")).await.ok();
        }
    }
    deploy_log::prune(&root.join("logs"), cfg.deployment_logs_retained)
        .await
        .ok();
    result
}

async fn deploy_steps(
    cfg: &AgentConfig,
    params: &DeployComposeProjectParams,
    root: &Path,
    env_path: &Path,
    log: &mut DeployLog,
) -> CoreResult<ComposeDeployOutcome> {
    let code_dir = root.join("code");
    let (compose_path, commit_sha) = match &params.rollback {
        Some(target) => {
            let path =
                releases::prepare_rollback(root, &code_dir, &params.project_slug, target, log)
                    .await?;
            (path, None)
        }
        None => {
            log.step("preparing source").await?;
            materialize_source(cfg, params, root, &code_dir, log).await?
        }
    };
    ensure_inside(&code_dir, &compose_path).await?;
    let agent_files = [
        env_path.to_path_buf(),
        root.join(DATABASE_ENV_FILE),
        root.join(REDIS_CONFIG_FILE),
    ];
    let compose = validate_compose_file(&compose_path, &code_dir, &agent_files).await?;
    check_bindings_against_compose(&compose, &params.domains)?;
    persist_compose_path(root, &code_dir, &compose_path).await?;

    let override_path = root.join(OVERRIDE_FILE);
    let override_body = if params.domains.is_empty() {
        let _ = tokio::fs::remove_file(&override_path).await;
        None
    } else {
        let body = render_override(&params.domains)?;
        tokio::fs::write(&override_path, &body)
            .await
            .map_err(|e| CoreError::Io(format!("writing Compose override: {e}")))?;
        Some(body)
    };

    let paths = ProjectPaths {
        code_dir,
        compose: compose_path,
        env: Some(env_path.to_path_buf()),
        override_file: (!params.domains.is_empty()).then_some(override_path),
    };
    // A builder's image exists only locally; there is nothing to pull.
    let builds_locally = matches!(
        &params.source,
        ComposeSource::Application(spec) if matches!(spec.build, ApplicationBuild::Builder { .. })
    );
    // A rollback only swaps containers: its images are pinned local tags.
    let rollback = params.rollback.is_some();
    let args = compose_up_args(
        &params.project_slug,
        &paths,
        !rollback,
        params.pull_images && !builds_locally && !rollback,
    );
    log.step("starting services with docker compose").await?;
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let timeout = Duration::from_secs(cfg.deploy_timeout_secs.max(60));
    // Pulls use the project's own registry logins, never root's `~/.docker`.
    let docker_config = docker_config::config_dir(root)
        .to_string_lossy()
        .into_owned();
    let envs = [("DOCKER_CONFIG", docker_config.as_str())];
    let code = run_streamed("docker", &refs, &envs, log, timeout).await?;
    if code != 0 {
        return Err(CoreError::External(format!(
            "docker compose up failed (exit code {code}); see the deployment log"
        )));
    }

    let warnings = apply_domains(
        cfg,
        &params.project_slug,
        root,
        &params.domains,
        params.acme_email.as_deref(),
        None,
        log,
    )
    .await?;
    let mut warnings = warnings;
    let keep = cfg.rollback_releases_retained;
    let (snapshot, current) = match &params.rollback {
        Some(target) => {
            if let Err(error) = releases::mark_current(root, target.deployment_id).await {
                warnings.push(format!("marking the release as current failed: {error}"));
            }
            (None, target.deployment_id)
        }
        // Database services are never rolled back: an older engine image
        // may not read data a newer one has written.
        None if matches!(params.source, ComposeSource::Database(_)) || keep == 0 => {
            (None, params.deployment_id)
        }
        None => {
            let snapshot = record_release(
                params,
                root,
                &paths,
                &compose,
                override_body.as_deref(),
                log,
            )
            .await;
            match snapshot {
                Ok(snapshot) => (Some(snapshot), params.deployment_id),
                Err(error) => {
                    warnings.push(format!(
                        "rollback to this deployment is unavailable: {error}"
                    ));
                    (None, params.deployment_id)
                }
            }
        }
    };
    let retained_releases = if keep == 0 {
        Vec::new()
    } else {
        releases::prune(root, &params.project_slug, current, keep).await
    };
    let services = list_services_for(cfg, &params.project_slug).await?;
    Ok(ComposeDeployOutcome {
        services,
        commit_sha,
        warnings,
        snapshot,
        retained_releases,
    })
}

/// Pins the images of a successful deployment and records its release.
async fn record_release(
    params: &DeployComposeProjectParams,
    root: &Path,
    paths: &ProjectPaths,
    compose: &Yaml,
    override_body: Option<&str>,
    log: &mut DeployLog,
) -> CoreResult<panel_core::types::DeploymentSnapshot> {
    let compose_body = tokio::fs::read_to_string(&paths.compose)
        .await
        .map_err(|e| CoreError::Io(format!("reading deployed Compose file: {e}")))?;
    let compose_path = paths
        .compose
        .strip_prefix(&paths.code_dir)
        .map_err(|_| CoreError::Validation("Compose path escaped project code directory".into()))?
        .to_string_lossy()
        .into_owned();
    let mut containers = Vec::new();
    if let Some(services) = compose.get("services").and_then(Yaml::as_mapping) {
        for name in services.keys().filter_map(Yaml::as_str) {
            let ids = service_container_ids(&params.project_slug, paths, name).await?;
            containers.push((name.to_string(), ids.into_iter().next()));
        }
    }
    let env_keys = read_environment(&root.join(ENV_FILE))
        .await?
        .into_iter()
        .map(|(key, _)| key)
        .collect();
    releases::record(
        releases::DeployedRelease {
            slug: &params.project_slug,
            deployment_id: params.deployment_id,
            root,
            compose_path: &compose_path,
            compose_body: &compose_body,
            compose,
            override_body,
            env_keys,
            containers,
        },
        log,
    )
    .await
}

/// Replaces the project's environment file; the next deployment uses it.
pub async fn write_project_environment(
    cfg: &AgentConfig,
    params: WriteComposeEnvironmentParams,
) -> CoreResult<()> {
    if !validate::is_valid_project_slug(&params.project_slug) {
        return Err(CoreError::Validation(format!(
            "invalid project slug: {}",
            params.project_slug
        )));
    }
    validate_environment(&params.environment)?;
    validate_masked_values(&params.masked_values)?;
    let root = project_root(cfg, &params.project_slug);
    create_private_dir(&root).await?;
    write_environment(&root.join(ENV_FILE), &params.environment).await?;
    write_private_file(
        &root.join(MASKED_VALUES_FILE),
        &params
            .masked_values
            .iter()
            .map(|value| format!("{value}\n"))
            .collect::<String>(),
    )
    .await
}

/// Every environment value of the project, including overridden inherited
/// ones, for masking in deployment and container logs.
pub(crate) async fn environment_secrets(root: &Path) -> CoreResult<Vec<String>> {
    let mut secrets: Vec<String> = read_environment(&root.join(ENV_FILE))
        .await?
        .into_iter()
        .map(|(_, value)| value)
        .collect();
    if root.join(DATABASE_ENV_FILE).is_file() {
        secrets.extend(
            read_environment(&root.join(DATABASE_ENV_FILE))
                .await?
                .into_iter()
                .map(|(_, value)| value),
        );
    }
    match tokio::fs::read_to_string(root.join(MASKED_VALUES_FILE)).await {
        Ok(body) => secrets.extend(body.lines().map(str::to_string)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(CoreError::Io(format!("reading masked values: {e}"))),
    }
    Ok(secrets)
}

fn validate_masked_values(values: &[String]) -> CoreResult<()> {
    if values.len() > MAX_MERGED_ENV_VARS
        || values
            .iter()
            .any(|v| v.contains('\0') || v.contains('\n') || v.len() > 16 * 1024)
    {
        return Err(CoreError::Validation("invalid masked values".into()));
    }
    Ok(())
}

pub async fn stop(cfg: &AgentConfig, params: ComposeProjectActionParams) -> CoreResult<()> {
    if validate::is_valid_project_slug(&params.project_slug)
        && static_release(cfg, &params.project_slug).is_some()
    {
        return Err(CoreError::Validation(
            "a static site runs no containers; remove its domains to stop serving it".into(),
        ));
    }
    let paths = existing_project_paths(cfg, &params.project_slug)?;
    let mut args = compose_args(&params.project_slug, &paths);
    args.extend(["stop".into()]);
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let (code, stdout, stderr) = run_allow_nonzero("docker", &refs).await?;
    write_action_log(&project_root(cfg, &params.project_slug), &stdout, &stderr).await?;
    if code != 0 {
        return Err(CoreError::External(format!(
            "docker compose stop failed (exit code {code})"
        )));
    }
    Ok(())
}

/// `docker compose restart` of the last deployed project model.
pub async fn restart(cfg: &AgentConfig, params: ComposeProjectActionParams) -> CoreResult<()> {
    if validate::is_valid_project_slug(&params.project_slug)
        && static_release(cfg, &params.project_slug).is_some()
    {
        return Err(CoreError::Validation(
            "a static site runs no containers to restart".into(),
        ));
    }
    let paths = existing_project_paths(cfg, &params.project_slug)?;
    let mut args = compose_args(&params.project_slug, &paths);
    args.extend(["restart".into()]);
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let (code, stdout, stderr) = run_allow_nonzero("docker", &refs).await?;
    write_action_log(&project_root(cfg, &params.project_slug), &stdout, &stderr).await?;
    if code != 0 {
        return Err(CoreError::External(format!(
            "docker compose restart failed (exit code {code})"
        )));
    }
    Ok(())
}

/// `docker compose stop|start <service>` for one service, used around a
/// volume backup or restore.
pub async fn service_action(
    cfg: &AgentConfig,
    slug: &str,
    action: &str,
    service: &str,
) -> CoreResult<()> {
    if !validate::is_valid_service_name(service) || !matches!(action, "stop" | "start") {
        return Err(CoreError::Validation("invalid service action".into()));
    }
    let paths = existing_project_paths(cfg, slug)?;
    let mut args = compose_args(slug, &paths);
    args.extend([action.to_string(), service.to_string()]);
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let (code, _, _) = run_allow_nonzero("docker", &refs).await?;
    if code != 0 {
        return Err(CoreError::External(format!(
            "docker compose {action} {service} failed (exit code {code})"
        )));
    }
    Ok(())
}

pub async fn remove(cfg: &AgentConfig, params: ComposeProjectActionParams) -> CoreResult<()> {
    if !validate::is_valid_project_slug(&params.project_slug) {
        return Err(CoreError::Validation(format!(
            "invalid project slug: {}",
            params.project_slug
        )));
    }
    // Timers of the project's scheduled jobs go first: they would otherwise
    // keep firing into a project that no longer exists.
    super::scheduled_jobs::remove_project_jobs(cfg, &params.project_slug).await?;
    let root = project_root(cfg, &params.project_slug);
    // A database service may be reachable from outside through a stream
    // proxy and firewall rule; close that first.
    db_exposure::clear(cfg, &root, &params.project_slug).await?;
    let static_dir = static_site_dir(cfg, &params.project_slug);
    if root.join(STATIC_MARKER).exists() || static_dir.exists() {
        // A static site: no containers, only vhosts and release files.
        let owned = read_owned_domains(&root).await;
        for domain in &owned {
            nginx::remove_compose_vhost(cfg, domain).await?;
        }
        if !owned.is_empty() {
            nginx::test_and_reload().await?;
        }
        for dir in [&static_dir, &root] {
            if dir.exists() {
                tokio::fs::remove_dir_all(dir)
                    .await
                    .map_err(|e| CoreError::Io(format!("removing project files: {e}")))?;
            }
        }
        return Ok(());
    }
    let paths = match existing_project_paths(cfg, &params.project_slug) {
        Ok(paths) => paths,
        // Never deployed: nothing runs, but the environment file or Git
        // credentials may exist, and a later project could reuse the slug.
        Err(CoreError::NotFound(_)) => {
            if root.exists() {
                tokio::fs::remove_dir_all(&root)
                    .await
                    .map_err(|e| CoreError::Io(format!("removing project files: {e}")))?;
            }
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    let mut args = compose_args(&params.project_slug, &paths);
    // Do not pass `--volumes`: persistent named volumes survive a project
    // removal unless an operator explicitly removes them outside the panel.
    args.extend(["down".into(), "--remove-orphans".into()]);
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let (code, stdout, stderr) = run_allow_nonzero("docker", &refs).await?;
    write_action_log(&root, &stdout, &stderr).await?;
    if code != 0 {
        return Err(CoreError::External(format!(
            "docker compose removal failed (exit code {code})"
        )));
    }
    let owned = read_owned_domains(&root).await;
    for domain in &owned {
        nginx::remove_compose_vhost(cfg, domain).await?;
    }
    if !owned.is_empty() {
        nginx::test_and_reload().await?;
    }
    // The containers are gone, so the release tags can go with them.
    releases::remove_all(&root, &params.project_slug).await;
    tokio::fs::remove_dir_all(&root)
        .await
        .map_err(|e| CoreError::Io(format!("removing project files: {e}")))?;
    Ok(())
}

pub async fn list_services_for(
    cfg: &AgentConfig,
    project_slug: &str,
) -> CoreResult<Vec<ComposeServiceInfo>> {
    if validate::is_valid_project_slug(project_slug) {
        if let Some(release) = static_release(cfg, project_slug) {
            return Ok(vec![static_service_info(&release)]);
        }
    }
    let paths = existing_project_paths(cfg, project_slug)?;
    let mut args = compose_args(project_slug, &paths);
    args.extend([
        "ps".into(),
        "--all".into(),
        "--format".into(),
        "json".into(),
    ]);
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let (code, stdout, stderr) = run_allow_nonzero("docker", &refs).await?;
    if code != 0 {
        return Err(CoreError::External(format!(
            "docker compose ps failed: {stderr}"
        )));
    }
    parse_ps_json(&stdout)
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "PascalCase")]
struct PsEntry {
    service: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    image: String,
    #[serde(default)]
    state: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    health: String,
    #[serde(default)]
    exit_code: i32,
    #[serde(default)]
    publishers: Option<Vec<PsPublisher>>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "PascalCase")]
struct PsPublisher {
    #[serde(rename = "URL", default)]
    url: String,
    #[serde(default)]
    target_port: u16,
    #[serde(default)]
    published_port: u16,
    #[serde(default)]
    protocol: String,
}

/// Compose v2.21+ prints one JSON object per line; older releases print a
/// single JSON array. Accept both.
fn parse_ps_json(output: &str) -> CoreResult<Vec<ComposeServiceInfo>> {
    let trimmed = output.trim();
    let entries: Vec<PsEntry> = if trimmed.is_empty() {
        Vec::new()
    } else if trimmed.starts_with('[') {
        serde_json::from_str(trimmed)
            .map_err(|e| CoreError::External(format!("parsing docker compose ps: {e}")))?
    } else {
        trimmed
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(serde_json::from_str)
            .collect::<Result<_, _>>()
            .map_err(|e| CoreError::External(format!("parsing docker compose ps: {e}")))?
    };
    Ok(entries
        .into_iter()
        .map(|entry| {
            let mut ports: Vec<String> = entry
                .publishers
                .unwrap_or_default()
                .into_iter()
                .filter(|p| p.target_port != 0)
                .map(|p| {
                    if p.published_port == 0 {
                        format!("{}/{}", p.target_port, p.protocol)
                    } else {
                        let host = if p.url.is_empty() { "0.0.0.0" } else { &p.url };
                        format!(
                            "{host}:{}->{}/{}",
                            p.published_port, p.target_port, p.protocol
                        )
                    }
                })
                .collect();
            ports.dedup();
            ComposeServiceInfo {
                name: entry.service,
                container: entry.name,
                image: entry.image,
                state: entry.state,
                status: entry.status,
                health: entry.health,
                exit_code: entry.exit_code,
                ports,
            }
        })
        .collect())
}

pub async fn logs(cfg: &AgentConfig, params: ComposeProjectLogsParams) -> CoreResult<String> {
    if validate::is_valid_project_slug(&params.project_slug)
        && static_release(cfg, &params.project_slug).is_some()
    {
        return Ok(
            "A static site runs no container, so it has no container logs; see the deployment log.
"
            .into(),
        );
    }
    let paths = existing_project_paths(cfg, &params.project_slug)?;
    let tail = params.tail_lines.clamp(1, 2_000).to_string();
    let mut args = compose_args(&params.project_slug, &paths);
    args.extend(["logs".into(), "--no-color".into(), "--tail".into(), tail]);
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let (code, stdout, stderr) = run_allow_nonzero("docker", &refs).await?;
    if code != 0 {
        return Err(CoreError::External(format!(
            "docker compose logs failed: {stderr}"
        )));
    }
    let secrets = environment_secrets(&project_root(cfg, &params.project_slug))
        .await
        .map(maskable_secrets)
        .unwrap_or_default();
    Ok(stdout
        .lines()
        .map(|line| redact(line, &secrets) + "\n")
        .collect())
}

/// Concurrent live log followers across all projects. Each holds a
/// `docker compose logs` process, so the count is bounded.
const MAX_LOG_FOLLOWERS: usize = 16;
/// A follower stops after this long; clients reconnect if they still care.
const FOLLOW_LIMIT: Duration = Duration::from_secs(60 * 60);
static LOG_FOLLOWERS: once_cell::sync::Lazy<Arc<tokio::sync::Semaphore>> =
    once_cell::sync::Lazy::new(|| Arc::new(tokio::sync::Semaphore::new(MAX_LOG_FOLLOWERS)));

/// Follows container output with `docker compose logs --follow`. Each line
/// is redacted with the project's environment values first and filtered by
/// `search` second, so searching cannot reveal a masked value.
pub async fn follow_logs(
    cfg: &AgentConfig,
    params: FollowComposeLogsParams,
) -> CoreResult<LogLines> {
    if validate::is_valid_project_slug(&params.project_slug)
        && static_release(cfg, &params.project_slug).is_some()
    {
        return Err(CoreError::Validation(
            "a static site runs no container, so there are no live logs".into(),
        ));
    }
    let paths = existing_project_paths(cfg, &params.project_slug)?;
    if let Some(service) = &params.service {
        if !validate::is_valid_service_name(service) {
            return Err(CoreError::Validation(format!(
                "invalid service name: {service}"
            )));
        }
    }
    let needle = match params.search.as_deref().map(str::trim) {
        Some(s) if s.len() > 200 || s.chars().any(char::is_control) => {
            return Err(CoreError::Validation("invalid log search".into()));
        }
        Some(s) if !s.is_empty() => Some(s.to_lowercase()),
        _ => None,
    };
    let permit = LOG_FOLLOWERS.clone().try_acquire_owned().map_err(|_| {
        CoreError::External("too many live log viewers are open; close one and retry".into())
    })?;
    let secrets = environment_secrets(&project_root(cfg, &params.project_slug))
        .await
        .map(maskable_secrets)
        .unwrap_or_default();

    let mut args = compose_args(&params.project_slug, &paths);
    args.extend([
        "logs".into(),
        "--follow".into(),
        "--no-color".into(),
        "--tail".into(),
        params.tail_lines.min(2_000).to_string(),
    ]);
    if params.timestamps {
        args.push("--timestamps".into());
    }
    if let Some(service) = params.service {
        args.extend(["--".into(), service]);
    }

    let (tx, rx) = tokio::sync::mpsc::channel(LOG_LINES_CAPACITY);
    tokio::spawn(async move {
        let _permit = permit;
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let filter = |line: &str| {
            let line = redact(line, &secrets);
            match &needle {
                Some(n) if !line.to_lowercase().contains(n) => None,
                _ => Some(line),
            }
        };
        if let Err(error) = follow_lines("docker", &refs, &tx, FOLLOW_LIMIT, filter).await {
            tx.send(Err(error)).await.ok();
        }
    });
    Ok(rx)
}

pub async fn read_deployment_log(
    cfg: &AgentConfig,
    params: DeploymentLogParams,
) -> CoreResult<DeploymentLogChunk> {
    if !validate::is_valid_project_slug(&params.project_slug) {
        return Err(CoreError::Validation(format!(
            "invalid project slug: {}",
            params.project_slug
        )));
    }
    let logs_dir = project_root(cfg, &params.project_slug).join("logs");
    deploy_log::read_chunk(
        &logs_dir,
        params.deployment_id,
        params.offset,
        params.max_bytes,
    )
    .await
}

fn validate_project_params(params: &DeployComposeProjectParams) -> CoreResult<()> {
    if !validate::is_valid_project_slug(&params.project_slug) {
        return Err(CoreError::Validation(format!(
            "invalid project slug: {}",
            params.project_slug
        )));
    }
    match &params.source {
        ComposeSource::Git {
            repository_url,
            branch,
            compose_path,
        } => {
            if !validate::is_valid_git_url(repository_url) {
                return Err(CoreError::Validation("invalid Git repository URL".into()));
            }
            if !validate::is_valid_git_branch(branch) {
                return Err(CoreError::Validation("invalid Git branch".into()));
            }
            if !validate::is_valid_compose_path(compose_path) {
                return Err(CoreError::Validation("invalid Compose path".into()));
            }
        }
        ComposeSource::Raw { compose_yaml } => {
            if compose_yaml.is_empty() || compose_yaml.len() > 512 * 1024 {
                return Err(CoreError::Validation(
                    "raw Compose YAML must be 1..=512KiB".into(),
                ));
            }
        }
        ComposeSource::Application(spec) => {
            spec.validate().map_err(CoreError::Validation)?;
        }
        ComposeSource::Static(spec) => {
            spec.validate().map_err(CoreError::Validation)?;
        }
        ComposeSource::Database(spec) => {
            spec.validate().map_err(CoreError::Validation)?;
            let database = params.database.as_ref().ok_or_else(|| {
                CoreError::Validation("a database service needs its port and credentials".into())
            })?;
            validate_database_deployment(spec, database)?;
            if !params.domains.is_empty() {
                return Err(CoreError::Validation(
                    "database services cannot have domains".into(),
                ));
            }
        }
    }
    if params.database.is_some() && !matches!(params.source, ComposeSource::Database(_)) {
        return Err(CoreError::Validation(
            "database credentials sent for a service that is not a database".into(),
        ));
    }
    if let Some(target) = &params.rollback {
        validate_rollback_target(params, target)?;
    }
    validate_bindings(&params.domains, params.acme_email.as_deref())
}

/// A rollback restarts pinned images of a Compose release. Static sites and
/// database services have none, and every tag must be one this project
/// recorded for the target deployment (checked again against the release).
fn validate_rollback_target(
    params: &DeployComposeProjectParams,
    target: &panel_core::types::RollbackTarget,
) -> CoreResult<()> {
    if matches!(
        params.source,
        ComposeSource::Static(_) | ComposeSource::Database(_)
    ) {
        return Err(CoreError::Validation(
            "static sites and database services cannot be rolled back".into(),
        ));
    }
    if target.deployment_id == params.deployment_id
        || target.compose_sha256.len() != 64
        || !target.compose_sha256.bytes().all(|b| b.is_ascii_hexdigit())
        || target.images.len() > 64
    {
        return Err(CoreError::Validation("invalid rollback target".into()));
    }
    for image in &target.images {
        let expected = panel_core::types::deployment_image_tag(
            &params.project_slug,
            &image.service,
            target.deployment_id,
        );
        if expected.as_deref() != Some(image.tag.as_str()) {
            return Err(CoreError::Validation(format!(
                "rollback image for service {} is not a tag of the target deployment",
                image.service
            )));
        }
    }
    Ok(())
}

fn validate_database_deployment(
    spec: &DatabaseServiceSpec,
    database: &DatabaseDeployment,
) -> CoreResult<()> {
    if !panel_core::database_services::DATABASE_LOOPBACK_PORTS.contains(&database.loopback_port) {
        return Err(CoreError::Validation(
            "invalid database loopback port".into(),
        ));
    }
    database
        .credentials
        .validate(spec.engine)
        .map_err(CoreError::Validation)
}

fn validate_environment(environment: &[(String, String)]) -> CoreResult<()> {
    if environment.len() > MAX_MERGED_ENV_VARS {
        return Err(CoreError::Validation(
            "too many environment variables".into(),
        ));
    }
    for (key, value) in environment {
        if !validate::is_valid_env_key(key)
            || value.contains('\0')
            || value.contains('\n')
            || value.len() > 16 * 1024
        {
            return Err(CoreError::Validation(format!(
                "invalid environment value for key: {key}"
            )));
        }
    }
    Ok(())
}

fn validate_bindings(
    bindings: &[ComposeDomainBinding],
    acme_email: Option<&str>,
) -> CoreResult<()> {
    if bindings.len() > MAX_DOMAIN_BINDINGS {
        return Err(CoreError::Validation(format!(
            "at most {MAX_DOMAIN_BINDINGS} domains per project"
        )));
    }
    let mut domains = HashSet::new();
    let mut host_ports = HashSet::new();
    for binding in bindings {
        if !validate::is_valid_domain(&binding.domain) {
            return Err(CoreError::Validation(format!(
                "invalid domain: {}",
                binding.domain
            )));
        }
        if !validate::is_valid_service_name(&binding.service_name) {
            return Err(CoreError::Validation(format!(
                "invalid service name: {}",
                binding.service_name
            )));
        }
        if binding.container_port == 0 || binding.host_port < 1024 {
            return Err(CoreError::Validation(format!(
                "invalid port binding for {}",
                binding.domain
            )));
        }
        if !domains.insert(binding.domain.as_str()) || !host_ports.insert(binding.host_port) {
            return Err(CoreError::Validation(format!(
                "duplicate domain or host port: {}",
                binding.domain
            )));
        }
    }
    if bindings.iter().any(|b| b.https) && !acme_email.is_some_and(is_plausible_email) {
        return Err(CoreError::Validation(
            "a valid ACME email is required for HTTPS domains".into(),
        ));
    }
    Ok(())
}

fn is_plausible_email(email: &str) -> bool {
    email.len() <= 254
        && !email.starts_with('-')
        && !email.chars().any(|c| c.is_whitespace() || c.is_control())
        && email
            .split_once('@')
            .is_some_and(|(local, domain)| !local.is_empty() && validate::is_valid_domain(domain))
}

fn check_bindings_against_compose(
    compose: &Yaml,
    bindings: &[ComposeDomainBinding],
) -> CoreResult<()> {
    let services = compose
        .get("services")
        .and_then(Yaml::as_mapping)
        .ok_or_else(|| CoreError::Validation("Compose file requires a services mapping".into()))?;
    for binding in bindings {
        if !services.contains_key(Yaml::String(binding.service_name.clone())) {
            return Err(CoreError::Validation(format!(
                "domain {} targets unknown service {}",
                binding.domain, binding.service_name
            )));
        }
    }
    Ok(())
}

/// Generates the override that publishes each bound container port on the
/// loopback interface only, where the generated Nginx vhost reaches it.
fn render_override(bindings: &[ComposeDomainBinding]) -> CoreResult<String> {
    let mut services = serde_yaml::Mapping::new();
    for binding in bindings {
        let entry = services
            .entry(Yaml::String(binding.service_name.clone()))
            .or_insert_with(|| {
                let mut service = serde_yaml::Mapping::new();
                service.insert("ports".into(), Yaml::Sequence(Vec::new()));
                Yaml::Mapping(service)
            });
        if let Some(Yaml::Sequence(ports)) = entry.get_mut("ports") {
            ports.push(Yaml::String(format!(
                "127.0.0.1:{}:{}",
                binding.host_port, binding.container_port
            )));
        }
    }
    let mut document = serde_yaml::Mapping::new();
    document.insert("services".into(), Yaml::Mapping(services));
    serde_yaml::to_string(&Yaml::Mapping(document))
        .map_err(|e| CoreError::External(format!("rendering Compose override: {e}")))
}

/// Writes vhosts for the desired bindings, removes vhosts of bindings that
/// were dropped, and reloads Nginx once. A configuration that fails
/// `nginx -t` is rolled back to the previous files. Certificates are then
/// requested for HTTPS bindings that lack one; a failed request is a warning,
/// not a failed deployment, and the domain keeps serving plain HTTP.
/// `static_site` is the release directory (and SPA flag) of a static site;
/// otherwise each binding proxies to its loopback port.
async fn apply_domains(
    cfg: &AgentConfig,
    project_slug: &str,
    root: &Path,
    bindings: &[ComposeDomainBinding],
    acme_email: Option<&str>,
    static_site: Option<(&Path, bool)>,
    log: &mut DeployLog,
) -> CoreResult<Vec<String>> {
    let target = |binding: &ComposeDomainBinding| match static_site {
        Some((root, spa)) => VhostTarget::Static { root, spa },
        None => VhostTarget::Proxy(binding.host_port),
    };
    let previous = read_owned_domains(root).await;
    if bindings.is_empty() && previous.is_empty() {
        return Ok(Vec::new());
    }
    log.step("configuring domains").await?;

    let mut backups: Vec<(String, Option<String>)> = Vec::new();
    for binding in bindings {
        backups.push((
            binding.domain.clone(),
            nginx::read_compose_vhost(cfg, &binding.domain).await,
        ));
        let tls = binding.https && nginx::certificate_exists(cfg, &binding.domain);
        let content =
            nginx::render_compose_vhost(cfg, project_slug, &binding.domain, &target(binding), tls);
        nginx::write_compose_vhost(cfg, &binding.domain, &content).await?;
    }
    for stale in previous
        .iter()
        .filter(|domain| !bindings.iter().any(|b| &b.domain == *domain))
    {
        backups.push((stale.clone(), nginx::read_compose_vhost(cfg, stale).await));
        nginx::remove_compose_vhost(cfg, stale).await?;
    }
    if let Err(error) = nginx::test_and_reload().await {
        for (domain, content) in backups {
            match content {
                Some(content) => nginx::write_compose_vhost(cfg, &domain, &content)
                    .await
                    .ok(),
                None => nginx::remove_compose_vhost(cfg, &domain).await.ok(),
            };
        }
        return Err(CoreError::External(format!(
            "generated Nginx configuration was rejected and rolled back: {error}"
        )));
    }
    write_owned_domains(root, bindings).await?;

    let mut warnings = Vec::new();
    let mut reload = false;
    for binding in bindings
        .iter()
        .filter(|b| b.https && !nginx::certificate_exists(cfg, &b.domain))
    {
        let Some(email) = acme_email else { continue };
        log.step(&format!("requesting certificate for {}", binding.domain))
            .await?;
        tokio::fs::create_dir_all(&cfg.acme_webroot)
            .await
            .map_err(|e| CoreError::Io(format!("creating ACME webroot: {e}")))?;
        let webroot = cfg.acme_webroot.to_string_lossy().into_owned();
        let args = certbot_webroot_args(&webroot, &binding.domain, email);
        let code = match run_streamed("certbot", &args, &[], log, CERTBOT_TIMEOUT).await {
            Ok(code) => code,
            Err(error @ CoreError::Cancelled(_)) => return Err(error),
            Err(_) => -1,
        };
        if code == 0 && nginx::certificate_exists(cfg, &binding.domain) {
            let content = nginx::render_compose_vhost(
                cfg,
                project_slug,
                &binding.domain,
                &target(binding),
                true,
            );
            nginx::write_compose_vhost(cfg, &binding.domain, &content).await?;
            reload = true;
        } else {
            warnings.push(format!(
                "certificate for {} could not be issued; serving HTTP only",
                binding.domain
            ));
        }
    }
    if reload {
        nginx::test_and_reload().await?;
    }
    Ok(warnings)
}

/// Deploys a static site: checkout, optional containerised build, a copy of
/// the publish directory into a fresh release, then the Nginx vhosts. The
/// previous release keeps serving until the new vhosts pass `nginx -t`.
async fn deploy_static(
    cfg: &AgentConfig,
    params: &DeployComposeProjectParams,
    spec: &StaticSiteSpec,
    root: &Path,
    log: &mut DeployLog,
) -> CoreResult<ComposeDeployOutcome> {
    let slug = &params.project_slug;
    let code_dir = root.join("code");
    log.step("preparing source").await?;
    let commit = checkout(
        root,
        &code_dir,
        &spec.repository_url,
        &spec.branch,
        params.git_credentials.as_ref(),
        log,
    )
    .await?;
    if let Some(build) = &spec.build {
        run_static_build(cfg, slug, root, &code_dir, build, log).await?;
    }

    let publish = code_dir.join(&spec.publish_path);
    ensure_inside_named(&code_dir, &publish, "publish directory").await?;
    let publish = tokio::fs::canonicalize(&publish)
        .await
        .map_err(|e| CoreError::Io(format!("resolving publish directory: {e}")))?;
    if !publish.is_dir() {
        return Err(CoreError::Validation(format!(
            "publish path {} is not a directory",
            spec.publish_path
        )));
    }

    let releases = static_site_dir(cfg, slug).join("releases");
    create_public_dirs(&cfg.static_sites_root, &releases).await?;
    let release = releases.join(params.deployment_id.to_string());
    log.step("publishing static files").await?;
    let stats = {
        let (from, to) = (publish.clone(), release.clone());
        tokio::task::spawn_blocking(move || publish_tree(&from, &to))
            .await
            .map_err(|e| CoreError::Io(format!("publishing static files: {e}")))?
    };
    let stats = match stats {
        Ok(stats) => stats,
        Err(error) => {
            tokio::fs::remove_dir_all(&release).await.ok();
            return Err(error);
        }
    };
    log.step(&format!(
        "published {} files ({} bytes) to release {}",
        stats.files, stats.bytes, params.deployment_id
    ))
    .await?;
    if !stats.skipped.is_empty() {
        log.step(&format!(
            "skipped {} symlinks or special files (only regular files are published): {}",
            stats.skipped_total,
            stats.skipped.join(", ")
        ))
        .await?;
    }

    let mut warnings = match apply_domains(
        cfg,
        slug,
        root,
        &params.domains,
        params.acme_email.as_deref(),
        Some((&release, spec.spa)),
        log,
    )
    .await
    {
        Ok(warnings) => warnings,
        Err(error) => {
            tokio::fs::remove_dir_all(&release).await.ok();
            return Err(error);
        }
    };
    if params.domains.is_empty() {
        warnings.push("the site has no domain yet; add one to serve it".into());
    }
    tokio::fs::write(root.join(STATIC_MARKER), params.deployment_id.to_string())
        .await
        .map_err(|e| CoreError::Io(format!("recording static release: {e}")))?;
    prune_static_releases(&releases, &params.deployment_id.to_string()).await;
    Ok(ComposeDeployOutcome {
        services: vec![static_service_info(&params.deployment_id.to_string())],
        commit_sha: commit,
        warnings,
        snapshot: None,
        retained_releases: Vec::new(),
    })
}

/// Runs each build command in a throwaway container with the checkout at
/// `/workspace`. Exec form only, all capabilities dropped, no new
/// privileges; the project environment file is passed by path, so build
/// tools (e.g. Vite) can read public settings without values on argv.
async fn run_static_build(
    cfg: &AgentConfig,
    slug: &str,
    root: &Path,
    code_dir: &Path,
    build: &panel_core::types::StaticBuild,
    log: &mut DeployLog,
) -> CoreResult<()> {
    let name = format!("scontrol-static-build-{slug}");
    // A container left behind by a killed deployment would block the name.
    let _ = run_allow_nonzero("docker", &["rm", "-f", &name]).await;
    let timeout = Duration::from_secs(cfg.deploy_timeout_secs.max(60));
    let (code, env) = (
        code_dir.to_string_lossy().into_owned(),
        root.join(ENV_FILE).to_string_lossy().into_owned(),
    );
    let total = build.commands.len();
    for (index, command) in build.commands.iter().enumerate() {
        log.step(&format!(
            "build step {}/{total} in {}: {}",
            index + 1,
            build.image,
            command.join(" ")
        ))
        .await?;
        let args = static_build_args(&name, &code, &env, &build.image, command);
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let result = run_streamed("docker", &refs, &[], log, timeout).await;
        let failure = match result {
            Ok(0) => continue,
            Ok(exit) => CoreError::External(format!(
                "static build step {} failed (exit code {exit}); see the deployment log",
                index + 1
            )),
            Err(error) => error,
        };
        // Cancellation stops the `docker run` client; make sure the
        // container itself goes too.
        let _ = run_allow_nonzero("docker", &["rm", "-f", &name]).await;
        return Err(failure);
    }
    Ok(())
}

fn static_build_args(
    name: &str,
    code_dir: &str,
    env_file: &str,
    image: &str,
    command: &[String],
) -> Vec<String> {
    let mut args: Vec<String> = [
        "run",
        "--rm",
        "--name",
        name,
        "--pull",
        "missing",
        "--cap-drop",
        "ALL",
        "--security-opt",
        "no-new-privileges",
        "--env-file",
        env_file,
        "--mount",
    ]
    .map(String::from)
    .into();
    // The source path is agent-built (configured root + validated slug).
    args.push(format!("type=bind,source={code_dir},target=/workspace"));
    // `image` is a validated reference, which never starts with `-`; the
    // command follows it, so Docker passes it to the container untouched.
    args.extend(["--workdir".into(), "/workspace".into(), image.into()]);
    args.extend(command.iter().cloned());
    args
}

/// Static releases are read by the Nginx worker, so unlike the private
/// project directory they are world-readable.
fn static_site_dir(cfg: &AgentConfig, slug: &str) -> PathBuf {
    cfg.static_sites_root.join(slug)
}

async fn create_public_dirs(base: &Path, dir: &Path) -> CoreResult<()> {
    tokio::fs::create_dir_all(dir)
        .await
        .map_err(|e| CoreError::Io(format!("creating release directory: {e}")))?;
    let mut current = dir.to_path_buf();
    loop {
        tokio::fs::set_permissions(
            &current,
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .await
        .map_err(|e| CoreError::Io(format!("setting release directory mode: {e}")))?;
        if current == base || !current.pop() || !current.starts_with(base) {
            return Ok(());
        }
    }
}

/// Files and bytes a single static release may hold.
const MAX_PUBLISH_FILES: u64 = 100_000;
const MAX_PUBLISH_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MAX_PUBLISH_DEPTH: usize = 64;

#[derive(Debug, Default)]
struct PublishStats {
    files: u64,
    bytes: u64,
    skipped_total: u64,
    /// First few skipped paths, for the deployment log.
    skipped: Vec<String>,
}

/// Copies regular files and directories from `from` into the new directory
/// `to`. Symlinks, devices, sockets, and FIFOs are skipped, never followed,
/// so a link planted in the repository or by the build cannot publish a
/// host file. `.git` directories are skipped so repository history never
/// becomes public. Files get mode 0644, directories 0755.
fn publish_tree(from: &Path, to: &Path) -> CoreResult<PublishStats> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

    let io = |what: &str, e: std::io::Error| CoreError::Io(format!("{what}: {e}"));
    std::fs::create_dir(to).map_err(|e| io("creating release", e))?;
    std::fs::set_permissions(to, std::fs::Permissions::from_mode(0o755))
        .map_err(|e| io("setting release mode", e))?;
    let mut stats = PublishStats::default();
    let mut pending = vec![(from.to_path_buf(), to.to_path_buf(), String::new(), 0usize)];
    while let Some((src_dir, dst_dir, prefix, depth)) = pending.pop() {
        if depth > MAX_PUBLISH_DEPTH {
            return Err(CoreError::Validation(
                "publish directory is nested too deeply".into(),
            ));
        }
        let entries =
            std::fs::read_dir(&src_dir).map_err(|e| io("reading publish directory", e))?;
        for entry in entries {
            let entry = entry.map_err(|e| io("reading publish directory", e))?;
            let name = entry.file_name();
            let relative = format!("{prefix}{}", name.to_string_lossy());
            if name == ".git" {
                continue;
            }
            let (src, dst) = (entry.path(), dst_dir.join(&name));
            // lstat: a symlink is reported as a symlink, not its target.
            let meta = std::fs::symlink_metadata(&src).map_err(|e| io("inspecting file", e))?;
            let kind = meta.file_type();
            if kind.is_dir() {
                std::fs::create_dir(&dst).map_err(|e| io("creating release directory", e))?;
                std::fs::set_permissions(&dst, std::fs::Permissions::from_mode(0o755))
                    .map_err(|e| io("setting release directory mode", e))?;
                pending.push((src, dst, format!("{relative}/"), depth + 1));
            } else if kind.is_file() {
                stats.files += 1;
                stats.bytes += meta.len();
                if stats.files > MAX_PUBLISH_FILES || stats.bytes > MAX_PUBLISH_BYTES {
                    return Err(CoreError::Validation(format!(
                        "publish directory exceeds {MAX_PUBLISH_FILES} files or {} GiB",
                        MAX_PUBLISH_BYTES >> 30
                    )));
                }
                let mut input = std::fs::File::open(&src).map_err(|e| io("reading file", e))?;
                // The opened file must be the regular file inspected above,
                // not something swapped in for it since.
                let opened = input.metadata().map_err(|e| io("reading file", e))?;
                if opened.dev() != meta.dev() || opened.ino() != meta.ino() || !opened.is_file() {
                    return Err(CoreError::Validation(format!(
                        "{relative} changed while publishing"
                    )));
                }
                let mut output = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o644)
                    .open(&dst)
                    .map_err(|e| io("writing release file", e))?;
                std::io::copy(&mut input, &mut output).map_err(|e| io("copying file", e))?;
                std::fs::set_permissions(&dst, std::fs::Permissions::from_mode(0o644))
                    .map_err(|e| io("setting release file mode", e))?;
            } else {
                stats.skipped_total += 1;
                if stats.skipped.len() < 10 {
                    stats.skipped.push(relative);
                }
            }
        }
    }
    Ok(stats)
}

/// Keeps the served release and the newest others, up to
/// [`STATIC_RELEASES_RETAINED`]. Only UUID-named directories are touched.
async fn prune_static_releases(releases: &Path, current: &str) {
    let Ok(mut entries) = tokio::fs::read_dir(releases).await else {
        return;
    };
    let mut others = Vec::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == current || uuid::Uuid::parse_str(&name).is_err() {
            continue;
        }
        if let Ok(meta) = tokio::fs::symlink_metadata(entry.path()).await {
            if meta.is_dir() {
                others.push((meta.modified().ok(), entry.path()));
            }
        }
    }
    others.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
    for (_, path) in others.into_iter().skip(STATIC_RELEASES_RETAINED - 1) {
        tokio::fs::remove_dir_all(path).await.ok();
    }
}

/// The release a static site serves, if it was deployed.
/// Whether the project is a static site (no containers).
pub(crate) fn is_static_project(cfg: &AgentConfig, slug: &str) -> bool {
    project_root(cfg, slug).join(STATIC_MARKER).exists() || static_site_dir(cfg, slug).exists()
}

fn static_release(cfg: &AgentConfig, slug: &str) -> Option<String> {
    std::fs::read_to_string(project_root(cfg, slug).join(STATIC_MARKER))
        .ok()
        .map(|id| id.trim().to_string())
        .filter(|id| uuid::Uuid::parse_str(id).is_ok())
}

fn static_service_info(release: &str) -> ComposeServiceInfo {
    ComposeServiceInfo {
        name: panel_core::types::STATIC_SERVICE_NAME.into(),
        container: String::new(),
        image: "nginx (static files)".into(),
        state: "running".into(),
        status: format!("serving release {release}"),
        health: String::new(),
        exit_code: 0,
        ports: Vec::new(),
    }
}

async fn read_owned_domains(root: &Path) -> Vec<String> {
    let Ok(raw) = tokio::fs::read_to_string(root.join(DOMAINS_FILE)).await else {
        return Vec::new();
    };
    serde_json::from_str::<Vec<String>>(&raw)
        .unwrap_or_default()
        .into_iter()
        // The file is agent-written, but it names files we delete: re-check.
        .filter(|domain| validate::is_valid_domain(domain))
        .collect()
}

async fn write_owned_domains(root: &Path, bindings: &[ComposeDomainBinding]) -> CoreResult<()> {
    let domains: Vec<&str> = bindings.iter().map(|b| b.domain.as_str()).collect();
    let body = serde_json::to_string(&domains)
        .map_err(|e| CoreError::External(format!("encoding domain list: {e}")))?;
    tokio::fs::write(root.join(DOMAINS_FILE), body)
        .await
        .map_err(|e| CoreError::Io(format!("writing domain list: {e}")))
}

/// Fetches or writes the project source and returns the Compose file path
/// plus, for Git, the deployed commit. An existing checkout of the same
/// remote is updated in place with a shallow fetch instead of a fresh clone.
async fn materialize_source(
    cfg: &AgentConfig,
    params: &DeployComposeProjectParams,
    root: &Path,
    code_dir: &Path,
    log: &mut DeployLog,
) -> CoreResult<(PathBuf, Option<String>)> {
    let credentials = params.git_credentials.as_ref();
    match &params.source {
        ComposeSource::Raw { compose_yaml } => {
            reset_dir(code_dir).await?;
            let path = code_dir.join(DEFAULT_COMPOSE_FILE);
            tokio::fs::write(&path, compose_yaml)
                .await
                .map_err(|e| CoreError::Io(format!("writing Compose YAML: {e}")))?;
            Ok((path, None))
        }
        ComposeSource::Git {
            repository_url,
            branch,
            compose_path,
        } => {
            let commit = checkout(root, code_dir, repository_url, branch, credentials, log).await?;
            Ok((code_dir.join(compose_path), commit))
        }
        ComposeSource::Application(spec) => match &spec.build {
            ApplicationBuild::Image { .. } => {
                reset_dir(code_dir).await?;
                let path = code_dir.join(DEFAULT_COMPOSE_FILE);
                let body = render_application(spec, root, &params.project_slug)?;
                write_generated_file(&path, &body).await?;
                Ok((path, None))
            }
            ApplicationBuild::Dockerfile {
                repository_url,
                branch,
                context_path,
                dockerfile_path,
                ..
            } => {
                let commit =
                    checkout(root, code_dir, repository_url, branch, credentials, log).await?;
                let context = code_dir.join(context_path);
                ensure_inside_named(code_dir, &context, "build context").await?;
                ensure_inside_named(code_dir, &context.join(dockerfile_path), "Dockerfile").await?;
                let path = code_dir.join(APPLICATION_COMPOSE_FILE);
                let body = render_application(spec, root, &params.project_slug)?;
                write_generated_file(&path, &body).await?;
                Ok((path, commit))
            }
            ApplicationBuild::Builder {
                builder,
                repository_url,
                branch,
                context_path,
            } => {
                let commit =
                    checkout(root, code_dir, repository_url, branch, credentials, log).await?;
                let context = code_dir.join(context_path);
                ensure_inside_named(code_dir, &context, "build context").await?;
                // Builders follow the path they are given, so hand them the
                // resolved directory rather than one that crosses a link.
                let context = tokio::fs::canonicalize(&context)
                    .await
                    .map_err(|e| CoreError::Io(format!("resolving build context: {e}")))?;
                build_with(cfg, *builder, &context, root, &params.project_slug, log).await?;
                let path = code_dir.join(APPLICATION_COMPOSE_FILE);
                let body = render_application(spec, root, &params.project_slug)?;
                write_generated_file(&path, &body).await?;
                Ok((path, commit))
            }
        },
        ComposeSource::Static(_) => Err(CoreError::Validation(
            "static sites are not deployed through Compose".into(),
        )),
        ComposeSource::Database(spec) => {
            let database = params.database.as_ref().ok_or_else(|| {
                CoreError::Validation("a database service needs its port and credentials".into())
            })?;
            write_database_files(spec, database, root).await?;
            reset_dir(code_dir).await?;
            let path = code_dir.join(DEFAULT_COMPOSE_FILE);
            let body = render_database(spec, database.loopback_port, root)?;
            write_generated_file(&path, &body).await?;
            Ok((path, None))
        }
    }
}

#[cfg(feature = "builders")]
async fn build_with(
    cfg: &AgentConfig,
    builder: panel_core::types::BuildTool,
    context: &Path,
    root: &Path,
    slug: &str,
    log: &mut DeployLog,
) -> CoreResult<()> {
    let image = panel_core::types::built_image_tag(slug);
    super::builders::build_image(cfg, builder, context, &root.join("build"), &image, log).await
}

#[cfg(not(feature = "builders"))]
async fn build_with(
    _cfg: &AgentConfig,
    builder: panel_core::types::BuildTool,
    _context: &Path,
    _root: &Path,
    _slug: &str,
    _log: &mut DeployLog,
) -> CoreResult<()> {
    Err(CoreError::PermissionDenied(format!(
        "this agent was built without application builders, so {} is unavailable; \
         re-run the installer and enable them",
        builder.as_str()
    )))
}

/// Clones `repository_url` into `code_dir`, or updates an existing checkout
/// of the same remote in place. Returns the checked-out commit.
async fn checkout(
    root: &Path,
    code_dir: &Path,
    repository_url: &str,
    branch: &str,
    credentials: Option<&panel_core::adapters::GitHttpCredentials>,
    log: &mut DeployLog,
) -> CoreResult<Option<String>> {
    let auth = git_auth::for_remote(root, repository_url, credentials).await?;
    log.step(&format!("Git access: {}", auth.description))
        .await?;
    let env = auth.env();
    let dir = code_dir.to_string_lossy().into_owned();
    let reusable = code_dir.join(".git").is_dir()
        && run("git", &["-C", &dir, "remote", "get-url", "origin"])
            .await
            .is_ok_and(|url| url.trim() == repository_url);
    let fetched = reusable && update_checkout(&dir, branch, &env, log).await?;
    if !fetched {
        reset_dir(code_dir).await?;
        tokio::fs::remove_dir(code_dir).await.ok();
        log.step(&format!("cloning {repository_url} ({branch})"))
            .await?;
        let args = git_clone_args(branch, repository_url, &dir);
        let code = run_streamed("git", &args, &env, log, GIT_TIMEOUT).await?;
        if code != 0 {
            return Err(CoreError::External(format!(
                "Git clone failed (exit code {code}); see the deployment log"
            )));
        }
    }
    let commit = run("git", &["-C", &dir, "rev-parse", "HEAD"])
        .await
        .ok()
        .map(|sha| sha.trim().to_string())
        .filter(|sha| sha.len() == 40 && sha.chars().all(|c| c.is_ascii_hexdigit()));
    if let Some(sha) = &commit {
        log.step(&format!("source at commit {sha}")).await?;
    }
    Ok(commit)
}

/// Writes a panel-generated file inside a checkout. Whatever the repository
/// put at that path (a symlink to a host file, a directory) is removed first
/// and the file is created fresh, so the write can never follow a link.
pub(crate) async fn write_generated_file(path: &Path, body: &str) -> CoreResult<()> {
    match tokio::fs::symlink_metadata(path).await {
        Ok(meta) if meta.is_dir() => {
            return Err(CoreError::Validation(format!(
                "source tree has a directory at the generated path {}",
                path.file_name().unwrap_or_default().to_string_lossy()
            )))
        }
        Ok(_) => tokio::fs::remove_file(path)
            .await
            .map_err(|e| CoreError::Io(format!("replacing generated Compose file: {e}")))?,
        Err(_) => {}
    }
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .await
        .map_err(|e| CoreError::Io(format!("writing generated Compose file: {e}")))?;
    tokio::io::AsyncWriteExt::write_all(&mut file, body.as_bytes())
        .await
        .map_err(|e| CoreError::Io(format!("writing generated Compose file: {e}")))
}

/// Renders a single-service application as Compose YAML. Values are placed
/// through `serde_yaml`, never string-interpolated, so no input can add keys.
/// The project environment file becomes the container's `env_file`.
fn render_application(spec: &ApplicationSpec, root: &Path, slug: &str) -> CoreResult<String> {
    use serde_yaml::{Mapping, Value};
    fn key(k: &str) -> Value {
        Value::String(k.into())
    }
    let mut service = Mapping::new();
    match &spec.build {
        ApplicationBuild::Image { image } => {
            service.insert(key("image"), Value::String(image.clone()));
        }
        ApplicationBuild::Dockerfile {
            context_path,
            dockerfile_path,
            target,
            ..
        } => {
            let mut build = Mapping::new();
            build.insert(key("context"), Value::String(format!("./{context_path}")));
            build.insert(key("dockerfile"), Value::String(dockerfile_path.clone()));
            if let Some(target) = target {
                build.insert(key("target"), Value::String(target.clone()));
            }
            service.insert(key("build"), Value::Mapping(build));
        }
        ApplicationBuild::Builder { .. } => {
            // Built by `builders` into the local image store before `up`.
            service.insert(
                key("image"),
                Value::String(panel_core::types::built_image_tag(slug)),
            );
            service.insert(key("pull_policy"), key("never"));
        }
    }
    service.insert(
        key("restart"),
        Value::String(spec.restart.as_compose().into()),
    );
    service.insert(
        key("env_file"),
        Value::Sequence(vec![Value::String(
            root.join(ENV_FILE).to_string_lossy().into_owned(),
        )]),
    );
    if let Some(port) = spec.container_port {
        service.insert(
            key("expose"),
            Value::Sequence(vec![Value::String(port.to_string())]),
        );
    }
    if let Some(mb) = spec.memory_limit_mb {
        service.insert(key("mem_limit"), Value::String(format!("{mb}m")));
    }
    if let Some(millis) = spec.cpu_limit_millis {
        service.insert(
            key("cpus"),
            Value::String(format!("{}.{:03}", millis / 1000, millis % 1000)),
        );
    }
    if let Some(check) = &spec.health_check {
        let mut test = vec![Value::String("CMD".into())];
        test.extend(check.command.iter().cloned().map(Value::String));
        let mut health = Mapping::new();
        health.insert(key("test"), Value::Sequence(test));
        health.insert(
            key("interval"),
            Value::String(format!("{}s", check.interval_secs)),
        );
        health.insert(
            key("timeout"),
            Value::String(format!("{}s", check.timeout_secs)),
        );
        health.insert(key("retries"), Value::Number(check.retries.into()));
        health.insert(
            key("start_period"),
            Value::String(format!("{}s", check.start_period_secs)),
        );
        service.insert(key("healthcheck"), Value::Mapping(health));
    }
    let mut volumes = Mapping::new();
    if !spec.volumes.is_empty() {
        let mounts = spec
            .volumes
            .iter()
            .map(|v| {
                let mut mount = Mapping::new();
                mount.insert(key("type"), key("volume"));
                mount.insert(key("source"), Value::String(v.name.clone()));
                mount.insert(key("target"), Value::String(v.target.clone()));
                Value::Mapping(mount)
            })
            .collect();
        service.insert(key("volumes"), Value::Sequence(mounts));
        for v in &spec.volumes {
            volumes.insert(
                Value::String(v.name.clone()),
                Value::Mapping(Mapping::new()),
            );
        }
    }
    let mut services = Mapping::new();
    services.insert(
        Value::String(spec.service_name.clone()),
        Value::Mapping(service),
    );
    let mut doc = Mapping::new();
    doc.insert(key("services"), Value::Mapping(services));
    if !volumes.is_empty() {
        doc.insert(key("volumes"), Value::Mapping(volumes));
    }
    let body = serde_yaml::to_string(&Value::Mapping(doc))
        .map_err(|e| CoreError::External(format!("rendering application Compose: {e}")))?;
    Ok(format!(
        "# Generated by scontrol from the application settings; do not edit.\n{body}"
    ))
}

/// Writes the generated credentials: `db.env` (0600) for every engine and,
/// for Redis, the 0400 config the server reads after dropping to its user.
async fn write_database_files(
    spec: &DatabaseServiceSpec,
    database: &DatabaseDeployment,
    root: &Path,
) -> CoreResult<()> {
    write_environment(
        &root.join(DATABASE_ENV_FILE),
        &spec.container_environment(&database.credentials),
    )
    .await?;
    let redis_config = root.join(REDIS_CONFIG_FILE);
    if spec.engine == panel_core::types::ManagedDatabaseEngine::Redis {
        // Recreate rather than rewrite: the file belongs to the container
        // user and is read-only.
        tokio::fs::remove_file(&redis_config).await.ok();
        write_private_file(
            &redis_config,
            &DatabaseServiceSpec::redis_config(&database.credentials),
        )
        .await?;
        #[cfg(unix)]
        {
            std::os::unix::fs::chown(
                &redis_config,
                Some(REDIS_CONTAINER_UID),
                Some(REDIS_CONTAINER_UID),
            )
            .map_err(|e| CoreError::Io(format!("handing the Redis config to its user: {e}")))?;
            tokio::fs::set_permissions(
                &redis_config,
                std::os::unix::fs::PermissionsExt::from_mode(0o400),
            )
            .await
            .map_err(|e| CoreError::Io(format!("protecting the Redis config: {e}")))?;
        }
    } else {
        tokio::fs::remove_file(&redis_config).await.ok();
    }
    Ok(())
}

/// Renders a database service from its engine template through
/// `serde_yaml` values. The port is published on loopback only, data is a
/// named volume, credentials come from the agent's own env files, and the
/// health check is exec form without a password.
fn render_database(
    spec: &DatabaseServiceSpec,
    loopback_port: u16,
    root: &Path,
) -> CoreResult<String> {
    use serde_yaml::{Mapping, Value};
    fn key(k: &str) -> Value {
        Value::String(k.into())
    }
    fn strings<'a>(items: impl IntoIterator<Item = &'a str>) -> Value {
        Value::Sequence(items.into_iter().map(|s| Value::String(s.into())).collect())
    }
    let engine = spec.engine;
    let mut service = Mapping::new();
    service.insert(key("image"), Value::String(engine.image(&spec.version)));
    service.insert(key("restart"), key("unless-stopped"));
    let env = root.join(ENV_FILE).to_string_lossy().into_owned();
    let db_env = root.join(DATABASE_ENV_FILE).to_string_lossy().into_owned();
    // Later files win, so user variables cannot replace the credentials.
    service.insert(key("env_file"), strings([env.as_str(), db_env.as_str()]));
    service.insert(
        key("ports"),
        strings([format!("127.0.0.1:{loopback_port}:{}", engine.container_port()).as_str()]),
    );
    let mut mount = Mapping::new();
    mount.insert(key("type"), key("volume"));
    mount.insert(
        key("source"),
        key(panel_core::database_services::DATABASE_VOLUME_NAME),
    );
    mount.insert(key("target"), key(engine.data_path(&spec.version)));
    service.insert(key("volumes"), Value::Sequence(vec![Value::Mapping(mount)]));
    service.insert(key("security_opt"), strings(["no-new-privileges:true"]));
    if let Some(mb) = spec.memory_limit_mb {
        service.insert(key("mem_limit"), Value::String(format!("{mb}m")));
    }
    if let Some(millis) = spec.cpu_limit_millis {
        service.insert(
            key("cpus"),
            Value::String(format!("{}.{:03}", millis / 1000, millis % 1000)),
        );
    }
    let mut test = vec![key("CMD")];
    test.extend(spec.health_check().into_iter().map(Value::String));
    let mut health = Mapping::new();
    health.insert(key("test"), Value::Sequence(test));
    health.insert(key("interval"), key("10s"));
    health.insert(key("timeout"), key("5s"));
    health.insert(key("retries"), Value::Number(10.into()));
    health.insert(key("start_period"), key("30s"));
    service.insert(key("healthcheck"), Value::Mapping(health));

    let mut doc = Mapping::new();
    if engine == panel_core::types::ManagedDatabaseEngine::Redis {
        let target = format!("/run/secrets/{REDIS_SECRET_NAME}");
        service.insert(key("command"), strings(["redis-server", target.as_str()]));
        service.insert(key("secrets"), strings([REDIS_SECRET_NAME]));
        let mut secret = Mapping::new();
        secret.insert(
            key("file"),
            Value::String(root.join(REDIS_CONFIG_FILE).to_string_lossy().into_owned()),
        );
        let mut secrets = Mapping::new();
        secrets.insert(key(REDIS_SECRET_NAME), Value::Mapping(secret));
        doc.insert(key("secrets"), Value::Mapping(secrets));
    }
    let mut services = Mapping::new();
    services.insert(key(DATABASE_SERVICE_NAME), Value::Mapping(service));
    doc.insert(key("services"), Value::Mapping(services));
    let mut volumes = Mapping::new();
    volumes.insert(
        key(panel_core::database_services::DATABASE_VOLUME_NAME),
        Value::Mapping(Mapping::new()),
    );
    doc.insert(key("volumes"), Value::Mapping(volumes));
    let body = serde_yaml::to_string(&Value::Mapping(doc))
        .map_err(|e| CoreError::External(format!("rendering database Compose: {e}")))?;
    Ok(format!(
        "# Generated by scontrol from the database settings; do not edit.\n{body}"
    ))
}

/// Shallow-fetches `branch` into an existing checkout and resets to it.
/// Returns `false` (caller re-clones) when the fetch fails.
async fn update_checkout(
    dir: &str,
    branch: &str,
    env: &[(&str, &str)],
    log: &mut DeployLog,
) -> CoreResult<bool> {
    log.step(&format!("fetching {branch}")).await?;
    let fetch = git_fetch_args(dir, branch);
    if run_streamed("git", &fetch, env, log, GIT_TIMEOUT).await? != 0 {
        log.step("fetch failed; re-cloning").await?;
        return Ok(false);
    }
    let reset = ["-C", dir, "reset", "--hard", "FETCH_HEAD"];
    let clean = ["-C", dir, "clean", "-ffdx"];
    Ok(
        run_streamed("git", &reset, env, log, GIT_TIMEOUT).await? == 0
            && run_streamed("git", &clean, env, log, GIT_TIMEOUT).await? == 0,
    )
}

async fn reset_dir(dir: &Path) -> CoreResult<()> {
    if dir.exists() {
        tokio::fs::remove_dir_all(dir)
            .await
            .map_err(|e| CoreError::Io(format!("clearing previous source: {e}")))?;
    }
    tokio::fs::create_dir_all(dir)
        .await
        .map_err(|e| CoreError::Io(format!("creating source directory: {e}")))
}

pub(crate) async fn create_private_dir(dir: &Path) -> CoreResult<()> {
    tokio::fs::create_dir_all(dir)
        .await
        .map_err(|e| CoreError::Io(format!("creating project root: {e}")))?;
    #[cfg(unix)]
    tokio::fs::set_permissions(dir, std::os::unix::fs::PermissionsExt::from_mode(0o700))
        .await
        .map_err(|e| CoreError::Io(format!("protecting project root: {e}")))?;
    Ok(())
}

/// Rejects a Compose path that resolves (e.g. through a symlink committed to
/// the repository) outside the project's code directory.
async fn ensure_inside(code_dir: &Path, compose_path: &Path) -> CoreResult<()> {
    ensure_inside_named(code_dir, compose_path, "Compose file").await
}

async fn ensure_inside_named(code_dir: &Path, path: &Path, what: &str) -> CoreResult<()> {
    let base = tokio::fs::canonicalize(code_dir)
        .await
        .map_err(|e| CoreError::Io(format!("resolving source directory: {e}")))?;
    let resolved = tokio::fs::canonicalize(path)
        .await
        .map_err(|_| CoreError::Validation(format!("{what} not found in source")))?;
    if !resolved.starts_with(&base) {
        return Err(CoreError::Validation(format!(
            "{what} escapes the project source directory"
        )));
    }
    Ok(())
}

/// Projects deployed before the environment moved out of the source tree
/// kept it at `code/.env`; adopt it once so a redeploy keeps the values.
async fn adopt_legacy_env_file(root: &Path, env_path: &Path) {
    let legacy = root.join("code").join(".env");
    if !env_path.exists() && legacy.is_file() {
        tokio::fs::rename(&legacy, env_path).await.ok();
    }
}

async fn write_environment(path: &Path, environment: &[(String, String)]) -> CoreResult<()> {
    let mut body = String::new();
    for (key, value) in environment {
        // Newlines are rejected in validation, so one key/value occupies one
        // line. The Compose `.env` parser receives literal values as supplied.
        body.push_str(key);
        body.push('=');
        body.push_str(value);
        body.push('\n');
    }
    write_private_file(path, &body).await
}

/// Replaces `path` with `body`, readable by root only (0600).
pub(crate) async fn write_private_file(path: &Path, body: &str) -> CoreResult<()> {
    let mut options = tokio::fs::OpenOptions::new();
    options.create(true).write(true).truncate(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options
        .open(path)
        .await
        .map_err(|e| CoreError::Io(format!("writing environment file: {e}")))?;
    tokio::io::AsyncWriteExt::write_all(&mut file, body.as_bytes())
        .await
        .map_err(|e| CoreError::Io(format!("writing environment file: {e}")))?;
    #[cfg(unix)]
    tokio::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o600))
        .await
        .map_err(|e| CoreError::Io(format!("protecting environment file: {e}")))?;
    Ok(())
}

/// Reads the existing environment file (only to mask its values in logs).
/// A missing file becomes an empty one so `--env-file` always resolves.
pub(crate) async fn read_environment(path: &Path) -> CoreResult<Vec<(String, String)>> {
    match tokio::fs::read_to_string(path).await {
        Ok(body) => Ok(body
            .lines()
            .filter_map(|line| line.split_once('='))
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            write_environment(path, &[]).await?;
            Ok(Vec::new())
        }
        Err(e) => Err(CoreError::Io(format!("reading environment file: {e}"))),
    }
}

/// Parses and validates the Compose file, then checks that every host path
/// it names (relative bind mounts, `env_file`, build contexts, secret and
/// config files) resolves inside the source tree, so a symlink committed to
/// the repository cannot point Docker at a host file. `allowed_env_file` is
/// the project's own environment file, which generated application files
/// reference by absolute path.
async fn validate_compose_file(
    path: &Path,
    code_dir: &Path,
    agent_files: &[PathBuf],
) -> CoreResult<Yaml> {
    let body = tokio::fs::read_to_string(path)
        .await
        .map_err(|e| CoreError::Validation(format!("cannot read Compose file: {e}")))?;
    let yaml: Yaml = serde_yaml::from_str(&body)
        .map_err(|e| CoreError::Validation(format!("invalid Compose YAML: {e}")))?;
    let services = yaml
        .get("services")
        .and_then(Yaml::as_mapping)
        .ok_or_else(|| CoreError::Validation("Compose file requires a services mapping".into()))?;
    if services.is_empty() || services.len() > 64 {
        return Err(CoreError::Validation(
            "Compose must define 1..=64 services".into(),
        ));
    }
    let host_paths = compose_policy(&yaml, agent_files)?;
    for relative in host_paths {
        ensure_resolves_inside(code_dir, &relative).await?;
    }
    Ok(yaml)
}

/// Checks a relative host path the way Docker will use it: the deepest
/// existing ancestor (Docker creates missing bind directories) must
/// canonicalize inside the source tree.
async fn ensure_resolves_inside(code_dir: &Path, relative: &str) -> CoreResult<()> {
    let base = tokio::fs::canonicalize(code_dir)
        .await
        .map_err(|e| CoreError::Io(format!("resolving source directory: {e}")))?;
    let mut candidate = base.join(relative);
    loop {
        match tokio::fs::canonicalize(&candidate).await {
            Ok(resolved) if resolved.starts_with(&base) => return Ok(()),
            Ok(_) => {
                return Err(CoreError::Validation(format!(
                    "host path {relative} escapes the project source directory"
                )))
            }
            Err(_) => {
                if !candidate.pop() || !candidate.starts_with(&base) {
                    return Ok(());
                }
            }
        }
    }
}

#[cfg(test)]
fn reject_unsafe_compose_features(yaml: &Yaml) -> CoreResult<()> {
    compose_policy(yaml, &[]).map(|_| ())
}

/// Capabilities that let a container escape or read the host.
const FORBIDDEN_CAPABILITIES: [&str; 8] = [
    "ALL",
    "SYS_ADMIN",
    "SYS_MODULE",
    "SYS_RAWIO",
    "SYS_PTRACE",
    "SYS_BOOT",
    "DAC_READ_SEARCH",
    "BPF",
];

/// A host path relative to the project directory: no absolute path, `~`,
/// `..` segment, or `$` interpolation (which could pull in the agent's own
/// environment, e.g. `${HOME}`).
fn safe_relative_host_path(path: &str) -> Option<String> {
    if path.is_empty()
        || path.starts_with('/')
        || path.starts_with('~')
        || path.contains(['$', '\0', '\\'])
        || path.contains("://")
        || path.split('/').any(|segment| segment == "..")
    {
        return None;
    }
    Some(path.to_string())
}

/// Static Compose policy. Returns the relative host paths the file names so
/// the caller can check they resolve inside the source tree.
/// `agent_files` are the agent's own files outside the source tree (the
/// environment file, database credentials) that generated Compose files
/// reference by absolute path; nothing else may be named that way.
fn compose_policy(yaml: &Yaml, agent_files: &[PathBuf]) -> CoreResult<Vec<String>> {
    let is_agent_file = |path: &str| agent_files.iter().any(|f| Path::new(path) == f);
    let mut host_paths = Vec::new();
    let Some(root) = yaml.as_mapping() else {
        return Err(CoreError::Validation(
            "Compose file must be a mapping".into(),
        ));
    };
    if root.contains_key("include") {
        return Err(CoreError::Validation(
            "Compose `include` is not supported; use a single Compose file".into(),
        ));
    }
    for section in ["secrets", "configs"] {
        if let Some(entries) = yaml.get(section).and_then(Yaml::as_mapping) {
            for (name, entry) in entries {
                if let Some(file) = entry.get("file") {
                    if file.as_str().is_some_and(is_agent_file) {
                        continue;
                    }
                    let path =
                        file.as_str()
                            .and_then(safe_relative_host_path)
                            .ok_or_else(|| {
                                CoreError::Validation(format!(
                                    "{section} entry {} must use a file inside the source tree",
                                    name.as_str().unwrap_or("unknown")
                                ))
                            })?;
                    host_paths.push(path);
                }
            }
        }
    }
    if let Some(volumes) = yaml.get("volumes").and_then(Yaml::as_mapping) {
        for (name, volume) in volumes {
            if volume.get("driver_opts").is_some() {
                return Err(CoreError::Validation(format!(
                    "volume {} sets driver_opts, which can bind host paths",
                    name.as_str().unwrap_or("unknown")
                )));
            }
        }
    }
    let Some(services) = yaml.get("services").and_then(Yaml::as_mapping) else {
        return Ok(host_paths);
    };
    for (name, service) in services {
        let service_name = name.as_str().unwrap_or("unknown");
        let forbid = |what: &str| {
            Err(CoreError::Validation(format!(
                "service {service_name} {what}"
            )))
        };
        let Some(map) = service.as_mapping() else {
            return forbid("must be a mapping");
        };
        let host_namespace = ["network_mode", "pid", "ipc", "uts", "cgroup", "userns_mode"]
            .iter()
            .any(|key| map.get(*key).and_then(Yaml::as_str) == Some("host"));
        let dangerous_capability =
            map.get("cap_add")
                .and_then(Yaml::as_sequence)
                .is_some_and(|caps| {
                    caps.iter().any(|cap| {
                        let cap = cap.as_str().unwrap_or("ALL").to_ascii_uppercase();
                        let cap = cap.strip_prefix("CAP_").unwrap_or(&cap);
                        FORBIDDEN_CAPABILITIES.contains(&cap)
                    })
                });
        let unconfined_security = map
            .get("security_opt")
            .and_then(Yaml::as_sequence)
            .is_some_and(|opts| {
                opts.iter().any(|opt| {
                    let opt = opt.as_str().unwrap_or("unconfined").replace(':', "=");
                    opt.ends_with("=unconfined") || opt == "label=disable"
                })
            });
        if map.get("privileged").and_then(Yaml::as_bool) == Some(true)
            || host_namespace
            || map.get("devices").is_some()
            || map.get("device_cgroup_rules").is_some()
            || dangerous_capability
            || unconfined_security
        {
            return forbid("requests a forbidden host privilege");
        }
        if map.contains_key("label_file") {
            return forbid("uses label_file, which reads host files");
        }
        if map
            .get("extends")
            .is_some_and(|extends| extends.get("file").is_some())
        {
            return forbid("extends another file; only same-file extends is supported");
        }
        if let Some(build) = map.get("build") {
            let (context, dockerfile) = match build {
                Yaml::String(context) => (Some(context.as_str()), None),
                Yaml::Mapping(spec) => {
                    for key in ["additional_contexts", "secrets", "ssh"] {
                        if spec.contains_key(key) {
                            return forbid(&format!("uses build.{key}, which reaches host files"));
                        }
                    }
                    (
                        spec.get("context").and_then(Yaml::as_str),
                        spec.get("dockerfile").and_then(Yaml::as_str),
                    )
                }
                _ => return forbid("has an invalid build section"),
            };
            let context = context.unwrap_or(".");
            let Some(context_path) = safe_relative_host_path(context) else {
                return forbid("has a build context outside the source tree");
            };
            if let Some(dockerfile) = dockerfile {
                let Some(file) = safe_relative_host_path(dockerfile) else {
                    return forbid("has a Dockerfile outside the source tree");
                };
                host_paths.push(format!("{context_path}/{file}"));
            }
            host_paths.push(context_path);
        }
        if let Some(env_files) = map.get("env_file") {
            let entries: Vec<&Yaml> = match env_files {
                Yaml::Sequence(seq) => seq.iter().collect(),
                other => vec![other],
            };
            for entry in entries {
                let path = entry
                    .as_str()
                    .or_else(|| entry.get("path").and_then(Yaml::as_str))
                    .unwrap_or_default();
                if is_agent_file(path) {
                    continue;
                }
                match safe_relative_host_path(path) {
                    Some(path) => host_paths.push(path),
                    None => return forbid("reads an env_file outside the source tree"),
                }
            }
        }
        if let Some(volumes) = map.get("volumes").and_then(Yaml::as_sequence) {
            for volume in volumes {
                if let Some(spec) = volume.as_str() {
                    let source = spec.split(':').next().unwrap_or_default();
                    let is_named = !source.is_empty()
                        && !source.contains(['/', '~', '$'])
                        && !source.starts_with('.');
                    if is_named || !spec.contains(':') {
                        // `name:/target`, or an anonymous volume `/target`.
                        continue;
                    }
                    match safe_relative_host_path(source) {
                        Some(path)
                            if path.trim_start_matches("./") != "."
                                && !path.trim_start_matches("./").is_empty() =>
                        {
                            host_paths.push(path)
                        }
                        _ => return forbid("uses a forbidden host bind mount"),
                    }
                } else if let Some(spec) = volume.as_mapping() {
                    if spec.get("type").and_then(Yaml::as_str) != Some("volume")
                        && spec.get("type").and_then(Yaml::as_str) != Some("tmpfs")
                    {
                        return forbid("uses a forbidden host bind mount");
                    }
                } else {
                    return forbid("uses a forbidden host bind mount");
                }
            }
        }
    }
    Ok(host_paths)
}

pub(crate) struct ProjectPaths {
    code_dir: PathBuf,
    compose: PathBuf,
    env: Option<PathBuf>,
    override_file: Option<PathBuf>,
}

pub(crate) fn project_root(cfg: &AgentConfig, slug: &str) -> PathBuf {
    cfg.docker_projects_root.join(slug)
}

pub(crate) fn existing_project_paths(cfg: &AgentConfig, slug: &str) -> CoreResult<ProjectPaths> {
    if !validate::is_valid_project_slug(slug) {
        return Err(CoreError::Validation(format!(
            "invalid project slug: {slug}"
        )));
    }
    let root = project_root(cfg, slug);
    let code_dir = root.join("code");
    let marker = std::fs::read_to_string(root.join(COMPOSE_PATH_MARKER)).map_err(|_| {
        CoreError::NotFound(format!("Compose metadata not found for project {slug}"))
    })?;
    let relative = marker.trim();
    if !validate::is_valid_compose_path(relative) {
        return Err(CoreError::Validation(format!(
            "invalid stored Compose path for project {slug}"
        )));
    }
    let compose = code_dir.join(relative);
    if !compose.exists() {
        return Err(CoreError::NotFound(format!(
            "Compose file not found for project {slug}"
        )));
    }
    // Same files as the last deployment, so `ps`/`stop`/`down` see the same
    // project model (including loopback ports published for domains).
    let env = [root.join(ENV_FILE), code_dir.join(".env")]
        .into_iter()
        .find(|path| path.is_file());
    let override_file = Some(root.join(OVERRIDE_FILE)).filter(|path| path.is_file());
    Ok(ProjectPaths {
        code_dir,
        compose,
        env,
        override_file,
    })
}

async fn persist_compose_path(root: &Path, code_dir: &Path, compose_path: &Path) -> CoreResult<()> {
    let relative = compose_path
        .strip_prefix(code_dir)
        .map_err(|_| CoreError::Validation("Compose path escaped project code directory".into()))?
        .to_string_lossy()
        .into_owned();
    if !validate::is_valid_compose_path(&relative) {
        return Err(CoreError::Validation("invalid Compose path".into()));
    }
    tokio::fs::write(root.join(COMPOSE_PATH_MARKER), relative)
        .await
        .map_err(|e| CoreError::Io(format!("writing Compose metadata: {e}")))
}

// Argv builders. Each user-influenced value is one discrete argument, and
// positional Git values follow `--` so they can never be read as options.

fn git_clone_args<'a>(branch: &'a str, repository_url: &'a str, dir: &'a str) -> [&'a str; 9] {
    [
        "clone",
        "--depth",
        "1",
        "--single-branch",
        "--branch",
        branch,
        "--",
        repository_url,
        dir,
    ]
}

fn git_fetch_args<'a>(dir: &'a str, branch: &'a str) -> [&'a str; 9] {
    [
        "-C", dir, "fetch", "--depth", "1", "--prune", "origin", "--", branch,
    ]
}

fn certbot_webroot_args<'a>(webroot: &'a str, domain: &'a str, email: &'a str) -> [&'a str; 11] {
    [
        "certonly",
        "--webroot",
        "-w",
        webroot,
        "-d",
        domain,
        "--non-interactive",
        "--agree-tos",
        "--keep-until-expiring",
        "-m",
        email,
    ]
}

fn compose_up_args(
    slug: &str,
    paths: &ProjectPaths,
    build: bool,
    pull_images: bool,
) -> Vec<String> {
    let mut args = compose_args(slug, paths);
    args.extend(["up".into(), "-d".into()]);
    if build {
        args.push("--build".into());
    }
    args.push("--remove-orphans".into());
    if pull_images {
        args.extend(["--pull".into(), "always".into()]);
    }
    args
}

/// `docker compose ... exec -T -e <marker>... <service> <argv...>` for a
/// scheduled job. Compose stops parsing its own options at the service
/// name, and the command never starts with `-` (see
/// `validate_job_command`), so every argument reaches the container
/// process as is, without a shell. `markers` are `NAME=value` variables the
/// agent uses to find the run's processes again.
pub(crate) fn compose_exec_args(
    slug: &str,
    paths: &ProjectPaths,
    service: &str,
    markers: &[String],
    command: &[String],
) -> Vec<String> {
    let mut args = compose_args(slug, paths);
    args.extend(["exec".into(), "-T".into()]);
    for marker in markers {
        args.extend(["-e".into(), marker.clone()]);
    }
    args.push(service.into());
    args.extend(command.iter().cloned());
    args
}

/// IDs of the containers of one service (`docker compose ps -q`).
pub(crate) async fn service_container_ids(
    slug: &str,
    paths: &ProjectPaths,
    service: &str,
) -> CoreResult<Vec<String>> {
    let mut args = compose_args(slug, paths);
    args.extend(["ps".into(), "-q".into(), service.into()]);
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let (code, stdout, stderr) = run_allow_nonzero("docker", &refs).await?;
    if code != 0 {
        return Err(CoreError::External(format!(
            "docker compose ps failed: {stderr}"
        )));
    }
    Ok(stdout
        .lines()
        .map(str::trim)
        .filter(|id| id.len() >= 12 && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_hexdigit()))
        .map(str::to_string)
        .collect())
}

fn compose_args(slug: &str, paths: &ProjectPaths) -> Vec<String> {
    let mut args = vec![
        "compose".into(),
        "-p".into(),
        slug.into(),
        "--project-directory".into(),
        paths.code_dir.to_string_lossy().into_owned(),
    ];
    if let Some(env) = &paths.env {
        args.extend(["--env-file".into(), env.to_string_lossy().into_owned()]);
    }
    args.extend(["-f".into(), paths.compose.to_string_lossy().into_owned()]);
    if let Some(override_file) = &paths.override_file {
        args.extend(["-f".into(), override_file.to_string_lossy().into_owned()]);
    }
    args
}

/// Output of stop/remove actions (deployments have their own logs).
async fn write_action_log(root: &Path, stdout: &str, stderr: &str) -> CoreResult<()> {
    let logs = root.join("logs");
    tokio::fs::create_dir_all(&logs)
        .await
        .map_err(|e| CoreError::Io(format!("creating deployment logs: {e}")))?;
    let path = logs.join("latest.log");
    tokio::fs::write(&path, format!("{stdout}{stderr}"))
        .await
        .map_err(|e| CoreError::Io(format!("writing action log: {e}")))?;
    #[cfg(unix)]
    tokio::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o640))
        .await
        .map_err(|e| CoreError::Io(format!("protecting action log: {e}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_privileged_or_host_compose_features() {
        let yaml: Yaml =
            serde_yaml::from_str("services:\n  app:\n    image: nginx\n    privileged: true")
                .unwrap();
        assert!(reject_unsafe_compose_features(&yaml).is_err());
        let yaml: Yaml =
            serde_yaml::from_str("services:\n  app:\n    image: nginx\n    network_mode: host")
                .unwrap();
        assert!(reject_unsafe_compose_features(&yaml).is_err());
        let yaml: Yaml =
            serde_yaml::from_str("services:\n  app:\n    image: nginx\n    cap_add: [SYS_ADMIN]")
                .unwrap();
        assert!(reject_unsafe_compose_features(&yaml).is_err());
    }

    #[test]
    fn allows_named_volumes_but_not_host_binds() {
        let safe: Yaml = serde_yaml::from_str(
            "services:\n  app:\n    image: nginx\n    volumes: [data:/var/lib/data]",
        )
        .unwrap();
        assert!(reject_unsafe_compose_features(&safe).is_ok());
        let unsafe_yaml: Yaml = serde_yaml::from_str("services:\n  app:\n    image: nginx\n    volumes: [/var/run/docker.sock:/var/run/docker.sock]").unwrap();
        assert!(reject_unsafe_compose_features(&unsafe_yaml).is_err());
    }

    fn policy(yaml: &str) -> CoreResult<Vec<String>> {
        compose_policy(&serde_yaml::from_str(yaml).unwrap(), &[])
    }

    #[test]
    fn rejects_compose_features_that_read_host_files() {
        for (case, yaml) in [
            ("abs context", "services:\n  a:\n    build: /\n"),
            ("parent context", "services:\n  a:\n    build:\n      context: ../..\n"),
            ("url context", "services:\n  a:\n    build: https://example.com/r.git\n"),
            ("dockerfile escape", "services:\n  a:\n    build:\n      context: .\n      dockerfile: ../../etc/x\n"),
            ("additional ctx", "services:\n  a:\n    build:\n      context: .\n      additional_contexts:\n        etc: /etc\n"),
            ("build secrets", "services:\n  a:\n    build:\n      context: .\n      secrets: [s]\n"),
            ("build ssh", "services:\n  a:\n    build:\n      context: .\n      ssh: [default]\n"),
            ("abs env_file", "services:\n  a:\n    image: x\n    env_file: /etc/shadow\n"),
            ("env_file map", "services:\n  a:\n    image: x\n    env_file:\n      - path: /root/.env\n"),
            ("label_file", "services:\n  a:\n    image: x\n    label_file: ./l\n"),
            ("extends file", "services:\n  a:\n    extends:\n      file: /etc/x.yml\n      service: b\n"),
            ("include", "include: [/etc/x.yml]\nservices:\n  a:\n    image: x\n"),
            ("secret file", "services:\n  a:\n    image: x\nsecrets:\n  s:\n    file: /etc/shadow\n"),
            ("config file", "services:\n  a:\n    image: x\nconfigs:\n  c:\n    file: ../x\n"),
            ("driver_opts bind", "services:\n  a:\n    image: x\nvolumes:\n  v:\n    driver_opts:\n      type: none\n      o: bind\n      device: /etc\n"),
            ("traversal bind", "services:\n  a:\n    image: x\n    volumes: ['./data/../../..:/h']\n"),
            ("home bind", "services:\n  a:\n    image: x\n    volumes: ['~/.ssh:/h']\n"),
            ("interpolated bind", "services:\n  a:\n    image: x\n    volumes: ['${HOME}:/h']\n"),
            ("whole tree bind", "services:\n  a:\n    image: x\n    volumes: ['./:/h']\n"),
            ("dac_read_search", "services:\n  a:\n    image: x\n    cap_add: [CAP_DAC_READ_SEARCH]\n"),
            ("sys_module", "services:\n  a:\n    image: x\n    cap_add: [sys_module]\n"),
            ("userns host", "services:\n  a:\n    image: x\n    userns_mode: host\n"),
            ("apparmor colon", "services:\n  a:\n    image: x\n    security_opt: ['apparmor:unconfined']\n"),
            ("systempaths", "services:\n  a:\n    image: x\n    security_opt: [systempaths=unconfined]\n"),
            ("device rules", "services:\n  a:\n    image: x\n    device_cgroup_rules: ['c 1:3 mr']\n"),
        ] {
            assert!(policy(yaml).is_err(), "{case} should be rejected");
        }
    }

    #[test]
    fn collects_relative_host_paths_for_resolution() {
        let paths = policy(
            "services:\n  a:\n    build:\n      context: ./web\n      dockerfile: docker/Dockerfile\n    env_file: [.env.app]\n    volumes: ['./data:/data', 'named:/n', '/anon']\n    cap_add: [NET_ADMIN]\nsecrets:\n  s:\n    file: ./secret.txt\n",
        )
        .unwrap();
        for expected in [
            "./web",
            "./web/docker/Dockerfile",
            ".env.app",
            "./data",
            "./secret.txt",
        ] {
            assert!(
                paths.iter().any(|p| p == expected),
                "{expected} in {paths:?}"
            );
        }
    }

    #[tokio::test]
    async fn host_paths_through_symlinks_must_stay_in_the_source() {
        let tmp = std::env::temp_dir().join(format!("scontrol-bind-{}", uuid::Uuid::new_v4()));
        let code = tmp.join("code");
        std::fs::create_dir_all(code.join("real")).unwrap();
        std::os::unix::fs::symlink("/etc", code.join("etc-link")).unwrap();
        std::os::unix::fs::symlink("real", code.join("inner-link")).unwrap();
        assert!(ensure_resolves_inside(&code, "./real").await.is_ok());
        assert!(ensure_resolves_inside(&code, "./inner-link/x")
            .await
            .is_ok());
        assert!(ensure_resolves_inside(&code, "./missing/deeper")
            .await
            .is_ok());
        assert!(ensure_resolves_inside(&code, "./etc-link").await.is_err());
        assert!(ensure_resolves_inside(&code, "etc-link/not-yet-there")
            .await
            .is_err());
        std::fs::remove_dir_all(tmp).unwrap();
    }

    fn application(build: ApplicationBuild) -> ApplicationSpec {
        ApplicationSpec {
            build,
            service_name: "web".into(),
            container_port: Some(8080),
            restart: panel_core::types::RestartPolicy::Always,
            memory_limit_mb: Some(512),
            cpu_limit_millis: Some(1500),
            health_check: Some(panel_core::types::ApplicationHealthCheck {
                command: vec![
                    "curl".into(),
                    "-f".into(),
                    "http://localhost:8080/ $(id)".into(),
                ],
                interval_secs: 15,
                timeout_secs: 5,
                retries: 4,
                start_period_secs: 10,
            }),
            volumes: vec![panel_core::types::ApplicationVolume {
                name: "uploads".into(),
                target: "/srv/uploads".into(),
            }],
        }
    }

    #[test]
    fn renders_application_into_a_policy_clean_compose_file() {
        let root = Path::new("/var/lib/scontrol/docker/shop");
        let spec = application(ApplicationBuild::Image {
            image: "ghcr.io/acme/shop:1.2".into(),
        });
        spec.validate().unwrap();
        let body = render_application(&spec, root, "shop").unwrap();
        let yaml: Yaml = serde_yaml::from_str(&body).unwrap();
        let env_file = root.join(ENV_FILE);
        assert!(compose_policy(&yaml, std::slice::from_ref(&env_file))
            .unwrap()
            .is_empty());
        // Without the project env file allowed, the absolute path is refused.
        assert!(compose_policy(&yaml, &[]).is_err());
        let web = &yaml["services"]["web"];
        assert_eq!(web["image"].as_str(), Some("ghcr.io/acme/shop:1.2"));
        assert_eq!(web["restart"].as_str(), Some("always"));
        assert_eq!(web["mem_limit"].as_str(), Some("512m"));
        assert_eq!(web["cpus"].as_str(), Some("1.500"));
        assert_eq!(web["expose"][0].as_str(), Some("8080"));
        assert_eq!(
            web["env_file"][0].as_str(),
            Some("/var/lib/scontrol/docker/shop/env")
        );
        // Exec-form health check: the hostile-looking argument stays one
        // argument and no shell form (`CMD-SHELL`) is produced.
        let test = web["healthcheck"]["test"].as_sequence().unwrap();
        assert_eq!(test[0].as_str(), Some("CMD"));
        assert_eq!(test[3].as_str(), Some("http://localhost:8080/ $(id)"));
        assert_eq!(web["volumes"][0]["type"].as_str(), Some("volume"));
        assert!(yaml["volumes"]["uploads"].is_mapping());
        assert_eq!(
            web.as_mapping().unwrap().len(),
            8,
            "unexpected keys in {body}"
        );

        let built = application(ApplicationBuild::Dockerfile {
            repository_url: "https://github.com/acme/shop.git".into(),
            branch: "main".into(),
            context_path: "services/web".into(),
            dockerfile_path: "Dockerfile.prod".into(),
            target: Some("runtime".into()),
        });
        let yaml: Yaml =
            serde_yaml::from_str(&render_application(&built, root, "shop").unwrap()).unwrap();
        let build = &yaml["services"]["web"]["build"];
        assert_eq!(build["context"].as_str(), Some("./services/web"));
        assert_eq!(build["dockerfile"].as_str(), Some("Dockerfile.prod"));
        assert_eq!(build["target"].as_str(), Some("runtime"));
        let paths = compose_policy(&yaml, std::slice::from_ref(&env_file)).unwrap();
        assert!(paths.contains(&"./services/web/Dockerfile.prod".to_string()));
    }

    #[test]
    fn application_validation_rejects_hostile_values() {
        let mut spec = application(ApplicationBuild::Image {
            image: "nginx\nprivileged: true".into(),
        });
        assert!(spec.validate().is_err());
        spec.build = ApplicationBuild::Image {
            image: "nginx".into(),
        };
        spec.validate().unwrap();
        spec.service_name = "web\n    privileged: true".into();
        assert!(spec.validate().is_err());
        spec.service_name = "web".into();
        spec.volumes[0].target = "/data:/etc".into();
        assert!(spec.validate().is_err());
        spec.volumes[0].target = "/data".into();
        spec.health_check.as_mut().unwrap().command = vec![];
        assert!(spec.validate().is_err());
        spec.health_check = None;
        spec.build = ApplicationBuild::Dockerfile {
            repository_url: "https://github.com/acme/shop.git".into(),
            branch: "main".into(),
            context_path: "../..".into(),
            dockerfile_path: "Dockerfile".into(),
            target: None,
        };
        assert!(spec.validate().is_err());
    }

    #[tokio::test]
    async fn generated_file_replaces_a_symlink_instead_of_following_it() {
        let tmp = std::env::temp_dir().join(format!("scontrol-gen-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        let victim = tmp.join("victim");
        std::fs::write(&victim, "keep").unwrap();
        let generated = tmp.join(APPLICATION_COMPOSE_FILE);
        std::os::unix::fs::symlink(&victim, &generated).unwrap();
        write_generated_file(&generated, "services: {}\n")
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "keep");
        assert!(!std::fs::symlink_metadata(&generated)
            .unwrap()
            .file_type()
            .is_symlink());
        std::fs::create_dir(tmp.join("dir.yml")).unwrap();
        assert!(write_generated_file(&tmp.join("dir.yml"), "x")
            .await
            .is_err());
        std::fs::remove_dir_all(tmp).unwrap();
    }

    #[test]
    fn builder_applications_run_the_locally_built_image_without_pulling() {
        let root = Path::new("/var/lib/scontrol/docker/shop");
        let spec = application(ApplicationBuild::Builder {
            builder: panel_core::types::BuildTool::Nixpacks,
            repository_url: "https://github.com/acme/shop.git".into(),
            branch: "main".into(),
            context_path: ".".into(),
        });
        spec.validate().unwrap();
        let yaml: Yaml =
            serde_yaml::from_str(&render_application(&spec, root, "shop").unwrap()).unwrap();
        let web = &yaml["services"]["web"];
        assert_eq!(web["image"].as_str(), Some("scontrol-app/shop:latest"));
        assert_eq!(web["pull_policy"].as_str(), Some("never"));
        assert!(web.get("build").is_none());
        assert!(compose_policy(&yaml, &[root.join(ENV_FILE)])
            .unwrap()
            .is_empty());
    }

    fn temp_dir(prefix: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("{prefix}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn static_publish_directory_must_resolve_inside_the_checkout() {
        let tmp = temp_dir("scontrol-static-escape");
        let code = tmp.join("code");
        std::fs::create_dir_all(code.join("dist")).unwrap();
        std::fs::create_dir_all(tmp.join("outside")).unwrap();
        std::os::unix::fs::symlink(tmp.join("outside"), code.join("escape")).unwrap();
        std::os::unix::fs::symlink("/etc", code.join("etc")).unwrap();
        assert!(
            ensure_inside_named(&code, &code.join("dist"), "publish directory")
                .await
                .is_ok()
        );
        for escaping in ["escape", "etc"] {
            let error = ensure_inside_named(&code, &code.join(escaping), "publish directory")
                .await
                .unwrap_err();
            assert!(error.to_string().contains("escapes"), "{error}");
        }
        std::fs::remove_dir_all(tmp).ok();
    }

    #[test]
    fn static_publish_copies_regular_files_only_and_never_follows_links() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = temp_dir("scontrol-publish");
        let dist = tmp.join("dist");
        std::fs::create_dir_all(dist.join("assets/js")).unwrap();
        std::fs::create_dir_all(dist.join(".git")).unwrap();
        std::fs::write(dist.join("index.html"), "<h1>hi</h1>").unwrap();
        std::fs::write(dist.join("assets/js/app.js"), "console.log(1)").unwrap();
        std::fs::write(dist.join(".git/config"), "[remote]").unwrap();
        std::fs::write(tmp.join("secret"), "host file").unwrap();
        std::fs::set_permissions(
            dist.join("index.html"),
            std::fs::Permissions::from_mode(0o4755),
        )
        .unwrap();
        // Links a hostile repository or build could plant.
        std::os::unix::fs::symlink(tmp.join("secret"), dist.join("leak.txt")).unwrap();
        std::os::unix::fs::symlink("/etc", dist.join("assets/etc")).unwrap();
        std::os::unix::fs::symlink("/etc/passwd", dist.join("passwd")).unwrap();

        let release = tmp.join("release");
        let stats = publish_tree(&dist, &release).unwrap();
        assert_eq!(stats.files, 2);
        assert_eq!(stats.skipped_total, 3);
        assert_eq!(
            std::fs::read_to_string(release.join("index.html")).unwrap(),
            "<h1>hi</h1>"
        );
        assert!(release.join("assets/js/app.js").is_file());
        for absent in ["leak.txt", "assets/etc", "passwd", ".git"] {
            assert!(
                std::fs::symlink_metadata(release.join(absent)).is_err(),
                "{absent} was published"
            );
        }
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode(&release.join("index.html")), 0o644, "setuid bit kept");
        assert_eq!(mode(&release.join("assets")), 0o755);
        // A release is always fresh: publishing into an existing one fails.
        assert!(publish_tree(&dist, &release).is_err());
        std::fs::remove_dir_all(tmp).ok();
    }

    #[tokio::test]
    async fn old_static_releases_are_pruned_but_the_served_one_stays() {
        let releases = temp_dir("scontrol-releases");
        let mut ids = Vec::new();
        for _ in 0..5 {
            let id = uuid::Uuid::new_v4().to_string();
            std::fs::create_dir(releases.join(&id)).unwrap();
            ids.push(id);
            std::thread::sleep(Duration::from_millis(20));
        }
        std::fs::create_dir(releases.join("not-a-release")).unwrap();
        prune_static_releases(&releases, &ids[0]).await;
        let left: HashSet<String> = std::fs::read_dir(&releases)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        let expected: HashSet<String> = [&ids[0], &ids[3], &ids[4]]
            .into_iter()
            .cloned()
            .chain(["not-a-release".to_string()])
            .collect();
        assert_eq!(left, expected);
        std::fs::remove_dir_all(releases).ok();
    }

    #[tokio::test]
    async fn static_sites_report_their_release_and_remove_their_files() {
        let tmp = temp_dir("scontrol-static-site");
        let cfg = AgentConfig {
            docker_projects_root: tmp.join("docker"),
            docker_volumes_root: std::path::PathBuf::from("/nonexistent-volumes"),
            static_sites_root: tmp.join("static"),
            ..Default::default()
        };
        let release = uuid::Uuid::new_v4().to_string();
        let root = project_root(&cfg, "site");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(
            static_site_dir(&cfg, "site")
                .join("releases")
                .join(&release),
        )
        .unwrap();
        std::fs::write(root.join(STATIC_MARKER), &release).unwrap();

        let services = list_services_for(&cfg, "site").await.unwrap();
        assert_eq!(services.len(), 1);
        assert!(services[0].status.contains(&release));
        let action = || ComposeProjectActionParams {
            project_slug: "site".into(),
        };
        assert!(matches!(
            stop(&cfg, action()).await,
            Err(CoreError::Validation(_))
        ));
        let logs = logs(
            &cfg,
            ComposeProjectLogsParams {
                project_slug: "site".into(),
                tail_lines: 10,
            },
        )
        .await
        .unwrap();
        assert!(logs.contains("no container"));
        remove(&cfg, action()).await.unwrap();
        assert!(!root.exists());
        assert!(!static_site_dir(&cfg, "site").exists());
        std::fs::remove_dir_all(tmp).ok();
    }

    #[test]
    fn static_build_runs_exec_form_without_privileges() {
        let command = vec![
            "npm".to_string(),
            "run".into(),
            "build; id $(whoami)".into(),
        ];
        let args = static_build_args(
            "scontrol-static-build-site",
            "/var/lib/scontrol/docker/site/code",
            "/var/lib/scontrol/docker/site/env",
            "node:22-alpine",
            &command,
        );
        assert_eq!(&args[..2], &["run", "--rm"]);
        let after = |flag: &str| args[args.iter().position(|a| a == flag).unwrap() + 1].clone();
        assert_eq!(after("--cap-drop"), "ALL");
        assert_eq!(after("--security-opt"), "no-new-privileges");
        assert_eq!(after("--env-file"), "/var/lib/scontrol/docker/site/env");
        assert_eq!(
            after("--mount"),
            "type=bind,source=/var/lib/scontrol/docker/site/code,target=/workspace"
        );
        let image = args.iter().position(|a| a == "node:22-alpine").unwrap();
        assert_eq!(&args[image + 1..], command.as_slice());
        for forbidden in ["--privileged", "sh", "-c", "--network=host", "-v"] {
            assert!(!args.iter().any(|a| a == forbidden), "{forbidden}");
        }
    }

    fn database_spec(engine: panel_core::types::ManagedDatabaseEngine) -> DatabaseServiceSpec {
        DatabaseServiceSpec {
            engine,
            version: engine.default_version().into(),
            database_name: "shop".into(),
            username: "shop_user".into(),
            memory_limit_mb: Some(512),
            cpu_limit_millis: Some(1500),
        }
    }

    fn database_credentials(
        engine: panel_core::types::ManagedDatabaseEngine,
    ) -> panel_core::types::DatabaseCredentials {
        panel_core::types::DatabaseCredentials {
            password: "SecretPw0123456789abcdefXYZ".into(),
            root_password: engine
                .has_root_password()
                .then(|| "RootPw0123456789abcdefXYZ".into()),
        }
    }

    #[test]
    fn database_templates_publish_on_loopback_only_with_named_volumes() {
        use panel_core::types::ManagedDatabaseEngine;
        let root = Path::new("/var/lib/scontrol/docker/shop-db");
        let agent_files = [
            root.join(ENV_FILE),
            root.join(DATABASE_ENV_FILE),
            root.join(REDIS_CONFIG_FILE),
        ];
        for engine in ManagedDatabaseEngine::ALL {
            let spec = database_spec(engine);
            let body = render_database(&spec, 25003, root).unwrap();
            let creds = database_credentials(engine);
            for secret in creds.secrets() {
                assert!(!body.contains(&secret), "{engine} leaks a password");
            }
            let yaml: Yaml = serde_yaml::from_str(&body).unwrap();
            // The shared policy accepts it; without the agent files it cannot
            // name the absolute credential paths.
            assert!(
                compose_policy(&yaml, &agent_files).unwrap().is_empty(),
                "{engine}"
            );
            assert!(compose_policy(&yaml, &[]).is_err(), "{engine}");
            let db = &yaml["services"][DATABASE_SERVICE_NAME];
            assert_eq!(
                db["image"].as_str(),
                Some(engine.image(&spec.version).as_str())
            );
            let ports = db["ports"].as_sequence().unwrap();
            assert_eq!(ports.len(), 1);
            assert_eq!(
                ports[0].as_str().unwrap(),
                format!("127.0.0.1:25003:{}", engine.container_port())
            );
            let volume = &db["volumes"][0];
            assert_eq!(volume["type"].as_str(), Some("volume"));
            assert_eq!(volume["source"].as_str(), Some("data"));
            assert_eq!(
                volume["target"].as_str(),
                Some(engine.data_path(&spec.version))
            );
            assert!(yaml["volumes"]["data"].is_mapping());
            let env_files: Vec<&str> = db["env_file"]
                .as_sequence()
                .unwrap()
                .iter()
                .filter_map(Yaml::as_str)
                .collect();
            assert_eq!(
                env_files.last().copied(),
                Some(root.join(DATABASE_ENV_FILE).to_str().unwrap()),
                "credentials must be the last env_file so they win"
            );
            assert_eq!(db["healthcheck"]["test"][0].as_str(), Some("CMD"));
            assert_eq!(db["mem_limit"].as_str(), Some("512m"));
            assert_eq!(db["cpus"].as_str(), Some("1.500"));
            assert!(db.get("privileged").is_none());
            assert!(db.get("network_mode").is_none());
            if engine == ManagedDatabaseEngine::Redis {
                assert_eq!(db["command"][1].as_str(), Some("/run/secrets/redis_conf"));
                assert_eq!(
                    yaml["secrets"]["redis_conf"]["file"].as_str(),
                    Some(root.join(REDIS_CONFIG_FILE).to_str().unwrap())
                );
            } else {
                assert!(db.get("command").is_none());
            }
        }
    }

    #[test]
    fn database_deployments_need_matching_credentials_and_no_domains() {
        use panel_core::types::{DatabaseDeployment, ManagedDatabaseEngine};
        let engine = ManagedDatabaseEngine::Postgres;
        let params = |database: Option<DatabaseDeployment>, domains| DeployComposeProjectParams {
            project_id: uuid::Uuid::new_v4(),
            deployment_id: uuid::Uuid::new_v4(),
            project_slug: "shop-db".into(),
            source: ComposeSource::Database(Box::new(database_spec(engine))),
            pull_images: false,
            domains,
            acme_email: None,
            git_credentials: None,
            database,
            rollback: None,
        };
        let good = DatabaseDeployment {
            loopback_port: 25000,
            credentials: database_credentials(engine),
        };
        assert!(validate_project_params(&params(Some(good.clone()), vec![])).is_ok());
        assert!(validate_project_params(&params(None, vec![])).is_err());
        let public_port = DatabaseDeployment {
            loopback_port: 5432,
            ..good.clone()
        };
        assert!(validate_project_params(&params(Some(public_port), vec![])).is_err());
        let mysql_creds = DatabaseDeployment {
            credentials: database_credentials(ManagedDatabaseEngine::Mysql),
            ..good.clone()
        };
        assert!(validate_project_params(&params(Some(mysql_creds), vec![])).is_err());
        let domain = vec![binding("db.example.com", "db", 40000, false)];
        assert!(validate_project_params(&params(Some(good.clone()), domain)).is_err());

        let mut raw = params(Some(good.clone()), vec![]);
        raw.source = ComposeSource::Raw {
            compose_yaml: "services: {}".into(),
        };
        assert!(validate_project_params(&raw).is_err());

        // Database services are never rolled back.
        let mut rollback = params(Some(good), vec![]);
        let target = uuid::Uuid::new_v4();
        rollback.rollback = Some(panel_core::types::RollbackTarget {
            deployment_id: target,
            compose_sha256: "a".repeat(64),
            images: vec![],
        });
        assert!(validate_project_params(&rollback).is_err());
    }

    #[test]
    fn rollback_targets_name_only_tags_of_the_target_deployment() {
        use panel_core::types::{deployment_image_tag, PinnedImage, RollbackTarget};
        let target = uuid::Uuid::new_v4();
        let pinned = |tag: String| PinnedImage {
            service: "web".into(),
            tag,
            image_id: format!("sha256:{}", "0".repeat(64)),
        };
        let params = |images: Vec<PinnedImage>| DeployComposeProjectParams {
            project_id: uuid::Uuid::new_v4(),
            deployment_id: uuid::Uuid::new_v4(),
            project_slug: "shop".into(),
            source: ComposeSource::Raw {
                compose_yaml: "services:\n  web:\n    image: nginx".into(),
            },
            pull_images: false,
            domains: vec![],
            acme_email: None,
            git_credentials: None,
            database: None,
            rollback: Some(RollbackTarget {
                deployment_id: target,
                compose_sha256: "a".repeat(64),
                images,
            }),
        };
        let own = deployment_image_tag("shop", "web", target).unwrap();
        assert!(validate_project_params(&params(vec![pinned(own)])).is_ok());
        for foreign in [
            "nginx:latest".to_string(),
            deployment_image_tag("other", "web", target).unwrap(),
            deployment_image_tag("shop", "web", uuid::Uuid::new_v4()).unwrap(),
        ] {
            assert!(
                validate_project_params(&params(vec![pinned(foreign.clone())])).is_err(),
                "{foreign}"
            );
        }
        let mut bad_hash = params(vec![]);
        bad_hash.rollback.as_mut().unwrap().compose_sha256 = "zz".into();
        assert!(validate_project_params(&bad_hash).is_err());
        let mut static_site = params(vec![]);
        static_site.source = ComposeSource::Static(Box::new(panel_core::types::StaticSiteSpec {
            repository_url: "https://github.com/acme/site.git".into(),
            branch: "main".into(),
            publish_path: "dist".into(),
            build: None,
            spa: false,
        }));
        assert!(validate_project_params(&static_site).is_err());
    }

    #[tokio::test]
    async fn database_credentials_are_private_and_masked() {
        use panel_core::types::ManagedDatabaseEngine;
        let root = temp_dir("scontrol-db-files");
        let engine = ManagedDatabaseEngine::Mysql;
        let database = panel_core::types::DatabaseDeployment {
            loopback_port: 25000,
            credentials: database_credentials(engine),
        };
        write_database_files(&database_spec(engine), &database, &root)
            .await
            .unwrap();
        let path = root.join(DATABASE_ENV_FILE);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        assert!(!root.join(REDIS_CONFIG_FILE).exists());
        let secrets = maskable_secrets(environment_secrets(&root).await.unwrap());
        let line = format!(
            "mysql -p{} root={}",
            database.credentials.password,
            database.credentials.root_password.clone().unwrap()
        );
        let masked = redact(&line, &secrets);
        for secret in database.credentials.secrets() {
            assert!(!masked.contains(&secret));
        }
        std::fs::remove_dir_all(&root).ok();
    }

    fn binding(domain: &str, service: &str, host_port: u16, https: bool) -> ComposeDomainBinding {
        ComposeDomainBinding {
            domain: domain.into(),
            service_name: service.into(),
            container_port: 3000,
            host_port,
            https,
        }
    }

    #[test]
    fn override_publishes_bound_ports_on_loopback_only() {
        let rendered = render_override(&[
            binding("a.example.com", "web", 40001, false),
            binding("b.example.com", "web", 40002, false),
            binding("api.example.com", "api", 40003, false),
        ])
        .unwrap();
        let yaml: Yaml = serde_yaml::from_str(&rendered).unwrap();
        let web_ports = yaml["services"]["web"]["ports"].as_sequence().unwrap();
        assert_eq!(web_ports.len(), 2);
        assert_eq!(web_ports[0].as_str(), Some("127.0.0.1:40001:3000"));
        assert_eq!(
            yaml["services"]["api"]["ports"][0].as_str(),
            Some("127.0.0.1:40003:3000")
        );
    }

    #[test]
    fn rejects_invalid_or_conflicting_bindings() {
        assert!(validate_bindings(&[binding("a.example.com", "web", 40001, false)], None).is_ok());
        // HTTPS needs an ACME email.
        assert!(validate_bindings(&[binding("a.example.com", "web", 40001, true)], None).is_err());
        assert!(validate_bindings(
            &[binding("a.example.com", "web", 40001, true)],
            Some("ops@example.com")
        )
        .is_ok());
        assert!(validate_bindings(
            &[binding("a.example.com", "web", 40001, true)],
            Some("-oProxyCommand@example.com")
        )
        .is_err());
        // Duplicate host port, privileged host port, injection in names.
        assert!(validate_bindings(
            &[
                binding("a.example.com", "web", 40001, false),
                binding("b.example.com", "web", 40001, false)
            ],
            None
        )
        .is_err());
        assert!(validate_bindings(&[binding("a.example.com", "web", 80, false)], None).is_err());
        assert!(
            validate_bindings(&[binding("a.example.com", "web;rm", 40001, false)], None).is_err()
        );
        assert!(
            validate_bindings(&[binding("a.com\nlisten 1", "web", 40001, false)], None).is_err()
        );
    }

    #[test]
    fn bindings_must_target_declared_services() {
        let compose: Yaml = serde_yaml::from_str("services:\n  web:\n    image: nginx").unwrap();
        assert!(check_bindings_against_compose(
            &compose,
            &[binding("a.example.com", "web", 40001, false)]
        )
        .is_ok());
        assert!(check_bindings_against_compose(
            &compose,
            &[binding("a.example.com", "db", 40001, false)]
        )
        .is_err());
    }

    #[tokio::test]
    async fn compose_path_symlink_cannot_escape_source() {
        let base = std::env::temp_dir().join(format!("scontrol-compose-{}", uuid::Uuid::new_v4()));
        let code = base.join("code");
        std::fs::create_dir_all(&code).unwrap();
        std::fs::write(base.join("outside.yml"), "services: {}").unwrap();
        std::fs::write(code.join("inside.yml"), "services: {}").unwrap();
        std::os::unix::fs::symlink(base.join("outside.yml"), code.join("escape.yml")).unwrap();
        assert!(ensure_inside(&code, &code.join("inside.yml")).await.is_ok());
        assert!(ensure_inside(&code, &code.join("escape.yml"))
            .await
            .is_err());
        std::fs::remove_dir_all(base).ok();
    }

    #[tokio::test]
    async fn environment_file_is_private_and_round_trips() {
        use std::os::unix::fs::PermissionsExt;
        let base = std::env::temp_dir().join(format!("scontrol-env-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&base).unwrap();
        let path = base.join(ENV_FILE);
        let env = vec![("TOKEN".to_string(), "a=b c".to_string())];
        write_environment(&path, &env).await.unwrap();
        assert_eq!(read_environment(&path).await.unwrap(), env);
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        std::fs::remove_dir_all(base).ok();
    }

    #[tokio::test]
    async fn overridden_inherited_values_are_kept_private_for_masking() {
        use std::os::unix::fs::PermissionsExt;
        let base = std::env::temp_dir().join(format!("scontrol-mask-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&base).unwrap();
        let cfg = AgentConfig {
            docker_projects_root: base.clone(),
            docker_volumes_root: std::path::PathBuf::from("/nonexistent-volumes"),
            ..AgentConfig::default()
        };
        write_project_environment(
            &cfg,
            WriteComposeEnvironmentParams {
                project_slug: "layered".into(),
                environment: vec![("DB_URL".into(), "service-value-1".into())],
                masked_values: vec!["project-value-1".into()],
            },
        )
        .await
        .unwrap();
        let root = project_root(&cfg, "layered");
        let secrets = maskable_secrets(environment_secrets(&root).await.unwrap());
        let line = redact("db=service-value-1 old=project-value-1", &secrets);
        assert!(!line.contains("service-value-1"), "{line}");
        assert!(!line.contains("project-value-1"), "{line}");
        let mode = std::fs::metadata(root.join(MASKED_VALUES_FILE))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
        assert!(validate_masked_values(&["a\nb".into()]).is_err());
        std::fs::remove_dir_all(base).ok();
    }

    #[test]
    fn parses_compose_ps_json_lines_and_arrays() {
        let line = r#"{"Service":"web","Name":"shop-web-1","Image":"nginx","State":"running","Status":"Up 2 minutes (healthy)","Health":"healthy","ExitCode":0,"Publishers":[{"URL":"127.0.0.1","TargetPort":80,"PublishedPort":40000,"Protocol":"tcp"},{"URL":"","TargetPort":443,"PublishedPort":0,"Protocol":"tcp"}]}"#;
        let worker = r#"{"Service":"worker","Name":"shop-worker-1","Image":"app","State":"exited","Status":"Exited (1)","ExitCode":1,"Publishers":null}"#;
        let services = parse_ps_json(&format!("{line}\n{worker}\n")).unwrap();
        assert_eq!(services.len(), 2);
        assert_eq!(services[0].name, "web");
        assert_eq!(services[0].container, "shop-web-1");
        assert_eq!(services[0].health, "healthy");
        assert_eq!(
            services[0].ports,
            vec!["127.0.0.1:40000->80/tcp", "443/tcp"]
        );
        assert_eq!(services[1].exit_code, 1);
        assert!(services[1].ports.is_empty());

        let array = parse_ps_json(&format!("[{line}]")).unwrap();
        assert_eq!(array.len(), 1);
        assert!(parse_ps_json("").unwrap().is_empty());
    }

    #[tokio::test]
    async fn cancelling_stops_the_running_process_group() {
        let dir = std::env::temp_dir().join(format!("scontrol-cancel-{}", uuid::Uuid::new_v4()));
        let (tx, rx) = watch::channel(false);
        let mut log = DeployLog::create(&dir, uuid::Uuid::new_v4(), 4096, vec![], rx)
            .await
            .unwrap();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            tx.send(true).ok();
        });
        let started = std::time::Instant::now();
        let result = run_streamed("sleep", &["30"], &[], &mut log, Duration::from_secs(60)).await;
        assert!(matches!(result, Err(CoreError::Cancelled(_))));
        assert!(started.elapsed() < Duration::from_secs(5));
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn timeout_stops_the_running_process_group() {
        let dir = std::env::temp_dir().join(format!("scontrol-timeout-{}", uuid::Uuid::new_v4()));
        let mut log = DeployLog::create(
            &dir,
            uuid::Uuid::new_v4(),
            4096,
            vec![],
            watch::channel(false).1,
        )
        .await
        .unwrap();
        let result =
            run_streamed("sleep", &["30"], &[], &mut log, Duration::from_millis(200)).await;
        assert!(matches!(result, Err(CoreError::External(m)) if m.contains("timed out")));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn argv_keeps_each_value_discrete_and_positionals_after_double_dash() {
        // Values a validator would refuse, to show the argv shape alone keeps
        // them inert: no shell parses them and none can become an option.
        let hostile = "x; rm -rf / $(id) `id` --upload-pack=evil";

        let clone = git_clone_args(hostile, hostile, "/var/lib/scontrol/docker/app/code");
        let dash = clone.iter().position(|a| *a == "--").unwrap();
        assert_eq!(clone[dash + 1], hostile);
        assert_eq!(clone[dash + 2], "/var/lib/scontrol/docker/app/code");
        assert_eq!(
            clone[clone.iter().position(|a| *a == "--branch").unwrap() + 1],
            hostile
        );

        let fetch = git_fetch_args("/srv/code", hostile);
        assert_eq!(&fetch[fetch.len() - 2..], &["--", hostile]);

        let certbot = certbot_webroot_args("/var/lib/scontrol/acme", hostile, hostile);
        assert_eq!(
            certbot[certbot.iter().position(|a| *a == "-d").unwrap() + 1],
            hostile
        );
        assert_eq!(certbot[certbot.len() - 1], hostile);
        assert!(certbot.contains(&"--non-interactive"));

        let paths = ProjectPaths {
            code_dir: "/data/my app/code".into(),
            compose: "/data/my app/code/compose file.yml".into(),
            env: Some("/data/my app/env".into()),
            override_file: Some("/data/my app/scontrol.override.yml".into()),
        };
        let up = compose_up_args("my-app", &paths, true, true);
        assert_eq!(&up[..3], &["compose", "-p", "my-app"]);
        assert!(up.contains(&"/data/my app/code/compose file.yml".to_string()));
        assert_eq!(
            up[up.iter().position(|a| a == "--env-file").unwrap() + 1],
            "/data/my app/env",
            "the env file is passed by path; values never reach argv"
        );
        assert!(up.ends_with(&[
            "up".into(),
            "-d".into(),
            "--build".into(),
            "--remove-orphans".into(),
            "--pull".into(),
            "always".into()
        ]));
        assert!(!compose_up_args("my-app", &paths, true, false).contains(&"--pull".to_string()));
        // A rollback neither builds nor pulls.
        let rollback = compose_up_args("my-app", &paths, false, false);
        assert!(rollback.ends_with(&["up".into(), "-d".into(), "--remove-orphans".into()]));
        assert!(!rollback.iter().any(|a| a == "--build" || a == "--pull"));

        for args in [clone.to_vec(), fetch.to_vec(), certbot.to_vec()] {
            assert!(!args
                .iter()
                .any(|a| *a == "sh" || *a == "-c" || *a == "bash"));
        }
    }
}
