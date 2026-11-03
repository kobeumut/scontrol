//! Dokploy-inspired, Compose-project deployment adapter.
//!
//! This is deliberately not a general `docker run` control surface. Each
//! deployment has a stable, validated project slug and an isolated directory:
//! `<docker_projects_root>/<slug>/code`. The agent accepts only a Git source
//! or raw Compose YAML, writes a 0600 `.env`, applies a conservative Compose
//! policy, and invokes Docker with a fixed argv vector.

use super::exec::run_allow_nonzero;
use crate::config::AgentConfig;
use panel_core::adapters::{
    ComposeProjectActionParams, ComposeProjectLogsParams, DeployComposeProjectParams,
};
use panel_core::error::{CoreError, CoreResult};
use panel_core::types::{validate, ComposeServiceInfo, ComposeSource};
use serde_yaml::Value as Yaml;
use std::path::{Path, PathBuf};

const DEFAULT_COMPOSE_FILE: &str = "docker-compose.yml";

pub async fn deploy(
    cfg: &AgentConfig,
    params: DeployComposeProjectParams,
) -> CoreResult<Vec<ComposeServiceInfo>> {
    validate_project_params(&params)?;
    let root = project_root(cfg, &params.project_slug);
    let code_dir = root.join("code");
    tokio::fs::create_dir_all(&root)
        .await
        .map_err(|e| CoreError::Io(format!("creating project root: {e}")))?;

    let compose_path = materialize_source(&code_dir, &params.source).await?;
    validate_compose_file(&compose_path).await?;
    persist_compose_path(&root, &code_dir, &compose_path).await?;
    write_environment(&code_dir, &params.environment).await?;

    let mut args = compose_args(&params.project_slug, &code_dir, &compose_path)?;
    args.extend([
        "up".into(),
        "-d".into(),
        "--build".into(),
        "--remove-orphans".into(),
    ]);
    if params.pull_images {
        args.extend(["--pull".into(), "always".into()]);
    }

    // Capture stdout/stderr to a panel-owned, non-secret deployment trace.
    // The environment values are never logged by this adapter.
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let (code, stdout, stderr) = run_allow_nonzero("docker", &refs).await?;
    write_deploy_log(&root, &stdout, &stderr).await?;
    if code != 0 {
        return Err(CoreError::External(format!(
            "docker compose deployment failed (see project log; exit code {code})"
        )));
    }

    list_services_for(cfg, &params.project_slug).await
}

pub async fn stop(cfg: &AgentConfig, params: ComposeProjectActionParams) -> CoreResult<()> {
    let (code_dir, compose_path) = existing_project_paths(cfg, &params.project_slug)?;
    let mut args = compose_args(&params.project_slug, &code_dir, &compose_path)?;
    args.extend(["stop".into()]);
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let (code, stdout, stderr) = run_allow_nonzero("docker", &refs).await?;
    write_deploy_log(&project_root(cfg, &params.project_slug), &stdout, &stderr).await?;
    if code != 0 {
        return Err(CoreError::External(format!(
            "docker compose stop failed (exit code {code})"
        )));
    }
    Ok(())
}

pub async fn remove(cfg: &AgentConfig, params: ComposeProjectActionParams) -> CoreResult<()> {
    let (code_dir, compose_path) = existing_project_paths(cfg, &params.project_slug)?;
    let mut args = compose_args(&params.project_slug, &code_dir, &compose_path)?;
    // Do not pass `--volumes`: persistent named volumes survive a project
    // removal unless an operator explicitly removes them outside the panel.
    args.extend(["down".into(), "--remove-orphans".into()]);
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let (code, stdout, stderr) = run_allow_nonzero("docker", &refs).await?;
    write_deploy_log(&project_root(cfg, &params.project_slug), &stdout, &stderr).await?;
    if code != 0 {
        return Err(CoreError::External(format!(
            "docker compose removal failed (exit code {code})"
        )));
    }
    tokio::fs::remove_dir_all(project_root(cfg, &params.project_slug))
        .await
        .map_err(|e| CoreError::Io(format!("removing project files: {e}")))?;
    Ok(())
}

pub async fn list_services_for(
    cfg: &AgentConfig,
    project_slug: &str,
) -> CoreResult<Vec<ComposeServiceInfo>> {
    let (code_dir, compose_path) = existing_project_paths(cfg, project_slug)?;
    let mut args = compose_args(project_slug, &code_dir, &compose_path)?;
    args.extend([
        "ps".into(),
        "--all".into(),
        "--format".into(),
        "{{.Service}}\t{{.Image}}\t{{.State}}\t{{.Publishers}}".into(),
    ]);
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let (code, stdout, stderr) = run_allow_nonzero("docker", &refs).await?;
    if code != 0 {
        return Err(CoreError::External(format!(
            "docker compose ps failed: {stderr}"
        )));
    }
    Ok(stdout
        .lines()
        .filter_map(|line| {
            let mut f = line.split('\t');
            Some(ComposeServiceInfo {
                name: f.next()?.to_string(),
                image: f.next().unwrap_or_default().to_string(),
                state: f.next().unwrap_or_default().to_string(),
                ports: f
                    .next()
                    .unwrap_or_default()
                    .split(", ")
                    .filter(|port| !port.is_empty())
                    .map(str::to_string)
                    .collect(),
            })
        })
        .collect())
}

pub async fn logs(cfg: &AgentConfig, params: ComposeProjectLogsParams) -> CoreResult<String> {
    let (code_dir, compose_path) = existing_project_paths(cfg, &params.project_slug)?;
    let tail = params.tail_lines.clamp(1, 2_000).to_string();
    let mut args = compose_args(&params.project_slug, &code_dir, &compose_path)?;
    args.extend(["logs".into(), "--no-color".into(), "--tail".into(), tail]);
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let (code, stdout, stderr) = run_allow_nonzero("docker", &refs).await?;
    if code != 0 {
        return Err(CoreError::External(format!(
            "docker compose logs failed: {stderr}"
        )));
    }
    Ok(stdout)
}

fn validate_project_params(params: &DeployComposeProjectParams) -> CoreResult<()> {
    if !validate::is_valid_project_slug(&params.project_slug) {
        return Err(CoreError::Validation(format!(
            "invalid project slug: {}",
            params.project_slug
        )));
    }
    if params.environment.len() > 256 {
        return Err(CoreError::Validation(
            "too many environment variables".into(),
        ));
    }
    for (key, value) in &params.environment {
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
    }
    Ok(())
}

async fn materialize_source(code_dir: &Path, source: &ComposeSource) -> CoreResult<PathBuf> {
    if code_dir.exists() {
        tokio::fs::remove_dir_all(code_dir)
            .await
            .map_err(|e| CoreError::Io(format!("clearing previous source: {e}")))?;
    }
    match source {
        ComposeSource::Raw { compose_yaml } => {
            tokio::fs::create_dir_all(code_dir)
                .await
                .map_err(|e| CoreError::Io(e.to_string()))?;
            let path = code_dir.join(DEFAULT_COMPOSE_FILE);
            tokio::fs::write(&path, compose_yaml)
                .await
                .map_err(|e| CoreError::Io(format!("writing Compose YAML: {e}")))?;
            Ok(path)
        }
        ComposeSource::Git {
            repository_url,
            branch,
            compose_path,
        } => {
            let destination = code_dir.to_string_lossy().into_owned();
            let refs = [
                "clone",
                "--depth",
                "1",
                "--branch",
                branch.as_str(),
                repository_url.as_str(),
                destination.as_str(),
            ];
            let (code, _stdout, stderr) = run_allow_nonzero("git", &refs).await?;
            if code != 0 {
                return Err(CoreError::External(format!("Git clone failed: {stderr}")));
            }
            Ok(code_dir.join(compose_path))
        }
    }
}

async fn write_environment(code_dir: &Path, environment: &[(String, String)]) -> CoreResult<()> {
    let mut body = String::new();
    for (key, value) in environment {
        // Newlines are rejected in validation, so one key/value occupies one
        // line. The Compose `.env` parser receives literal values as supplied.
        body.push_str(key);
        body.push('=');
        body.push_str(value);
        body.push('\n');
    }
    let path = code_dir.join(".env");
    tokio::fs::write(&path, body)
        .await
        .map_err(|e| CoreError::Io(format!("writing environment file: {e}")))?;
    #[cfg(unix)]
    tokio::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o600))
        .await
        .map_err(|e| CoreError::Io(format!("protecting environment file: {e}")))?;
    Ok(())
}

async fn validate_compose_file(path: &Path) -> CoreResult<()> {
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
    reject_unsafe_compose_features(&yaml)
}

fn reject_unsafe_compose_features(yaml: &Yaml) -> CoreResult<()> {
    let Some(services) = yaml.get("services").and_then(Yaml::as_mapping) else {
        return Ok(());
    };
    for (name, service) in services {
        let service_name = name.as_str().unwrap_or("unknown");
        let Some(map) = service.as_mapping() else {
            return Err(CoreError::Validation(format!(
                "service {service_name} must be a mapping"
            )));
        };
        let host_namespace = ["network_mode", "pid", "ipc", "uts", "cgroup"]
            .iter()
            .any(|key| map.get(*key).and_then(Yaml::as_str) == Some("host"));
        let sys_admin_capability =
            map.get("cap_add")
                .and_then(Yaml::as_sequence)
                .is_some_and(|caps| {
                    caps.iter()
                        .any(|cap| matches!(cap.as_str(), Some("SYS_ADMIN") | Some("ALL")))
                });
        let unconfined_security = map
            .get("security_opt")
            .and_then(Yaml::as_sequence)
            .is_some_and(|opts| {
                opts.iter().any(|opt| {
                    matches!(
                        opt.as_str(),
                        Some("apparmor=unconfined") | Some("seccomp=unconfined")
                    )
                })
            });
        if map.get("privileged").and_then(Yaml::as_bool) == Some(true)
            || host_namespace
            || map.get("devices").is_some()
            || sys_admin_capability
            || unconfined_security
        {
            return Err(CoreError::Validation(format!(
                "service {service_name} requests a forbidden host privilege"
            )));
        }
        if let Some(volumes) = map.get("volumes").and_then(Yaml::as_sequence) {
            for volume in volumes {
                let unsafe_bind = if let Some(spec) = volume.as_str() {
                    let source = spec.split(':').next().unwrap_or_default();
                    source.starts_with('/') || source.starts_with("../") || source == "."
                } else if let Some(spec) = volume.as_mapping() {
                    spec.get("type").and_then(Yaml::as_str) == Some("bind")
                } else {
                    true
                };
                if unsafe_bind {
                    return Err(CoreError::Validation(format!(
                        "service {service_name} uses a forbidden host bind mount"
                    )));
                }
            }
        }
    }
    Ok(())
}

fn project_root(cfg: &AgentConfig, slug: &str) -> PathBuf {
    cfg.docker_projects_root.join(slug)
}

fn existing_project_paths(cfg: &AgentConfig, slug: &str) -> CoreResult<(PathBuf, PathBuf)> {
    if !validate::is_valid_project_slug(slug) {
        return Err(CoreError::Validation(format!(
            "invalid project slug: {slug}"
        )));
    }
    let root = project_root(cfg, slug);
    let code_dir = root.join("code");
    let marker = std::fs::read_to_string(root.join(".scontrol-compose-path")).map_err(|_| {
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
    Ok((code_dir, compose))
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
    tokio::fs::write(root.join(".scontrol-compose-path"), relative)
        .await
        .map_err(|e| CoreError::Io(format!("writing Compose metadata: {e}")))
}

fn compose_args(slug: &str, code_dir: &Path, compose_path: &Path) -> CoreResult<Vec<String>> {
    let code = code_dir.to_string_lossy().into_owned();
    let compose = compose_path.to_string_lossy().into_owned();
    Ok(vec![
        "compose".into(),
        "-p".into(),
        slug.into(),
        "--project-directory".into(),
        code,
        "--env-file".into(),
        code_dir.join(".env").to_string_lossy().into_owned(),
        "-f".into(),
        compose,
    ])
}

async fn write_deploy_log(root: &Path, stdout: &str, stderr: &str) -> CoreResult<()> {
    let logs = root.join("logs");
    tokio::fs::create_dir_all(&logs)
        .await
        .map_err(|e| CoreError::Io(format!("creating deployment logs: {e}")))?;
    let path = logs.join("latest.log");
    tokio::fs::write(&path, format!("{stdout}{stderr}"))
        .await
        .map_err(|e| CoreError::Io(format!("writing deployment log: {e}")))?;
    #[cfg(unix)]
    tokio::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o640))
        .await
        .map_err(|e| CoreError::Io(format!("protecting deployment log: {e}")))?;
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
}
