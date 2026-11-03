//! Per-user crontab management. Each site's cron jobs live in the crontab of
//! the site's own system user (never root), keyed by job UUID so entries can
//! be added/updated/removed idempotently.

use super::exec::{run, run_allow_nonzero};
use panel_core::error::{CoreError, CoreResult};
use panel_core::types::validate::is_valid_username;
use uuid::Uuid;

fn marker(job_id: Uuid) -> String {
    format!("# scontrol:{job_id}")
}

async fn read_crontab(username: &str) -> CoreResult<Vec<String>> {
    let (code, stdout, stderr) = run_allow_nonzero("crontab", &["-u", username, "-l"]).await?;
    if code == 0 {
        Ok(stdout.lines().map(str::to_string).collect())
    } else if stderr.to_lowercase().contains("no crontab") {
        Ok(vec![])
    } else {
        Err(CoreError::External(format!(
            "reading crontab for {username}: {stderr}"
        )))
    }
}

async fn write_crontab(username: &str, lines: &[String]) -> CoreResult<()> {
    let mut content = lines.join("\n");
    if !content.is_empty() {
        content.push('\n');
    }
    let tmp_path = std::env::temp_dir().join(format!("scontrol-cron-{username}.tmp"));
    tokio::fs::write(&tmp_path, content)
        .await
        .map_err(|e| CoreError::Io(format!("writing temp crontab: {e}")))?;
    let path_str = tmp_path.to_string_lossy().into_owned();
    let result = run("crontab", &["-u", username, &path_str]).await;
    let _ = tokio::fs::remove_file(&tmp_path).await;
    result.map(|_| ())
}

pub async fn upsert_job(
    owner_username: &str,
    job_id: Uuid,
    schedule: &str,
    command: &str,
    enabled: bool,
) -> CoreResult<()> {
    if !is_valid_username(owner_username) {
        return Err(CoreError::Validation(format!(
            "invalid username: {owner_username}"
        )));
    }
    if schedule.split_whitespace().count() != 5 {
        return Err(CoreError::Validation(format!(
            "invalid cron schedule: {schedule}"
        )));
    }
    if command.contains('\n') {
        return Err(CoreError::Validation(
            "cron command must be single-line".into(),
        ));
    }

    let mark = marker(job_id);
    let mut lines = read_crontab(owner_username).await?;
    lines.retain(|l| l != &mark);
    if enabled {
        lines.push(format!("{schedule} {command} {mark}"));
    }
    write_crontab(owner_username, &lines).await
}

pub async fn remove_job(owner_username: &str, job_id: Uuid) -> CoreResult<()> {
    if !is_valid_username(owner_username) {
        return Err(CoreError::Validation(format!(
            "invalid username: {owner_username}"
        )));
    }
    let mark = marker(job_id);
    let mut lines = read_crontab(owner_username).await?;
    lines.retain(|l| l != &mark);
    write_crontab(owner_username, &lines).await
}
