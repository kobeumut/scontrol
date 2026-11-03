//! System user provisioning for site isolation. Each site runs under its own
//! unprivileged, shell-less system account.

use super::exec::run;
use panel_core::error::{CoreError, CoreResult};
use panel_core::types::validate::{is_valid_identifier, is_valid_username};

pub async fn create_user(username: &str, home_dir: &str) -> CoreResult<()> {
    if !is_valid_username(username) {
        return Err(CoreError::Validation(format!(
            "invalid username: {username}"
        )));
    }
    if home_dir.is_empty() || !home_dir.starts_with('/') || home_dir.contains("..") {
        return Err(CoreError::Validation(format!(
            "invalid home dir: {home_dir}"
        )));
    }
    let result = run(
        "useradd",
        &[
            "--create-home",
            "--home-dir",
            home_dir,
            "--shell",
            "/usr/sbin/nologin",
            "--user-group",
            username,
        ],
    )
    .await;

    match result {
        Ok(_) => Ok(()),
        Err(e) if e.to_string().contains("already exists") => Ok(()),
        Err(e) => Err(e),
    }
}

pub async fn delete_user(username: &str) -> CoreResult<()> {
    if !is_valid_username(username) {
        return Err(CoreError::Validation(format!(
            "invalid username: {username}"
        )));
    }
    let result = run("userdel", &["--remove", username]).await;
    match result {
        Ok(_) => Ok(()),
        Err(e) if e.to_string().contains("does not exist") => Ok(()),
        Err(e) => Err(e),
    }
}

/// Also used to validate database/system identifiers before they are
/// embedded in generated SQL/config text (never in a shell string).
pub fn ensure_identifier(ident: &str) -> CoreResult<()> {
    if !is_valid_identifier(ident) {
        return Err(CoreError::Validation(format!(
            "invalid identifier: {ident}"
        )));
    }
    Ok(())
}
