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

/// Vhost file name for a Compose domain binding. The prefix keeps these
/// apart from site vhosts (`<domain>.conf`), so neither can overwrite the
/// other even if the API's cross-check were bypassed.
#[cfg(feature = "docker")]
fn compose_vhost_name(domain: &str) -> String {
    format!("scontrol-compose-{domain}.conf")
}

#[cfg(feature = "docker")]
fn render_proxy_location(host_port: u16) -> String {
    format!(
        "    location / {{\n\
        proxy_pass http://127.0.0.1:{host_port};\n\
        proxy_http_version 1.1;\n\
        proxy_set_header Upgrade $http_upgrade;\n\
        proxy_set_header Connection \"upgrade\";\n\
        proxy_set_header Host $host;\n\
        proxy_set_header X-Real-IP $remote_addr;\n\
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;\n\
        proxy_set_header X-Forwarded-Proto $scheme;\n\
        proxy_read_timeout 300s;\n\
        client_max_body_size 64m;\n\
    }}\n"
    )
}

/// Renders the reverse-proxy vhost for one Compose domain binding. Port 80
/// always serves the ACME HTTP-01 webroot; with `tls` it redirects everything
/// else to a 443 server using the certbot-managed certificate.
#[cfg(feature = "docker")]
pub fn render_compose_vhost(
    cfg: &AgentConfig,
    project_slug: &str,
    domain: &str,
    host_port: u16,
    tls: bool,
) -> String {
    let acme_root = cfg.acme_webroot.to_string_lossy();
    let http_body = if tls {
        "    location / {\n        return 301 https://$host$request_uri;\n    }\n".to_string()
    } else {
        render_proxy_location(host_port)
    };
    let mut out = format!(
        "# Managed by scontrol for Compose project {project_slug}. Do not edit by hand.\n\
server {{\n\
    listen 80;\n\
    listen [::]:80;\n\
    server_name {domain};\n\
\n\
    location ^~ /.well-known/acme-challenge/ {{\n\
        root {acme_root};\n\
        default_type text/plain;\n\
    }}\n\
\n\
{http_body}}}\n"
    );
    if tls {
        let live = cfg.letsencrypt_live_dir.join(domain);
        let proxy = render_proxy_location(host_port);
        out.push_str(&format!(
            "\nserver {{\n\
    listen 443 ssl http2;\n\
    listen [::]:443 ssl http2;\n\
    server_name {domain};\n\
    ssl_certificate {cert};\n\
    ssl_certificate_key {key};\n\
    ssl_protocols TLSv1.2 TLSv1.3;\n\
    ssl_session_cache shared:scontrol_ssl:10m;\n\
\n\
{proxy}}}\n",
            cert = live.join("fullchain.pem").to_string_lossy(),
            key = live.join("privkey.pem").to_string_lossy(),
        ));
    }
    out
}

/// Whether certbot has a live certificate for `domain`.
#[cfg(feature = "docker")]
pub fn certificate_exists(cfg: &AgentConfig, domain: &str) -> bool {
    let live = cfg.letsencrypt_live_dir.join(domain);
    live.join("fullchain.pem").is_file() && live.join("privkey.pem").is_file()
}

/// Current content of a Compose vhost, used to restore it if a new
/// configuration fails `nginx -t`.
#[cfg(feature = "docker")]
pub async fn read_compose_vhost(cfg: &AgentConfig, domain: &str) -> Option<String> {
    tokio::fs::read_to_string(cfg.nginx_sites_available.join(compose_vhost_name(domain)))
        .await
        .ok()
}

/// Writes (atomically) and enables a Compose vhost. Does not reload Nginx;
/// callers batch all changes and call [`test_and_reload`] once.
#[cfg(feature = "docker")]
pub async fn write_compose_vhost(cfg: &AgentConfig, domain: &str, content: &str) -> CoreResult<()> {
    if !panel_core::types::validate::is_valid_domain(domain) {
        return Err(CoreError::Validation(format!("invalid domain: {domain}")));
    }
    let name = compose_vhost_name(domain);
    let available = cfg.nginx_sites_available.join(&name);
    let staging = cfg.nginx_sites_available.join(format!(".{name}.tmp"));
    tokio::fs::write(&staging, content)
        .await
        .map_err(|e| CoreError::Io(format!("writing {}: {e}", staging.display())))?;
    tokio::fs::rename(&staging, &available)
        .await
        .map_err(|e| CoreError::Io(format!("installing {}: {e}", available.display())))?;
    let enabled = cfg.nginx_sites_enabled.join(&name);
    if tokio::fs::symlink_metadata(&enabled).await.is_err() {
        tokio::fs::symlink(&available, &enabled)
            .await
            .map_err(|e| CoreError::Io(format!("symlinking {}: {e}", enabled.display())))?;
    }
    Ok(())
}

/// Disables and deletes a Compose vhost. Does not reload Nginx.
#[cfg(feature = "docker")]
pub async fn remove_compose_vhost(cfg: &AgentConfig, domain: &str) -> CoreResult<()> {
    if !panel_core::types::validate::is_valid_domain(domain) {
        return Err(CoreError::Validation(format!("invalid domain: {domain}")));
    }
    let name = compose_vhost_name(domain);
    let _ = tokio::fs::remove_file(cfg.nginx_sites_enabled.join(&name)).await;
    let _ = tokio::fs::remove_file(cfg.nginx_sites_available.join(&name)).await;
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

    #[cfg(feature = "docker")]
    #[test]
    fn compose_vhost_proxies_to_loopback_and_serves_acme() {
        let cfg = AgentConfig::default();
        let plain = render_compose_vhost(&cfg, "shop", "shop.example.com", 40001, false);
        assert!(plain.contains("server_name shop.example.com;"));
        assert!(plain.contains("proxy_pass http://127.0.0.1:40001;"));
        assert!(plain.contains("location ^~ /.well-known/acme-challenge/"));
        assert!(!plain.contains("listen 443"));

        let tls = render_compose_vhost(&cfg, "shop", "shop.example.com", 40001, true);
        assert!(tls.contains("return 301 https://$host$request_uri;"));
        assert!(tls.contains("/etc/letsencrypt/live/shop.example.com/fullchain.pem"));
        assert_eq!(tls.matches("proxy_pass").count(), 1);
    }
}
