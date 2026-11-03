//! Per-site PHP-FPM pool management.

use super::exec::run;
use panel_core::error::{CoreError, CoreResult};

fn pool_dir(version: &str) -> CoreResult<std::path::PathBuf> {
    if !version.chars().all(|c| c.is_ascii_digit() || c == '.') {
        return Err(CoreError::Validation(format!(
            "invalid PHP version: {version}"
        )));
    }
    Ok(std::path::PathBuf::from(format!(
        "/etc/php/{version}/fpm/pool.d"
    )))
}

fn socket_path(domain: &str) -> String {
    format!("/run/php/{}.sock", domain.replace('.', "_"))
}

fn render_pool(domain: &str, owner_username: &str, document_root: &str) -> String {
    format!(
        "; Managed by scontrol. Do not edit by hand.\n\
[{domain}]\n\
user = {owner_username}\n\
group = {owner_username}\n\
listen = {sock}\n\
listen.owner = www-data\n\
listen.group = www-data\n\
pm = dynamic\n\
pm.max_children = 5\n\
pm.start_servers = 2\n\
pm.min_spare_servers = 1\n\
pm.max_spare_servers = 3\n\
chdir = {document_root}\n\
php_admin_value[open_basedir] = {document_root}:/tmp\n",
        sock = socket_path(domain)
    )
}

pub async fn write_pool(
    version: &str,
    domain: &str,
    owner_username: &str,
    document_root: &str,
) -> CoreResult<()> {
    if !panel_core::types::validate::is_valid_domain(domain) {
        return Err(CoreError::Validation(format!("invalid domain: {domain}")));
    }
    if !panel_core::types::validate::is_valid_username(owner_username) {
        return Err(CoreError::Validation(format!(
            "invalid username: {owner_username}"
        )));
    }
    let dir = pool_dir(version)?;
    let path = dir.join(format!("{domain}.conf"));
    let content = render_pool(domain, owner_username, document_root);
    tokio::fs::write(&path, content)
        .await
        .map_err(|e| CoreError::Io(format!("writing {}: {e}", path.display())))?;
    restart(version).await
}

pub async fn remove_pool(version: &str, domain: &str) -> CoreResult<()> {
    let dir = pool_dir(version)?;
    let path = dir.join(format!("{domain}.conf"));
    let _ = tokio::fs::remove_file(&path).await;
    restart(version).await
}

pub async fn restart(version: &str) -> CoreResult<()> {
    let service = format!("php{version}-fpm");
    run("systemctl", &["restart", &service]).await?;
    Ok(())
}
