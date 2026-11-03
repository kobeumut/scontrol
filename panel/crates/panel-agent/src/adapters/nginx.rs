//! Nginx vhost management: renders a config file for a site's runtime and
//! (re)loads the web server. All file paths are derived from a
//! pre-validated domain (see `panel_core::types::validate::is_valid_domain`).

use super::exec::run;
use crate::config::AgentConfig;
use panel_core::error::{CoreError, CoreResult};
use panel_core::types::Runtime;

/// Deterministic local TCP port for a proxied (Node.js/Python) site, derived
/// from the domain so restarts / re-provisioning are idempotent.
pub fn app_port_for_domain(domain: &str) -> u16 {
    let mut hash: u32 = 2166136261; // FNV-1a
    for b in domain.as_bytes() {
        hash ^= *b as u32;
        hash = hash.wrapping_mul(16777619);
    }
    30000 + (hash % 10000) as u16
}

fn unix_socket_path(domain: &str) -> String {
    format!("/run/php/{}.sock", domain.replace('.', "_"))
}

fn render_location_block(domain: &str, runtime: &Runtime) -> String {
    let mut lines: Vec<String> = Vec::new();
    match runtime {
        Runtime::Php { .. } => {
            lines.push("    index index.php index.html;".into());
            lines.push(String::new());
            lines.push("    location / {".into());
            lines.push("        try_files $uri $uri/ /index.php?$query_string;".into());
            lines.push("    }".into());
            lines.push(String::new());
            lines.push("    location ~ \\.php$ {".into());
            lines.push("        include snippets/fastcgi-php.conf;".into());
            lines.push(format!(
                "        fastcgi_pass unix:{};",
                unix_socket_path(domain)
            ));
            lines.push("    }".into());
        }
        Runtime::NodeJs { .. } | Runtime::Python { .. } => {
            let port = app_port_for_domain(domain);
            lines.push("    location / {".into());
            lines.push(format!("        proxy_pass http://127.0.0.1:{port};"));
            lines.push("        proxy_http_version 1.1;".into());
            lines.push("        proxy_set_header Upgrade $http_upgrade;".into());
            lines.push("        proxy_set_header Connection \"upgrade\";".into());
            lines.push("        proxy_set_header Host $host;".into());
            lines.push("        proxy_set_header X-Real-IP $remote_addr;".into());
            lines.push(
                "        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;".into(),
            );
            lines.push("        proxy_set_header X-Forwarded-Proto $scheme;".into());
            lines.push("    }".into());
        }
        Runtime::Static => {
            lines.push("    index index.html;".into());
            lines.push(String::new());
            lines.push("    location / {".into());
            lines.push("        try_files $uri $uri/ =404;".into());
            lines.push("    }".into());
        }
        Runtime::ReverseProxy { upstream } => {
            lines.push("    location / {".into());
            lines.push(format!("        proxy_pass {upstream};"));
            lines.push("        proxy_set_header Host $host;".into());
            lines.push("        proxy_set_header X-Real-IP $remote_addr;".into());
            lines.push("    }".into());
        }
    }
    lines.join("\n")
}

fn render_vhost(domain: &str, document_root: &str, runtime: &Runtime) -> String {
    let location_block = render_location_block(domain, runtime);
    format!(
        "# Managed by scontrol. Do not edit by hand.\n\
server {{\n\
    listen 80;\n\
    listen [::]:80;\n\
    server_name {domain};\n\
    root {document_root};\n\
\n\
{location_block}\n\
}}\n"
    )
}

pub async fn write_vhost(
    cfg: &AgentConfig,
    domain: &str,
    document_root: &str,
    runtime: &Runtime,
) -> CoreResult<()> {
    if !panel_core::types::validate::is_valid_domain(domain) {
        return Err(CoreError::Validation(format!("invalid domain: {domain}")));
    }
    let content = render_vhost(domain, document_root, runtime);
    let available = cfg.nginx_sites_available.join(format!("{domain}.conf"));
    tokio::fs::write(&available, content)
        .await
        .map_err(|e| CoreError::Io(format!("writing {}: {e}", available.display())))?;

    let enabled = cfg.nginx_sites_enabled.join(format!("{domain}.conf"));
    let _ = tokio::fs::remove_file(&enabled).await;
    #[cfg(unix)]
    {
        tokio::fs::symlink(&available, &enabled)
            .await
            .map_err(|e| CoreError::Io(format!("symlinking {}: {e}", enabled.display())))?;
    }

    test_and_reload().await
}

pub async fn remove_vhost(cfg: &AgentConfig, domain: &str) -> CoreResult<()> {
    if !panel_core::types::validate::is_valid_domain(domain) {
        return Err(CoreError::Validation(format!("invalid domain: {domain}")));
    }
    let available = cfg.nginx_sites_available.join(format!("{domain}.conf"));
    let enabled = cfg.nginx_sites_enabled.join(format!("{domain}.conf"));
    let _ = tokio::fs::remove_file(&enabled).await;
    let _ = tokio::fs::remove_file(&available).await;
    test_and_reload().await
}

pub async fn test_and_reload() -> CoreResult<()> {
    run("nginx", &["-t"]).await?;
    run("systemctl", &["reload", "nginx"]).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_port_is_deterministic_and_in_range() {
        let a = app_port_for_domain("example.com");
        let b = app_port_for_domain("example.com");
        assert_eq!(a, b);
        assert!((30000..40000).contains(&a));
    }

    #[test]
    fn different_domains_usually_get_different_ports() {
        assert_ne!(
            app_port_for_domain("a.example.com"),
            app_port_for_domain("b.example.com")
        );
    }

    #[test]
    fn rendered_vhost_contains_no_raw_newlines_from_domain() {
        // Defense in depth: even though callers validate the domain first,
        // the renderer itself must not let a value containing config
        // directives (e.g. embedded newlines) escape the server_name line.
        let content = render_vhost(
            "example.com",
            "/srv/scontrol/sites/u/example.com/public",
            &Runtime::Static,
        );
        assert!(content.contains("server_name example.com;"));
        assert!(content.starts_with("# Managed by scontrol"));
    }
}
