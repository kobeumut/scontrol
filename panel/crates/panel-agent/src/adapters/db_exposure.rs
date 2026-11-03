//! Outside access to database services.
//!
//! Database containers publish their port on `127.0.0.1` only. Docker-published
//! ports bypass UFW (Docker writes its own iptables chains), so exposing a
//! database by publishing it on `0.0.0.0` would ignore the firewall. Instead
//! the host Nginx listens on the public port with a generated `stream` server
//! that proxies to the loopback port, and UFW allows that port, from the
//! configured sources only. Nginx also enforces the same sources with
//! `allow`/`deny`, so a disabled or misconfigured UFW still does not open the
//! database to everyone.
//!
//! The applied exposure is recorded in `<project root>/exposure.json` so it
//! can be replaced or removed (including when the project is removed).

use super::exec::run;
use crate::config::AgentConfig;
use panel_core::adapters::SetDatabaseExposureParams;
use panel_core::database_services::{normalize_source, DATABASE_LOOPBACK_PORTS};
use panel_core::error::{CoreError, CoreResult};
use panel_core::types::{validate, DatabaseExposure};
use std::path::{Path, PathBuf};

const STATE_FILE: &str = "exposure.json";

pub async fn set(cfg: &AgentConfig, params: SetDatabaseExposureParams) -> CoreResult<()> {
    if !validate::is_valid_project_slug(&params.project_slug) {
        return Err(CoreError::Validation(format!(
            "invalid project slug: {}",
            params.project_slug
        )));
    }
    if !DATABASE_LOOPBACK_PORTS.contains(&params.loopback_port) {
        return Err(CoreError::Validation(
            "invalid database loopback port".into(),
        ));
    }
    let desired = match params.exposure {
        Some(mut exposure) => {
            exposure.normalize().map_err(CoreError::Validation)?;
            Some(exposure)
        }
        None => None,
    };
    let root = cfg.docker_projects_root.join(&params.project_slug);
    apply(
        cfg,
        &root,
        &params.project_slug,
        params.loopback_port,
        desired.as_ref(),
    )
    .await
}

/// Removes whatever exposure the project has (used on project removal).
pub async fn clear(cfg: &AgentConfig, root: &Path, slug: &str) -> CoreResult<()> {
    if read_state(root).await.is_none() && !stream_file(cfg, slug).exists() {
        return Ok(());
    }
    apply(cfg, root, slug, 0, None).await
}

async fn apply(
    cfg: &AgentConfig,
    root: &Path,
    slug: &str,
    loopback_port: u16,
    desired: Option<&DatabaseExposure>,
) -> CoreResult<()> {
    let previous = read_state(root).await;
    let path = stream_file(cfg, slug);
    let backup = tokio::fs::read_to_string(&path).await.ok();

    match desired {
        Some(exposure) => {
            tokio::fs::create_dir_all(&cfg.nginx_stream_dir)
                .await
                .map_err(|e| CoreError::Io(format!("creating Nginx stream directory: {e}")))?;
            let ipv6 = Path::new("/proc/net/if_inet6").exists();
            write_atomic(
                &path,
                &render_stream_server(slug, loopback_port, exposure, ipv6),
            )
            .await?;
            if let Err(error) = check_and_reload(&path).await {
                restore(&path, backup.as_deref()).await;
                return Err(error);
            }
        }
        None => {
            if backup.is_some() {
                tokio::fs::remove_file(&path)
                    .await
                    .map_err(|e| CoreError::Io(format!("removing stream server: {e}")))?;
                run("nginx", &["-t"]).await?;
                run("systemctl", &["reload", "nginx"]).await?;
            }
        }
    }

    // Firewall: drop rules that are no longer wanted, then add the new ones
    // (UFW skips a rule that already exists).
    let old_rules = previous.as_ref().map(ufw_rules).unwrap_or_default();
    let new_rules = desired.map(ufw_rules).unwrap_or_default();
    for rule in old_rules.iter().filter(|r| !new_rules.contains(r)) {
        let mut args = vec!["delete", "allow"];
        args.extend(rule.iter().map(String::as_str));
        if let Err(error) = run("ufw", &args).await {
            tracing::warn!(%error, "removing a database firewall rule failed");
        }
    }
    let comment = format!("scontrol-db-{slug}");
    for rule in &new_rules {
        let mut args = vec!["allow"];
        args.extend(rule.iter().map(String::as_str));
        args.extend(["comment", comment.as_str()]);
        run("ufw", &args).await?;
    }

    match desired {
        Some(exposure) => {
            let body = serde_json::to_string(exposure)
                .map_err(|e| CoreError::Io(format!("encoding exposure: {e}")))?;
            tokio::fs::create_dir_all(root)
                .await
                .map_err(|e| CoreError::Io(format!("creating project root: {e}")))?;
            tokio::fs::write(root.join(STATE_FILE), body)
                .await
                .map_err(|e| CoreError::Io(format!("recording exposure: {e}")))?;
        }
        None => {
            tokio::fs::remove_file(root.join(STATE_FILE)).await.ok();
        }
    }
    Ok(())
}

fn stream_file(cfg: &AgentConfig, slug: &str) -> PathBuf {
    cfg.nginx_stream_dir
        .join(format!("scontrol-db-{slug}.conf"))
}

async fn read_state(root: &Path) -> Option<DatabaseExposure> {
    let body = tokio::fs::read_to_string(root.join(STATE_FILE))
        .await
        .ok()?;
    let mut exposure: DatabaseExposure = serde_json::from_str(&body).ok()?;
    exposure.normalize().ok()?;
    Some(exposure)
}

/// `nginx -T` both tests the configuration and prints every file it loaded,
/// which proves the stream directory is actually included (otherwise the
/// file would be silently ignored).
async fn check_and_reload(path: &Path) -> CoreResult<()> {
    let dump = run("nginx", &["-T"]).await?;
    let marker = format!("# configuration file {}:", path.display());
    if !dump.lines().any(|line| line.trim() == marker) {
        return Err(CoreError::External(
            "the Nginx stream module or the scontrol stream include is not configured; \
             re-run the installer with Docker support"
                .into(),
        ));
    }
    run("systemctl", &["reload", "nginx"]).await?;
    Ok(())
}

async fn restore(path: &Path, backup: Option<&str>) {
    match backup {
        Some(body) => {
            write_atomic(path, body).await.ok();
        }
        None => {
            tokio::fs::remove_file(path).await.ok();
        }
    }
}

async fn write_atomic(path: &Path, body: &str) -> CoreResult<()> {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let staging = path.with_file_name(format!(".{name}.tmp"));
    tokio::fs::write(&staging, body)
        .await
        .map_err(|e| CoreError::Io(format!("writing stream server: {e}")))?;
    tokio::fs::rename(&staging, path)
        .await
        .map_err(|e| CoreError::Io(format!("installing stream server: {e}")))
}

/// Renders the Nginx `stream` server. Every interpolated value is a number,
/// a validated slug, or a source re-parsed as an IP address or CIDR.
pub(crate) fn render_stream_server(
    slug: &str,
    loopback_port: u16,
    exposure: &DatabaseExposure,
    ipv6: bool,
) -> String {
    let port = exposure.public_port;
    let mut out = format!(
        "# Generated by scontrol for database service {slug}; do not edit.\nserver {{\n    listen {port};\n"
    );
    if ipv6 {
        out.push_str(&format!("    listen [::]:{port};\n"));
    }
    let sources: Vec<String> = exposure
        .allow_from
        .iter()
        .filter_map(|s| normalize_source(s))
        .collect();
    if !sources.is_empty() {
        for source in &sources {
            out.push_str(&format!("    allow {source};\n"));
        }
        out.push_str("    deny all;\n");
    }
    out.push_str(&format!(
        "    proxy_pass 127.0.0.1:{loopback_port};\n    proxy_connect_timeout 10s;\n    proxy_timeout 1h;\n}}\n"
    ));
    out
}

/// UFW rule specs (after `allow`/`delete allow`), one per source.
pub(crate) fn ufw_rules(exposure: &DatabaseExposure) -> Vec<Vec<String>> {
    let port = exposure.public_port.to_string();
    let sources: Vec<String> = if exposure.allow_from.is_empty() {
        vec!["any".into()]
    } else {
        exposure
            .allow_from
            .iter()
            .filter_map(|s| normalize_source(s))
            .collect()
    };
    sources
        .into_iter()
        .map(|source| {
            vec![
                "proto".into(),
                "tcp".into(),
                "from".into(),
                source,
                "to".into(),
                "any".into(),
                "port".into(),
                port.clone(),
            ]
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exposure(sources: &[&str]) -> DatabaseExposure {
        DatabaseExposure {
            public_port: 15432,
            allow_from: sources.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn stream_server_proxies_to_loopback_and_restricts_sources() {
        let conf = render_stream_server(
            "shop-db",
            25001,
            &exposure(&["203.0.113.4", "10.0.0.0/8"]),
            true,
        );
        assert!(conf.contains("listen 15432;\n"));
        assert!(conf.contains("listen [::]:15432;\n"));
        assert!(conf.contains("allow 203.0.113.4;\n    allow 10.0.0.0/8;\n    deny all;\n"));
        assert!(conf.contains("proxy_pass 127.0.0.1:25001;"));
        assert!(!conf.contains("listen 0.0.0.0"));

        let open = render_stream_server("shop-db", 25001, &exposure(&[]), false);
        assert!(!open.contains("allow") && !open.contains("deny"));
        assert!(!open.contains("[::]"));
    }

    #[test]
    fn hostile_sources_never_reach_the_config_or_firewall() {
        let hostile = exposure(&["1.2.3.4; } server { listen 22", "10.0.0.0/8"]);
        let conf = render_stream_server("shop-db", 25001, &hostile, false);
        assert!(!conf.contains("listen 22"));
        let rules = ufw_rules(&hostile);
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0][3], "10.0.0.0/8");
    }

    #[test]
    fn firewall_rules_are_one_per_source_or_any() {
        let rules = ufw_rules(&exposure(&[]));
        assert_eq!(
            rules,
            vec![
                vec!["proto", "tcp", "from", "any", "to", "any", "port", "15432"]
                    .into_iter()
                    .map(String::from)
                    .collect::<Vec<_>>()
            ]
        );
        assert_eq!(
            ufw_rules(&exposure(&["10.0.0.1", "2001:db8::/32"])).len(),
            2
        );
    }

    #[tokio::test]
    async fn invalid_requests_are_refused_before_touching_the_host() {
        let cfg = AgentConfig::default();
        let bad_port = SetDatabaseExposureParams {
            project_slug: "shop-db".into(),
            loopback_port: 5432,
            exposure: None,
        };
        assert!(matches!(
            set(&cfg, bad_port).await,
            Err(CoreError::Validation(_))
        ));
        let bad_slug = SetDatabaseExposureParams {
            project_slug: "../etc".into(),
            loopback_port: 25000,
            exposure: None,
        };
        assert!(matches!(
            set(&cfg, bad_slug).await,
            Err(CoreError::Validation(_))
        ));
        let bad_public = SetDatabaseExposureParams {
            project_slug: "shop-db".into(),
            loopback_port: 25000,
            exposure: Some(DatabaseExposure {
                public_port: 22,
                allow_from: vec![],
            }),
        };
        assert!(matches!(
            set(&cfg, bad_public).await,
            Err(CoreError::Validation(_))
        ));
    }
}
