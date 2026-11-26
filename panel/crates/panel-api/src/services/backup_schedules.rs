//! Scheduled backups: the panel-api scheduler and the run it starts.
//!
//! A schedule backs up either a managed database service (its connection
//! and sealed password come from the service, so nobody types a password
//! that would have to be stored) or a named volume of a Compose service.
//! The scheduler wakes every [`TICK`], claims the schedules that are due
//! (`db::backup_schedules::claim_due`, safe with several panel processes),
//! and starts each one as an ordinary `backup_runs` row with
//! `trigger = 'schedule'`. The agent applies the schedule's retention right
//! after a successful upload, within the series of that service only.
//!
//! Secrets (S3 keys, database password) are opened only to build the agent
//! request, travel only in it, and are redacted from every stored error.

use crate::db;
use crate::db::backup_schedules::ScheduleRow;
use crate::db::backups::RunRow;
use crate::services::agent_pool::AgentPool;
use crate::services::notifications::{Notification, Notifier};
use crate::services::secret_box::SecretBox;
use crate::services::{backups as sealed_destinations, database_services as sealed_databases};
use chrono::{DateTime, Utc};
use panel_core::adapters::*;
use panel_core::database_services::ManagedDatabaseEngine;
use panel_core::types::{ComposeProject, ComposeSource};
use serde_json::json;
use sqlx::PgPool;
use std::sync::Arc;
use std::time::Duration;

/// How often the scheduler looks for due schedules. Cron has minute
/// resolution, so a run starts at most this late.
const TICK: Duration = Duration::from_secs(20);
/// Schedules claimed per tick; the rest are claimed on the next one.
const CLAIM_LIMIT: i64 = 50;

/// Everything a run needs from the panel's state.
#[derive(Clone)]
pub struct BackupContext {
    pub db: PgPool,
    pub agents: AgentPool,
    pub notifier: Notifier,
    pub secret_box: SecretBox,
}

/// The next time `row` runs after `after`, or `None` while it is disabled
/// (or, defensively, when its stored expression no longer parses).
pub fn next_run(
    row_enabled: bool,
    cron: &str,
    timezone: &str,
    after: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    if !row_enabled {
        return None;
    }
    panel_core::schedule::next_cron_run(cron, timezone, after).ok()
}

/// The backup engine of a managed database service, or why it has none.
pub fn backup_engine(engine: ManagedDatabaseEngine) -> Result<BackupEngine, String> {
    match engine {
        ManagedDatabaseEngine::Postgres => Ok(BackupEngine::Postgres),
        ManagedDatabaseEngine::Mysql | ManagedDatabaseEngine::Mariadb => Ok(BackupEngine::Mysql),
        ManagedDatabaseEngine::Mongo => Ok(BackupEngine::Mongodb),
        ManagedDatabaseEngine::Redis => {
            Err("Redis has no dump tool support; back up its volume instead".into())
        }
    }
}

/// A run ready to hand to the agent.
struct Prepared {
    kind: &'static str,
    source: String,
    agent: Arc<dyn AdapterPort>,
    request: Request,
    /// Values to remove from any error text before it is stored or sent.
    secrets: Vec<String>,
}

enum Request {
    Database(BackupDatabaseParams),
    Volume(BackupVolumeParams),
}

fn kind_and_source(row: &ScheduleRow, project: Option<&ComposeProject>) -> (&'static str, String) {
    let slug = project.map_or("?", |p| p.slug.as_str());
    match row.target_kind.as_str() {
        "volume" => (
            "volume_backup",
            format!("{slug}/{}", row.volume_name.as_deref().unwrap_or("?")),
        ),
        _ => {
            let database = match project.map(|p| &p.source) {
                Some(ComposeSource::Database(spec)) => spec.database_name.as_str(),
                _ => "?",
            };
            ("database_backup", format!("{slug}/{database}"))
        }
    }
}

async fn prepare(
    ctx: &BackupContext,
    row: &ScheduleRow,
    project: &ComposeProject,
) -> Result<Prepared, String> {
    let dest = db::backups::find_destination(&ctx.db, row.s3_destination_id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or("the S3 destination no longer exists")?;
    let target = sealed_destinations::open(ctx.notifier.secrets(), &dest)
        .map_err(|_| "the destination's stored S3 keys cannot be opened".to_string())?;
    let mut secrets = vec![target.secret_key.clone(), target.access_key.clone()];
    let agent = ctx
        .agents
        .for_server_id(&ctx.db, project.server_id)
        .await
        .map_err(|e| e.to_string())?;
    let retention_count = row.retention_count.max(0) as u32;
    let (kind, source) = kind_and_source(row, Some(project));
    let request = match row.target_kind.as_str() {
        "database" => {
            let ComposeSource::Database(spec) = &project.source else {
                return Err("the service is no longer a database service".into());
            };
            let engine = backup_engine(spec.engine)?;
            let service = db::database_services::find(&ctx.db, project.id)
                .await
                .map_err(|e| e.to_string())?
                .ok_or("the database service settings are missing")?;
            let credentials = sealed_databases::open(&ctx.secret_box, &service)
                .map_err(|_| "the database service's credentials cannot be opened".to_string())?;
            secrets.push(credentials.password.clone());
            secrets.extend(credentials.root_password.clone());
            Request::Database(BackupDatabaseParams {
                target,
                prefix: dest.prefix.clone(),
                connection: DatabaseConnection {
                    engine,
                    host: "127.0.0.1".into(),
                    port: u16::try_from(service.loopback_port)
                        .map_err(|_| "invalid loopback port".to_string())?,
                    database: spec.database_name.clone(),
                    username: spec.username.clone(),
                    password: credentials.password,
                },
                retention_count,
                namespace: Some(service_backup_namespace(&project.slug)),
            })
        }
        "volume" => Request::Volume(BackupVolumeParams {
            target,
            prefix: dest.prefix.clone(),
            project_slug: project.slug.clone(),
            service: row.service_name.clone(),
            volume: row
                .volume_name
                .clone()
                .ok_or("the schedule names no volume")?,
            stop_service: row.stop_service,
            retention_count,
        }),
        other => return Err(format!("unknown target kind {other:?}")),
    };
    Ok(Prepared {
        kind,
        source,
        agent,
        request,
        secrets,
    })
}

/// Removes every secret from `text` and bounds it.
pub fn redact(text: &str, secrets: &[String]) -> String {
    let mut text = text.to_string();
    for secret in secrets.iter().filter(|s| s.len() >= 4) {
        text = text.replace(secret.as_str(), "********");
    }
    text.chars().take(1000).collect()
}

/// Starts one run of `row`: records a `running` row linked to the schedule
/// and finishes it from a task. A schedule that cannot be prepared (its
/// service is gone, its keys do not open) still gets a failed run, so the
/// problem shows up in the panel and in notifications.
pub async fn start_run(ctx: &BackupContext, row: &ScheduleRow) -> anyhow::Result<RunRow> {
    let project = db::compose_projects::find_by_id(&ctx.db, row.compose_project_id).await?;
    let prepared = match &project {
        Some(project) => prepare(ctx, row, project).await,
        None => Err("the service no longer exists".into()),
    };
    let (kind, source) = match &prepared {
        Ok(p) => (p.kind, p.source.clone()),
        Err(_) => kind_and_source(row, project.as_ref()),
    };
    let run =
        db::backups::start_run_for(&ctx.db, row.s3_destination_id, kind, &source, Some(row.id))
            .await?;
    db::backup_schedules::record_run(&ctx.db, row.id, run.id, run.started_at).await?;
    db::audit::record(
        &ctx.db,
        None,
        "system",
        kind,
        Some(&row.name),
        json!({ "schedule_id": row.id, "run_id": run.id, "source": source,
                "destination_id": row.s3_destination_id }),
    )
    .await
    .ok();

    let ctx = ctx.clone();
    let schedule = row.clone();
    let returned = run.clone();
    tokio::spawn(async move {
        let result = match prepared {
            Err(error) => Err(error),
            Ok(p) => {
                let outcome = match p.request {
                    Request::Database(params) => p.agent.backup_database(params).await,
                    Request::Volume(params) => p.agent.backup_volume(params).await,
                };
                outcome
                    .map(|out| {
                        (
                            Some(out.key),
                            Some(out.size_bytes as i64),
                            out.pruned as i32,
                        )
                    })
                    .map_err(|e| redact(&e.to_string(), &p.secrets))
            }
        };
        let failure = result.as_ref().err().cloned();
        if let Err(e) = db::backups::finish_run(&ctx.db, run.id, result).await {
            tracing::error!(error = %e, run_id = %run.id, "recording scheduled backup result");
        }
        if let Some(error) = failure {
            tracing::warn!(schedule_id = %schedule.id, run_id = %run.id, %error, "scheduled backup failed");
            ctx.notifier.broadcast(
                ctx.db.clone(),
                Notification::backup_failed(&schedule.name, &source, run.id, schedule.id, &error),
            );
        }
    });
    Ok(returned)
}

/// One scheduler pass: claims due schedules and starts them. A schedule
/// whose previous run is still going skips this occurrence.
pub async fn tick(ctx: &BackupContext, now: DateTime<Utc>) -> anyhow::Result<usize> {
    let due = db::backup_schedules::claim_due(&ctx.db, now, CLAIM_LIMIT, |row| {
        next_run(row.enabled, &row.cron_expression, &row.timezone, now)
    })
    .await?;
    let mut started = 0;
    for row in due {
        if db::backup_schedules::has_running_run(&ctx.db, row.id).await? {
            tracing::warn!(schedule_id = %row.id, "previous scheduled backup still running; skipped");
            continue;
        }
        match start_run(ctx, &row).await {
            Ok(_) => started += 1,
            Err(error) => {
                tracing::error!(schedule_id = %row.id, %error, "starting scheduled backup")
            }
        }
    }
    Ok(started)
}

/// Runs the scheduler for the life of the process.
pub fn start(ctx: BackupContext) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(TICK);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            if let Err(error) = tick(&ctx, Utc::now()).await {
                tracing::error!(%error, "backup scheduler pass failed");
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redaction_removes_every_secret() {
        let secrets = vec![
            "s3-secret-key".to_string(),
            "db-password".to_string(),
            "ab".to_string(),
        ];
        let text = redact("auth s3-secret-key failed for db-password (ab)", &secrets);
        assert_eq!(text, "auth ******** failed for ******** (ab)");
    }

    #[test]
    fn redis_has_no_dump_backup() {
        assert!(backup_engine(ManagedDatabaseEngine::Redis).is_err());
        assert!(matches!(
            backup_engine(ManagedDatabaseEngine::Mariadb),
            Ok(BackupEngine::Mysql)
        ));
    }
}
