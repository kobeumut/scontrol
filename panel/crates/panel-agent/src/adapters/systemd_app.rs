//! systemd unit management for Node.js / Python sites (each app runs as its
//! own unprivileged system user, bound to a deterministic local port that
//! Nginx reverse-proxies to; see `nginx::app_port_for_domain`).

use super::exec::run;
use super::nginx::app_port_for_domain;
use crate::config::AgentConfig;
use panel_core::error::{CoreError, CoreResult};
use panel_core::types::Runtime;

fn unit_name(domain: &str) -> String {
    format!("scontrol-site-{}.service", domain.replace('.', "-"))
}

fn render_unit(
    domain: &str,
    owner_username: &str,
    document_root: &str,
    exec_start: &str,
    port: u16,
) -> String {
    format!(
        "# Managed by scontrol. Do not edit by hand.\n\
[Unit]\n\
Description=scontrol site {domain}\n\
After=network.target\n\
\n\
[Service]\n\
Type=simple\n\
User={owner_username}\n\
Group={owner_username}\n\
WorkingDirectory={document_root}\n\
Environment=PORT={port}\n\
ExecStart={exec_start}\n\
Restart=on-failure\n\
RestartSec=2\n\
NoNewPrivileges=true\n\
PrivateTmp=true\n\
ProtectSystem=strict\n\
ReadWritePaths={document_root}\n\
\n\
[Install]\n\
WantedBy=multi-user.target\n"
    )
}

fn exec_start_for(runtime: &Runtime, document_root: &str) -> CoreResult<String> {
    match runtime {
        Runtime::NodeJs { entrypoint, .. } => {
            validate_entrypoint(entrypoint)?;
            Ok(format!("/usr/bin/node {document_root}/{entrypoint}"))
        }
        Runtime::Python { entrypoint, .. } => {
            validate_entrypoint(entrypoint)?;
            Ok(format!(
                "{document_root}/venv/bin/python {document_root}/{entrypoint}"
            ))
        }
        other => Err(CoreError::Validation(format!(
            "runtime {other} does not run under systemd_app"
        ))),
    }
}

fn validate_entrypoint(entrypoint: &str) -> CoreResult<()> {
    if entrypoint.is_empty()
        || entrypoint.contains("..")
        || entrypoint.starts_with('/')
        || entrypoint.contains('\0')
    {
        return Err(CoreError::Validation(format!(
            "invalid entrypoint: {entrypoint}"
        )));
    }
    Ok(())
}

pub async fn write_unit(
    cfg: &AgentConfig,
    domain: &str,
    owner_username: &str,
    document_root: &str,
    runtime: &Runtime,
) -> CoreResult<()> {
    if !panel_core::types::validate::is_valid_domain(domain) {
        return Err(CoreError::Validation(format!("invalid domain: {domain}")));
    }
    if !panel_core::types::validate::is_valid_username(owner_username) {
        return Err(CoreError::Validation(format!(
            "invalid username: {owner_username}"
        )));
    }
    let port = app_port_for_domain(domain);
    let exec_start = exec_start_for(runtime, document_root)?;
    let content = render_unit(domain, owner_username, document_root, &exec_start, port);
    let path = cfg.systemd_unit_dir.join(unit_name(domain));
    tokio::fs::write(&path, content)
        .await
        .map_err(|e| CoreError::Io(format!("writing {}: {e}", path.display())))?;

    run("systemctl", &["daemon-reload"]).await?;
    let name = unit_name(domain);
    run("systemctl", &["enable", "--now", &name]).await?;
    Ok(())
}

pub async fn remove_unit(cfg: &AgentConfig, domain: &str) -> CoreResult<()> {
    let name = unit_name(domain);
    let _ = run("systemctl", &["disable", "--now", &name]).await;
    let path = cfg.systemd_unit_dir.join(&name);
    let _ = tokio::fs::remove_file(&path).await;
    run("systemctl", &["daemon-reload"]).await?;
    Ok(())
}
