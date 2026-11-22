//! Let's Encrypt certificate issuance via certbot's Nginx plugin.

use super::exec::run;
use panel_core::error::{CoreError, CoreResult};
use panel_core::types::validate::is_valid_domain;

pub async fn issue_certificate(domain: &str, admin_email: &str) -> CoreResult<()> {
    if !is_valid_domain(domain) {
        return Err(CoreError::Validation(format!("invalid domain: {domain}")));
    }
    if !admin_email.contains('@') || admin_email.len() > 254 {
        return Err(CoreError::Validation(format!(
            "invalid email: {admin_email}"
        )));
    }
    let _guard = super::nginx::CONFIG_LOCK.lock().await;
    run(
        "certbot",
        &[
            "--nginx",
            "-d",
            domain,
            "--non-interactive",
            "--agree-tos",
            "-m",
            admin_email,
            "--redirect",
        ],
    )
    .await?;
    Ok(())
}
