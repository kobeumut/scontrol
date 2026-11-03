//! Container management via the `docker` CLI (only compiled in when the
//! `docker` cargo feature is enabled, which the installer only does when the
//! operator opts into Docker at install time).

use super::exec::run;
use panel_core::error::{CoreError, CoreResult};
use panel_core::types::{validate::is_valid_identifier, ContainerInfo};

pub async fn create_container(
    name: &str,
    image: &str,
    ports: &[(u16, u16)],
    env: &[(String, String)],
) -> CoreResult<ContainerInfo> {
    if !is_valid_identifier(name) {
        return Err(CoreError::Validation(format!(
            "invalid container name: {name}"
        )));
    }
    if !is_valid_image_ref(image) {
        return Err(CoreError::Validation(format!(
            "invalid image reference: {image}"
        )));
    }

    let mut args: Vec<String> = vec!["run".into(), "-d".into(), "--name".into(), name.into()];
    for (host_port, container_port) in ports {
        args.push("-p".into());
        args.push(format!("{host_port}:{container_port}"));
    }
    for (k, v) in env {
        if !is_valid_identifier(k) {
            return Err(CoreError::Validation(format!("invalid env var name: {k}")));
        }
        args.push("-e".into());
        args.push(format!("{k}={v}"));
    }
    args.push(image.into());

    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let id = run("docker", &arg_refs).await?.trim().to_string();

    Ok(ContainerInfo {
        id,
        name: name.to_string(),
        image: image.to_string(),
        status: "running".into(),
        ports: ports.iter().map(|(h, c)| format!("{h}->{c}")).collect(),
    })
}

pub async fn start_container(name: &str) -> CoreResult<()> {
    validate_name(name)?;
    run("docker", &["start", name]).await?;
    Ok(())
}

pub async fn stop_container(name: &str) -> CoreResult<()> {
    validate_name(name)?;
    run("docker", &["stop", name]).await?;
    Ok(())
}

pub async fn remove_container(name: &str) -> CoreResult<()> {
    validate_name(name)?;
    run("docker", &["rm", "-f", name]).await?;
    Ok(())
}

pub async fn list_containers() -> CoreResult<Vec<ContainerInfo>> {
    let out = run(
        "docker",
        &[
            "ps",
            "-a",
            "--format",
            "{{.ID}}\t{{.Names}}\t{{.Image}}\t{{.Status}}\t{{.Ports}}",
        ],
    )
    .await?;
    let mut result = Vec::new();
    for line in out.lines() {
        let mut fields = line.split('\t');
        if let (Some(id), Some(name), Some(image), Some(status), ports) = (
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
        ) {
            result.push(ContainerInfo {
                id: id.to_string(),
                name: name.to_string(),
                image: image.to_string(),
                status: status.to_string(),
                ports: ports
                    .unwrap_or("")
                    .split(", ")
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect(),
            });
        }
    }
    Ok(result)
}

fn validate_name(name: &str) -> CoreResult<()> {
    if !is_valid_identifier(name) {
        return Err(CoreError::Validation(format!(
            "invalid container name: {name}"
        )));
    }
    Ok(())
}

/// Very small allow-list validator for `repo[:tag]` / `repo@sha256:digest`
/// image references, sufficient to keep obviously malformed input from ever
/// reaching `Command::new("docker")`.
fn is_valid_image_ref(image: &str) -> bool {
    !image.is_empty()
        && image.len() < 256
        && image
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "/:._-@".contains(c))
}
