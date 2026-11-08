//! Scheduled jobs: generated systemd timers that run one exec-form command
//! in a Compose service (`docker compose exec -T <service> <argv...>`).
//!
//! Each job gets two units in `systemd_unit_dir`:
//!
//! ```text
//! scontrol-job-<id>.timer     OnCalendar= lines (time zone included)
//! scontrol-job-<id>.service   Type=oneshot, ExecStart=<agent> --config <agent.toml>
//!                             run-scheduled-job <slug> <id>
//! ```
//!
//! The service runs this binary in helper mode ([`execute`]), never a shell
//! or a command line: the job's argv lives in a 0600 `job.json` and is read
//! back and validated again by the helper. Every value written into a unit
//! is a UUID, a validated slug, a checked path, or an `OnCalendar=` value
//! limited to calendar characters, so nothing can add a unit line.
//!
//! Per job, inside the project directory (0700):
//!
//! ```text
//! jobs/<id>/job.json            0600 typed command, service, timeout
//! jobs/<id>/manual-trigger      present while a "run now" is starting
//! jobs/<id>/runs/<run>.json     run record (status, times, exit code)
//! jobs/<id>/runs/<run>.log      streamed output, masked and bounded
//! ```
//!
//! systemd never starts a second instance of an active service. Stopping
//! the `docker compose exec` client does not stop the process it started
//! inside the container, though, so every run carries `SCONTROL_JOB=<id>`
//! and `SCONTROL_JOB_RUN=<run>` in its environment, and the agent stops the
//! container processes carrying the job's marker when a run times out,
//! before the next run starts, and when the job is removed
//! ([`stop_job_processes`]). Runs of one job therefore never overlap.
//! panel-api polls run records ([`list_runs`]) into PostgreSQL and notifies
//! on failures.

use super::compose;
use super::deploy_log::{self, DeployLog};
use super::exec::{run, run_allow_nonzero, run_streamed};
use crate::config::AgentConfig;
use chrono::{DateTime, Utc};
use panel_core::adapters::{
    ListScheduledJobRunsParams, ScheduledJobRef, ScheduledJobRunLogParams, UpsertScheduledJobParams,
};
use panel_core::error::{CoreError, CoreResult};
use panel_core::schedule::{
    validate_job_command, JobRunInfo, JobRunStatus, JobRunTrigger, JOB_LOG_MAX_BYTES,
    JOB_RUNS_RETAINED,
};
use panel_core::types::{validate, DeploymentLogChunk};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;
use uuid::Uuid;

/// Subcommand the generated service runs (see `main`).
pub const HELPER_COMMAND: &str = "run-scheduled-job";

const JOBS_DIR: &str = "jobs";
const JOB_FILE: &str = "job.json";
const RUNS_DIR: &str = "runs";
const MANUAL_MARKER: &str = "manual-trigger";
const ZONEINFO_DIR: &str = "/usr/share/zoneinfo";
/// Extra time systemd allows the helper beyond the job's own timeout, so
/// the helper's process-group stop and final record write come first.
const UNIT_TIMEOUT_GRACE_SECS: u32 = 60;
/// Most run records one listing returns.
const MAX_LISTED_RUNS: usize = 2_000;
/// Environment variables marking a run's processes inside the container.
const JOB_MARKER: &str = "SCONTROL_JOB";
const RUN_MARKER: &str = "SCONTROL_JOB_RUN";
/// Where host PIDs of container processes (from `docker top`) are read.
const PROC_ROOT: &str = "/proc";
/// Time marked processes get to exit after SIGTERM before SIGKILL.
const STOP_GRACE: Duration = Duration::from_secs(10);

/// What the helper reads back when the timer fires.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct JobFile {
    job_id: Uuid,
    project_slug: String,
    service_name: String,
    command: Vec<String>,
    timeout_secs: u32,
}

fn unit_names(job_id: Uuid) -> (String, String) {
    (
        format!("scontrol-job-{job_id}.service"),
        format!("scontrol-job-{job_id}.timer"),
    )
}

fn check_slug(slug: &str) -> CoreResult<()> {
    if validate::is_valid_project_slug(slug) {
        Ok(())
    } else {
        Err(CoreError::Validation(format!(
            "invalid project slug: {slug}"
        )))
    }
}

fn jobs_root(cfg: &AgentConfig, slug: &str) -> PathBuf {
    compose::project_root(cfg, slug).join(JOBS_DIR)
}

fn job_dir(cfg: &AgentConfig, slug: &str, job_id: Uuid) -> PathBuf {
    jobs_root(cfg, slug).join(job_id.to_string())
}

/// An absolute path that can appear in `ExecStart=` as is: no spaces,
/// quotes, `%` specifiers, `$` expansion, or backslash escapes.
fn unit_safe_path(path: &Path) -> CoreResult<&str> {
    let text = path
        .to_str()
        .filter(|p| p.starts_with('/'))
        .filter(|p| {
            p.chars()
                .all(|c| c.is_ascii_alphanumeric() || "/._-".contains(c))
        })
        .filter(|p| !p.split('/').any(|segment| segment == ".."));
    text.ok_or_else(|| {
        CoreError::Validation(format!(
            "{} cannot be used in a systemd unit (absolute path of letters, digits, '/', '.', '_' and '-' only)",
            path.display()
        ))
    })
}

fn render_service_unit(
    agent_binary: &Path,
    config_path: &Path,
    slug: &str,
    job_id: Uuid,
    timeout_secs: u32,
) -> CoreResult<String> {
    check_slug(slug)?;
    let binary = unit_safe_path(agent_binary)?;
    let config = unit_safe_path(config_path)?;
    let timeout = timeout_secs + UNIT_TIMEOUT_GRACE_SECS;
    Ok(format!(
        "# Managed by scontrol. Do not edit by hand.\n\
[Unit]\n\
Description=scontrol scheduled job {job_id}\n\
After=docker.service\n\
\n\
[Service]\n\
Type=oneshot\n\
ExecStart={binary} --config {config} {HELPER_COMMAND} {slug} {job_id}\n\
TimeoutStartSec={timeout}\n\
NoNewPrivileges=yes\n\
PrivateTmp=yes\n\
ProtectSystem=full\n"
    ))
}

fn render_timer_unit(job_id: Uuid, on_calendar: &[String]) -> CoreResult<String> {
    if on_calendar.is_empty() {
        return Err(CoreError::Validation("a timer needs a schedule".into()));
    }
    let mut lines = String::new();
    for expression in on_calendar {
        if !is_calendar_value(expression) {
            return Err(CoreError::Validation(format!(
                "invalid calendar expression: {expression}"
            )));
        }
        lines.push_str(&format!("OnCalendar={expression}\n"));
    }
    let (service, _) = unit_names(job_id);
    Ok(format!(
        "# Managed by scontrol. Do not edit by hand.\n\
[Unit]\n\
Description=scontrol scheduled job {job_id} timer\n\
\n\
[Timer]\n\
{lines}AccuracySec=1s\n\
Unit={service}\n\
\n\
[Install]\n\
WantedBy=timers.target\n"
    ))
}

/// An `OnCalendar=` value from `JobSchedule::on_calendar`: a calendar
/// expression plus a time zone, both from narrow character sets.
fn is_calendar_value(value: &str) -> bool {
    match value.rsplit_once(' ') {
        Some((expression, timezone)) => {
            panel_core::schedule::is_valid_calendar_expression(expression)
                && panel_core::schedule::is_valid_timezone(timezone)
        }
        None => false,
    }
}

/// Replaces `dir/name` with `body` (0644) through a temporary file and a
/// rename, so systemd never reads a half-written unit and a symlink at the
/// destination is replaced, not followed.
async fn write_unit_file(dir: &Path, name: &str, body: &str) -> CoreResult<()> {
    use tokio::io::AsyncWriteExt;

    let path = dir.join(name);
    let tmp = dir.join(format!(".{name}.tmp"));
    let _ = tokio::fs::remove_file(&tmp).await;
    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o644);
    let mut file = options
        .open(&tmp)
        .await
        .map_err(|e| CoreError::Io(format!("writing {}: {e}", tmp.display())))?;
    file.write_all(body.as_bytes())
        .await
        .map_err(|e| CoreError::Io(format!("writing {}: {e}", tmp.display())))?;
    file.sync_all()
        .await
        .map_err(|e| CoreError::Io(format!("writing {}: {e}", tmp.display())))?;
    drop(file);
    // The agent runs with `UMask=0007`; units are world-readable like
    // the ones packages install.
    #[cfg(unix)]
    tokio::fs::set_permissions(&tmp, std::os::unix::fs::PermissionsExt::from_mode(0o644))
        .await
        .map_err(|e| CoreError::Io(format!("writing {}: {e}", tmp.display())))?;
    tokio::fs::rename(&tmp, &path)
        .await
        .map_err(|e| CoreError::Io(format!("installing {}: {e}", path.display())))
}

/// Removes both units of a job; `true` if a file was there.
async fn remove_unit_files(dir: &Path, job_id: Uuid) -> CoreResult<bool> {
    let (service, timer) = unit_names(job_id);
    let mut removed = false;
    for name in [timer, service] {
        match tokio::fs::remove_file(dir.join(&name)).await {
            Ok(()) => removed = true,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(CoreError::Io(format!("removing {name}: {e}"))),
        }
    }
    Ok(removed)
}

/// Writes `value` as JSON to `path` through a rename (0600).
async fn write_json_atomically<T: Serialize>(path: &Path, value: &T) -> CoreResult<()> {
    let body = serde_json::to_string(value)
        .map_err(|e| CoreError::Io(format!("encoding {}: {e}", path.display())))?;
    let tmp = path.with_extension("json.tmp");
    compose::write_private_file(&tmp, &body).await?;
    tokio::fs::rename(&tmp, path)
        .await
        .map_err(|e| CoreError::Io(format!("writing {}: {e}", path.display())))
}

/// Refuses a time zone the host's tz database does not have, without
/// letting the name walk out of the zoneinfo directory.
fn check_timezone_installed(timezone: &str) -> CoreResult<()> {
    if timezone == "UTC" {
        return Ok(());
    }
    if !panel_core::schedule::is_valid_timezone(timezone)
        || !Path::new(ZONEINFO_DIR).join(timezone).is_file()
    {
        return Err(CoreError::Validation(format!(
            "unknown time zone: {timezone}"
        )));
    }
    Ok(())
}

/// Installs (or replaces) a job's definition and units, then enables or
/// stops its timer.
pub async fn upsert(cfg: &AgentConfig, params: UpsertScheduledJobParams) -> CoreResult<()> {
    check_slug(&params.project_slug)?;
    let on_calendar = params.spec.validate().map_err(CoreError::Validation)?;
    if compose::is_static_project(cfg, &params.project_slug) {
        return Err(CoreError::Validation(
            "a static site runs no container to execute a job in".into(),
        ));
    }
    check_timezone_installed(&params.spec.timezone)?;
    for expression in &on_calendar {
        // systemd's own parser has the final word on the grammar.
        run("systemd-analyze", &["calendar", "--", expression])
            .await
            .map_err(|_| {
                CoreError::Validation(format!("systemd rejected the schedule {expression:?}"))
            })?;
    }

    let agent_binary = std::env::current_exe()
        .map_err(|e| CoreError::Io(format!("locating the agent binary: {e}")))?;
    let service = render_service_unit(
        &agent_binary,
        &cfg.config_path,
        &params.project_slug,
        params.job_id,
        params.spec.timeout_secs,
    )?;
    let timer = render_timer_unit(params.job_id, &on_calendar)?;

    let dir = job_dir(cfg, &params.project_slug, params.job_id);
    compose::create_private_dir(&compose::project_root(cfg, &params.project_slug)).await?;
    compose::create_private_dir(&jobs_root(cfg, &params.project_slug)).await?;
    compose::create_private_dir(&dir).await?;
    let job = JobFile {
        job_id: params.job_id,
        project_slug: params.project_slug.clone(),
        service_name: params.spec.service_name.clone(),
        command: params.spec.command.clone(),
        timeout_secs: params.spec.timeout_secs,
    };
    write_json_atomically(&dir.join(JOB_FILE), &job).await?;

    let (service_name, timer_name) = unit_names(params.job_id);
    write_unit_file(&cfg.systemd_unit_dir, &service_name, &service).await?;
    write_unit_file(&cfg.systemd_unit_dir, &timer_name, &timer).await?;
    run("systemctl", &["daemon-reload"]).await?;
    if params.enabled {
        run("systemctl", &["enable", &timer_name]).await?;
        // Restart so a changed schedule takes effect now.
        run("systemctl", &["restart", &timer_name]).await?;
    } else {
        run("systemctl", &["disable", "--now", &timer_name]).await?;
    }
    Ok(())
}

/// Stops a job, removes its units and its directory (records and logs).
/// Removing a job that does not exist succeeds.
pub async fn remove(cfg: &AgentConfig, params: ScheduledJobRef) -> CoreResult<()> {
    check_slug(&params.project_slug)?;
    remove_job(cfg, &params.project_slug, params.job_id).await
}

async fn remove_job(cfg: &AgentConfig, slug: &str, job_id: Uuid) -> CoreResult<()> {
    let (service_name, timer_name) = unit_names(job_id);
    let installed = cfg.systemd_unit_dir.join(&timer_name).exists()
        || cfg.systemd_unit_dir.join(&service_name).exists();
    if installed {
        let _ = run_allow_nonzero("systemctl", &["disable", "--now", &timer_name]).await;
        let _ = run_allow_nonzero("systemctl", &["stop", &service_name]).await;
    }
    if remove_unit_files(&cfg.systemd_unit_dir, job_id).await? {
        run("systemctl", &["daemon-reload"]).await?;
        let _ = run_allow_nonzero("systemctl", &["reset-failed", &service_name]).await;
    }
    let dir = job_dir(cfg, slug, job_id);
    if let Ok(job) = read_job_file(&dir).await {
        // A run in progress outlives its stopped service inside the container.
        if let Err(error) = stop_job_processes(cfg, &job).await {
            tracing::warn!(%job_id, %error, "stopping job processes failed");
        }
    }
    if dir.exists() {
        tokio::fs::remove_dir_all(&dir)
            .await
            .map_err(|e| CoreError::Io(format!("removing job files: {e}")))?;
    }
    Ok(())
}

/// Removes every job of a project, before the project itself is removed.
pub async fn remove_project_jobs(cfg: &AgentConfig, slug: &str) -> CoreResult<()> {
    for job_id in job_ids(&jobs_root(cfg, slug)).await? {
        remove_job(cfg, slug, job_id).await?;
    }
    Ok(())
}

async fn job_ids(root: &Path) -> CoreResult<Vec<Uuid>> {
    let mut entries = match tokio::fs::read_dir(root).await {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(CoreError::Io(format!("listing jobs: {e}"))),
    };
    let mut ids = Vec::new();
    while let Some(entry) = entries
        .next_entry()
        .await
        .map_err(|e| CoreError::Io(format!("listing jobs: {e}")))?
    {
        if let Some(id) = entry.file_name().to_str().and_then(|n| n.parse().ok()) {
            ids.push(id);
        }
    }
    Ok(ids)
}

async fn service_is_active(service_name: &str) -> CoreResult<bool> {
    let (code, _, _) =
        run_allow_nonzero("systemctl", &["is-active", "--quiet", service_name]).await?;
    Ok(code == 0)
}

/// Starts a run now through the job's own service, so it can never overlap
/// a timer run. The helper reads the marker to record the run as manual.
pub async fn run_now(cfg: &AgentConfig, params: ScheduledJobRef) -> CoreResult<()> {
    check_slug(&params.project_slug)?;
    let dir = job_dir(cfg, &params.project_slug, params.job_id);
    if !dir.join(JOB_FILE).is_file() {
        return Err(CoreError::NotFound("scheduled job is not installed".into()));
    }
    let (service_name, _) = unit_names(params.job_id);
    if service_is_active(&service_name).await? {
        return Err(CoreError::AlreadyExists(
            "a run of this job is already in progress".into(),
        ));
    }
    compose::write_private_file(&dir.join(MANUAL_MARKER), "").await?;
    run("systemctl", &["start", "--no-block", &service_name]).await?;
    Ok(())
}

fn record_path(runs_dir: &Path, run_id: Uuid) -> PathBuf {
    runs_dir.join(format!("{run_id}.json"))
}

/// Run records in `runs_dir` changed after `since`, oldest first.
async fn read_records(
    runs_dir: &Path,
    since: Option<DateTime<Utc>>,
) -> CoreResult<Vec<JobRunInfo>> {
    let mut entries = match tokio::fs::read_dir(runs_dir).await {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(CoreError::Io(format!("listing job runs: {e}"))),
    };
    // Coarse file times must not hide a record written in the same tick.
    let cutoff = since.map(|s| std::time::SystemTime::from(s) - Duration::from_secs(2));
    let mut records = Vec::new();
    while let Some(entry) = entries
        .next_entry()
        .await
        .map_err(|e| CoreError::Io(format!("listing job runs: {e}")))?
    {
        let path = entry.path();
        let is_record = path.extension().is_some_and(|e| e == "json")
            && path
                .file_stem()
                .and_then(|s| s.to_str())
                .is_some_and(|s| Uuid::parse_str(s).is_ok());
        if !is_record {
            continue;
        }
        if let (Some(cutoff), Ok(modified)) =
            (cutoff, entry.metadata().await.and_then(|m| m.modified()))
        {
            if modified < cutoff {
                continue;
            }
        }
        let Ok(body) = tokio::fs::read(&path).await else {
            continue;
        };
        if let Ok(record) = serde_json::from_slice::<JobRunInfo>(&body) {
            if since.is_none_or(|since| record.updated_at > since) {
                records.push(record);
            }
        }
    }
    records.sort_by_key(|r| r.started_at);
    Ok(records)
}

/// Run records of the given jobs. A record still marked running whose
/// service is no longer active belongs to a helper that was killed (reboot,
/// systemd timeout), so it is closed as failed.
pub async fn list_runs(
    cfg: &AgentConfig,
    params: ListScheduledJobRunsParams,
) -> CoreResult<Vec<JobRunInfo>> {
    let mut runs = Vec::new();
    for job in &params.jobs {
        check_slug(&job.project_slug)?;
        let runs_dir = job_dir(cfg, &job.project_slug, job.job_id).join(RUNS_DIR);
        for record in read_records(&runs_dir, params.since).await? {
            if record.job_id != job.job_id {
                continue;
            }
            let record = if record.status == JobRunStatus::Running {
                close_if_abandoned(&runs_dir, record).await?
            } else {
                record
            };
            runs.push(record);
        }
        if runs.len() >= MAX_LISTED_RUNS {
            runs.truncate(MAX_LISTED_RUNS);
            break;
        }
    }
    Ok(runs)
}

async fn close_if_abandoned(runs_dir: &Path, record: JobRunInfo) -> CoreResult<JobRunInfo> {
    let (service_name, _) = unit_names(record.job_id);
    if service_is_active(&service_name).await? {
        return Ok(record);
    }
    // The helper writes its final record before it exits, so a record
    // read again after the service stopped is final.
    let path = record_path(runs_dir, record.id);
    let current: JobRunInfo = match tokio::fs::read(&path).await {
        Ok(body) => serde_json::from_slice(&body).unwrap_or(record),
        Err(_) => return Ok(record),
    };
    if current.status != JobRunStatus::Running {
        return Ok(current);
    }
    let closed = interrupted(current);
    write_json_atomically(&path, &closed).await?;
    Ok(closed)
}

fn interrupted(mut record: JobRunInfo) -> JobRunInfo {
    let now = Utc::now();
    record.status = JobRunStatus::Failed;
    record.finished_at = Some(now);
    record.updated_at = now;
    record.error_message = Some("the run was interrupted before it finished".into());
    record
}

pub async fn read_run_log(
    cfg: &AgentConfig,
    params: ScheduledJobRunLogParams,
) -> CoreResult<DeploymentLogChunk> {
    check_slug(&params.project_slug)?;
    let runs_dir = job_dir(cfg, &params.project_slug, params.job_id).join(RUNS_DIR);
    deploy_log::read_chunk(&runs_dir, params.run_id, params.offset, params.max_bytes).await
}

/// Keeps the `keep` most recent runs (record and log) and removes the rest.
async fn prune_runs(runs_dir: &Path, keep: usize) -> CoreResult<()> {
    let mut records = read_records(runs_dir, None).await?;
    if records.len() <= keep {
        return Ok(());
    }
    records.sort_by_key(|r| std::cmp::Reverse(r.started_at));
    for record in records.into_iter().skip(keep) {
        tokio::fs::remove_file(record_path(runs_dir, record.id))
            .await
            .ok();
        tokio::fs::remove_file(deploy_log::log_path(runs_dir, record.id))
            .await
            .ok();
    }
    Ok(())
}

/// Shown at the top of a run's log: the argv, one quoted value per
/// argument, so the boundaries between arguments are visible.
fn describe_command(service: &str, command: &[String]) -> String {
    let argv: Vec<String> = command.iter().map(|arg| format!("{arg:?}")).collect();
    format!("exec in service {service}: [{}]", argv.join(", "))
}

/// Helper mode: one run of a job, started by its systemd service. Records
/// the run, streams its masked output into the run log, and returns the
/// final status (the process exits non-zero on failure, so systemd shows
/// the unit as failed too).
async fn read_job_file(dir: &Path) -> CoreResult<JobFile> {
    let body = tokio::fs::read(dir.join(JOB_FILE))
        .await
        .map_err(|e| CoreError::NotFound(format!("reading job definition: {e}")))?;
    serde_json::from_slice(&body)
        .map_err(|e| CoreError::Validation(format!("invalid job definition: {e}")))
}

/// Host PIDs in `docker top <container> -o pid` output.
fn parse_top_pids(output: &str) -> Vec<u32> {
    output
        .lines()
        .filter_map(|line| line.trim().parse().ok())
        .filter(|pid| *pid > 1)
        .collect()
}

/// The PIDs whose environment (`<proc_root>/<pid>/environ`) contains
/// `marker` (`NAME=value`) as one whole entry.
async fn marked_processes(proc_root: &Path, pids: &[u32], marker: &str) -> Vec<u32> {
    let mut marked = Vec::new();
    for pid in pids {
        let Ok(environ) = tokio::fs::read(proc_root.join(pid.to_string()).join("environ")).await
        else {
            continue;
        };
        if environ
            .split(|b| *b == 0)
            .any(|entry| entry == marker.as_bytes())
        {
            marked.push(*pid);
        }
    }
    marked
}

/// Stops the processes of `job` still running in its service's containers
/// (SIGTERM, then SIGKILL after [`STOP_GRACE`]). Only processes carrying
/// the job's marker are touched; each is checked again right before it is
/// signalled, so a reused PID is left alone. Returns how many there were.
async fn stop_job_processes(cfg: &AgentConfig, job: &JobFile) -> CoreResult<usize> {
    let Ok(paths) = compose::existing_project_paths(cfg, &job.project_slug) else {
        return Ok(0);
    };
    let containers =
        compose::service_container_ids(&job.project_slug, &paths, &job.service_name).await?;
    let mut pids = Vec::new();
    for container in &containers {
        let (code, stdout, _) =
            run_allow_nonzero("docker", &["top", container, "-o", "pid"]).await?;
        if code == 0 {
            pids.extend(parse_top_pids(&stdout));
        }
    }
    let marker = format!("{JOB_MARKER}={}", job.job_id);
    let proc_root = Path::new(PROC_ROOT);
    let targets = marked_processes(proc_root, &pids, &marker).await;
    if targets.is_empty() {
        return Ok(0);
    }
    let mut remaining = targets.clone();
    for signal in ["-TERM", "-KILL"] {
        let alive = marked_processes(proc_root, &remaining, &marker).await;
        if alive.is_empty() {
            break;
        }
        let pids: Vec<String> = alive.iter().map(u32::to_string).collect();
        let mut args = vec![signal, "--"];
        args.extend(pids.iter().map(String::as_str));
        run_allow_nonzero("kill", &args).await?;
        remaining = alive;
        let deadline = tokio::time::Instant::now() + STOP_GRACE;
        while tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(250)).await;
            remaining = marked_processes(proc_root, &remaining, &marker).await;
            if remaining.is_empty() {
                break;
            }
        }
    }
    Ok(targets.len())
}

pub async fn execute(cfg: &AgentConfig, slug: &str, job_id: Uuid) -> CoreResult<JobRunStatus> {
    check_slug(slug)?;
    let dir = job_dir(cfg, slug, job_id);
    let job = read_job_file(&dir).await?;
    if job.job_id != job_id || job.project_slug != slug {
        return Err(CoreError::Validation(
            "job definition does not match its unit".into(),
        ));
    }
    // The file is root-only, but validate it as if it were input.
    if !validate::is_valid_service_name(&job.service_name) {
        return Err(CoreError::Validation("invalid service name".into()));
    }
    validate_job_command(&job.command).map_err(CoreError::Validation)?;

    let runs_dir = dir.join(RUNS_DIR);
    compose::create_private_dir(&runs_dir).await?;
    // systemd runs one instance at a time: a record still marked running
    // was left by a helper that died.
    for record in read_records(&runs_dir, None).await? {
        if record.status == JobRunStatus::Running {
            write_json_atomically(&record_path(&runs_dir, record.id), &interrupted(record)).await?;
        }
    }
    let trigger = match tokio::fs::remove_file(dir.join(MANUAL_MARKER)).await {
        Ok(()) => JobRunTrigger::Manual,
        Err(_) => JobRunTrigger::Schedule,
    };
    let started_at = Utc::now();
    let mut record = JobRunInfo {
        id: Uuid::new_v4(),
        job_id,
        trigger,
        status: JobRunStatus::Running,
        started_at,
        finished_at: None,
        exit_code: None,
        error_message: None,
        log_bytes: 0,
        log_truncated: false,
        updated_at: started_at,
    };
    let path = record_path(&runs_dir, record.id);
    write_json_atomically(&path, &record).await?;

    let outcome = run_command(cfg, &job, &runs_dir, record.id).await;
    let finished_at = Utc::now();
    record.finished_at = Some(finished_at);
    record.updated_at = finished_at;
    match outcome {
        Ok(output) => {
            record.log_bytes = output.log_bytes;
            record.log_truncated = output.log_truncated;
            record.exit_code = output.exit_code;
            record.error_message = output.error;
            record.status = if output.exit_code == Some(0) {
                JobRunStatus::Succeeded
            } else {
                JobRunStatus::Failed
            };
        }
        Err(error) => {
            record.status = JobRunStatus::Failed;
            record.error_message = Some(error.message().to_string());
        }
    }
    write_json_atomically(&path, &record).await?;
    prune_runs(&runs_dir, JOB_RUNS_RETAINED).await?;
    Ok(record.status)
}

struct RunOutput {
    exit_code: Option<i32>,
    error: Option<String>,
    log_bytes: u64,
    log_truncated: bool,
}

async fn run_command(
    cfg: &AgentConfig,
    job: &JobFile,
    runs_dir: &Path,
    run_id: Uuid,
) -> CoreResult<RunOutput> {
    let root = compose::project_root(cfg, &job.project_slug);
    // Every environment value of the project is masked, like deployments.
    let paths = compose::existing_project_paths(cfg, &job.project_slug)?;
    let secrets =
        compose::resolved_environment_secrets(&root, &job.project_slug, &paths, &[]).await?;
    let (_cancel_tx, cancel) = tokio::sync::watch::channel(false);
    let mut log = DeployLog::create(runs_dir, run_id, JOB_LOG_MAX_BYTES, secrets, cancel).await?;
    // systemd runs one helper at a time, but a run whose helper was killed
    // may still be running inside the container.
    match stop_job_processes(cfg, job).await {
        Ok(0) => {}
        Ok(n) => {
            log.step(&format!(
                "stopped {n} process(es) left running by an earlier run"
            ))
            .await?
        }
        Err(error) => {
            log.step(&format!(
                "could not check for earlier runs: {}",
                error.message()
            ))
            .await?
        }
    }
    log.step(&describe_command(&job.service_name, &job.command))
        .await?;
    let result = match compose::existing_project_paths(cfg, &job.project_slug) {
        Ok(paths) => {
            let markers = [
                format!("{JOB_MARKER}={}", job.job_id),
                format!("{RUN_MARKER}={run_id}"),
            ];
            let args = compose::compose_exec_args(
                &job.project_slug,
                &paths,
                &job.service_name,
                &markers,
                &job.command,
            );
            let refs: Vec<&str> = args.iter().map(String::as_str).collect();
            let timeout = Duration::from_secs(job.timeout_secs.into());
            run_streamed("docker", &refs, &[], &mut log, timeout).await
        }
        Err(error) => Err(error),
    };
    if result.is_err() {
        // A timeout stops the exec client; the command itself is still
        // running in the container until it is stopped here.
        match stop_job_processes(cfg, job).await {
            Ok(0) => {}
            Ok(n) => {
                log.step(&format!(
                    "stopped {n} process(es) of this run in the container"
                ))
                .await?
            }
            Err(error) => {
                log.step(&format!(
                    "could not stop the command in the container: {}",
                    error.message()
                ))
                .await?
            }
        }
    }
    let (exit_code, error) = match result {
        Ok(0) => (Some(0), None),
        Ok(code) => (
            Some(code),
            Some(format!("the command exited with code {code}")),
        ),
        Err(error) => (
            None,
            Some(deploy_log::redact(error.message(), log.secrets())),
        ),
    };
    let summary = match (&exit_code, &error) {
        (Some(0), _) => "finished successfully".to_string(),
        (_, Some(error)) => error.clone(),
        _ => "failed".to_string(),
    };
    log.step(&summary).await?;
    Ok(RunOutput {
        exit_code,
        error,
        log_bytes: log.bytes_written(),
        log_truncated: log.truncated(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use panel_core::schedule::{JobSchedule, ScheduledJobSpec};

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("scontrol-jobs-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn record(job_id: Uuid, status: JobRunStatus, started_at: DateTime<Utc>) -> JobRunInfo {
        JobRunInfo {
            id: Uuid::new_v4(),
            job_id,
            trigger: JobRunTrigger::Schedule,
            status,
            started_at,
            finished_at: None,
            exit_code: None,
            error_message: None,
            log_bytes: 0,
            log_truncated: false,
            updated_at: started_at,
        }
    }

    #[test]
    fn units_run_the_agent_helper_with_fixed_arguments() {
        let id = Uuid::new_v4();
        let service = render_service_unit(
            Path::new("/usr/local/bin/panel-agent"),
            Path::new("/etc/scontrol/agent.toml"),
            "shop-api",
            id,
            300,
        )
        .unwrap();
        assert!(service.contains(&format!(
            "\nExecStart=/usr/local/bin/panel-agent --config /etc/scontrol/agent.toml run-scheduled-job shop-api {id}\n"
        )));
        assert!(service.contains("\nType=oneshot\n"));
        assert!(service.contains("\nTimeoutStartSec=360\n"));
        assert!(!service.contains("sh -c"));

        let timer = render_timer_unit(
            id,
            &[
                "*-*-01,15 09:00:00 Europe/Istanbul".into(),
                "Mon *-*-* 09:00:00 Europe/Istanbul".into(),
            ],
        )
        .unwrap();
        assert!(timer.contains("\nOnCalendar=*-*-01,15 09:00:00 Europe/Istanbul\nOnCalendar=Mon *-*-* 09:00:00 Europe/Istanbul\n"));
        assert!(timer.contains(&format!("\nUnit=scontrol-job-{id}.service\n")));
        assert!(timer.contains("\nWantedBy=timers.target\n"));
    }

    #[test]
    fn nothing_can_add_a_unit_line_or_specifier() {
        let id = Uuid::new_v4();
        let config = Path::new("/etc/scontrol/agent.toml");
        for binary in [
            "relative/panel-agent",
            "/opt/my agent/panel-agent",
            "/usr/bin/panel-agent\nExecStartPre=/bin/sh",
            "/usr/bin/%h",
            "/usr/bin/panel-agent;id",
            "/usr/../tmp/x",
            "/usr/local/bin/panel-agent (deleted)",
        ] {
            assert!(
                render_service_unit(Path::new(binary), config, "shop-api", id, 60).is_err(),
                "{binary:?}"
            );
        }
        assert!(render_service_unit(Path::new("/usr/bin/a"), config, "Shop api", id, 60).is_err());
        for bad in [
            "daily\nExecStart=/bin/sh UTC",
            "daily %h UTC",
            "daily",
            "daily UTC\n",
            "daily ../../etc",
        ] {
            assert!(render_timer_unit(id, &[bad.into()]).is_err(), "{bad:?}");
        }
        assert!(render_timer_unit(id, &[]).is_err());
    }

    #[tokio::test]
    async fn unit_files_are_replaced_atomically_and_removed() {
        let dir = temp_dir();
        let id = Uuid::new_v4();
        let (service, timer) = unit_names(id);
        // A symlink planted at the destination is replaced, not followed.
        let outside = dir.join("outside");
        std::fs::write(&outside, "keep").unwrap();
        std::os::unix::fs::symlink(&outside, dir.join(&service)).unwrap();
        write_unit_file(&dir, &service, "[Service]\n")
            .await
            .unwrap();
        write_unit_file(&dir, &timer, "[Timer]\n").await.unwrap();
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "keep");
        assert_eq!(
            std::fs::read_to_string(dir.join(&service)).unwrap(),
            "[Service]\n"
        );
        let meta = std::fs::symlink_metadata(dir.join(&service)).unwrap();
        assert!(meta.file_type().is_file());
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(meta.permissions().mode() & 0o777, 0o644);
        // A stale temporary file from an interrupted write is no obstacle.
        std::fs::write(dir.join(format!(".{timer}.tmp")), "stale").unwrap();
        write_unit_file(&dir, &timer, "[Timer]\nx\n").await.unwrap();
        assert!(!dir.join(format!(".{timer}.tmp")).exists());

        assert!(remove_unit_files(&dir, id).await.unwrap());
        assert!(!dir.join(&service).exists() && !dir.join(&timer).exists());
        assert!(!remove_unit_files(&dir, id).await.unwrap());
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn records_are_listed_by_change_time_and_pruned() {
        let dir = temp_dir();
        let job_id = Uuid::new_v4();
        let base = Utc::now() - chrono::Duration::minutes(10);
        let mut ids = Vec::new();
        for minute in 0..5 {
            let mut r = record(
                job_id,
                JobRunStatus::Succeeded,
                base + chrono::Duration::minutes(minute),
            );
            r.updated_at = r.started_at;
            write_json_atomically(&record_path(&dir, r.id), &r)
                .await
                .unwrap();
            std::fs::write(deploy_log::log_path(&dir, r.id), "out\n").unwrap();
            ids.push(r.id);
        }
        std::fs::write(dir.join("not-a-record.json"), "{}").unwrap();

        let all = read_records(&dir, None).await.unwrap();
        assert_eq!(all.iter().map(|r| r.id).collect::<Vec<_>>(), ids);
        let recent = read_records(&dir, Some(base + chrono::Duration::seconds(150)))
            .await
            .unwrap();
        assert_eq!(recent.len(), 2);

        prune_runs(&dir, 3).await.unwrap();
        let kept = read_records(&dir, None).await.unwrap();
        assert_eq!(kept.iter().map(|r| r.id).collect::<Vec<_>>(), ids[2..]);
        assert!(!deploy_log::log_path(&dir, ids[0]).exists());
        assert!(deploy_log::log_path(&dir, ids[4]).exists());
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn only_processes_carrying_the_job_marker_are_selected() {
        assert_eq!(parse_top_pids("PID\n4242\n  4243 \n1\nx\n"), [4242, 4243]);
        let proc_root = temp_dir();
        let job = Uuid::new_v4();
        let marker = format!("{JOB_MARKER}={job}");
        for (pid, environ) in [
            (10u32, format!("PATH=/bin\0{marker}\0{RUN_MARKER}=x\0")),
            (11, format!("{marker}0\0")),
            (12, format!("X={marker}\0")),
            (13, format!("{JOB_MARKER}={}\0", Uuid::new_v4())),
        ] {
            let dir = proc_root.join(pid.to_string());
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("environ"), environ).unwrap();
        }
        let found = marked_processes(&proc_root, &[10, 11, 12, 13, 14], &marker).await;
        assert_eq!(found, [10]);
        std::fs::remove_dir_all(proc_root).ok();
    }

    #[test]
    fn interrupted_runs_are_closed_as_failed() {
        let running = record(Uuid::new_v4(), JobRunStatus::Running, Utc::now());
        let closed = interrupted(running.clone());
        assert_eq!(closed.status, JobRunStatus::Failed);
        assert!(closed.finished_at.is_some() && closed.updated_at >= running.updated_at);
        assert!(closed.error_message.unwrap().contains("interrupted"));
    }

    #[test]
    fn job_file_round_trips_and_command_is_described_per_argument() {
        let job = JobFile {
            job_id: Uuid::new_v4(),
            project_slug: "shop-api".into(),
            service_name: "web".into(),
            command: vec!["echo".into(), "a b; rm -rf /".into()],
            timeout_secs: 60,
        };
        let parsed: JobFile = serde_json::from_str(&serde_json::to_string(&job).unwrap()).unwrap();
        assert_eq!(parsed, job);
        assert_eq!(
            describe_command("web", &job.command),
            r#"exec in service web: ["echo", "a b; rm -rf /"]"#
        );
    }

    #[test]
    fn spec_validation_runs_before_any_host_change() {
        // `upsert` validates with the shared spec rules first; a shell
        // command string never reaches the unit files or job.json.
        let spec = ScheduledJobSpec {
            service_name: "web".into(),
            command: vec!["sh".into(), "-c".into(), "id".into()],
            schedule: JobSchedule::Cron("* * * * *".into()),
            timezone: "UTC".into(),
            timeout_secs: 60,
        };
        assert!(spec.validate().is_err());
    }
}
