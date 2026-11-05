//! Scheduled-job run collection.
//!
//! systemd timers on the host start runs, and the agent records each run
//! next to its log (see `panel-agent` `adapters/scheduled_jobs.rs`). This
//! collector copies those records into `job_runs`, keeps each job's last
//! run up to date, and sends `job_failed` notifications. A finished run is
//! stored once (`db::scheduled_jobs::upsert_run` reports only new or changed
//! rows), so a failure is notified exactly once however often it is seen.
//!
//! Each server's agent is asked about its own jobs, with its own cursor;
//! the servers are asked at once and each pass is bounded by
//! [`SERVER_TIMEOUT`], so an unreachable server delays no other.
//!
//! It polls the agent every [`IDLE_POLL`], every [`ACTIVE_POLL`] while a
//! run is in progress, and a moment after "run now". With no jobs it costs
//! one indexed query per poll and no agent call.

use crate::db;
use crate::services::agent_pool::AgentPool;
use crate::services::notifications::Notifier;
use chrono::{DateTime, Utc};
use panel_core::adapters::{AdapterPort, ListScheduledJobRunsParams, ScheduledJobRef};
use panel_core::schedule::{JobRunStatus, JOB_RUNS_RETAINED};
use sqlx::PgPool;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;
use uuid::Uuid;

const IDLE_POLL: Duration = Duration::from_secs(10);
const ACTIVE_POLL: Duration = Duration::from_secs(2);
/// How long after "run now" the collector looks, so the run shows quickly.
const RUN_NOW_DELAY: Duration = Duration::from_millis(1500);
/// Records are asked for again this long after the newest one seen: a
/// record written a moment late (its time is taken before the write) must
/// not fall behind the cursor. Re-reading a stored record is a no-op.
const CURSOR_OVERLAP: chrono::Duration = chrono::Duration::seconds(30);
/// Longest one server's collection pass may take.
const SERVER_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Default)]
pub struct JobRunCollector {
    wake: Arc<Notify>,
}

impl JobRunCollector {
    pub fn new() -> Self {
        Self::default()
    }

    /// Spawns the collector loop. Call once, from the process that serves
    /// HTTP (like the deployment workers).
    pub fn start(&self, db: PgPool, agents: AgentPool, notifier: Notifier) {
        let wake = self.wake.clone();
        tokio::spawn(async move {
            let mut cursors: HashMap<Uuid, DateTime<Utc>> = HashMap::new();
            loop {
                match db::scheduled_jobs::list_refs(&db).await {
                    Ok(refs) => {
                        let servers: HashSet<Uuid> = refs.iter().map(|r| r.server_id).collect();
                        let passes = servers.into_iter().map(|server| {
                            let (db, agents, notifier) = (&db, &agents, &notifier);
                            let since = cursors.get(&server).copied();
                            async move {
                                let pass = async {
                                    let agent = agents
                                        .for_server_id(db, server)
                                        .await
                                        .map_err(|e| anyhow::anyhow!(e.to_string()))?;
                                    collect_server(db, agent.as_ref(), notifier, server, since)
                                        .await
                                };
                                let result = tokio::time::timeout(SERVER_TIMEOUT, pass)
                                    .await
                                    .unwrap_or_else(|_| Err(anyhow::anyhow!("timed out")));
                                (server, result)
                            }
                        });
                        for (server, result) in futures::future::join_all(passes).await {
                            match result {
                                Ok(Some(latest)) => {
                                    cursors.insert(server, latest - CURSOR_OVERLAP);
                                }
                                Ok(None) => {}
                                Err(error) => {
                                    tracing::warn!(%server, %error, "collecting scheduled job runs failed")
                                }
                            }
                        }
                    }
                    Err(error) => tracing::warn!(%error, "listing scheduled jobs failed"),
                }
                let poll = match db::scheduled_jobs::any_running(&db).await {
                    Ok(true) => ACTIVE_POLL,
                    _ => IDLE_POLL,
                };
                tokio::select! {
                    _ = wake.notified() => {}
                    _ = tokio::time::sleep(poll) => {}
                }
            }
        });
    }

    /// Collects again shortly, after a run was started from the panel.
    pub fn run_started(&self) {
        let wake = self.wake.clone();
        tokio::spawn(async move {
            tokio::time::sleep(RUN_NOW_DELAY).await;
            wake.notify_one();
        });
    }
}

/// One collection pass over the jobs on this host.
#[cfg(test)]
pub async fn collect_once(
    db: &PgPool,
    agent: &dyn AdapterPort,
    notifier: &Notifier,
    since: Option<DateTime<Utc>>,
) -> anyhow::Result<Option<DateTime<Utc>>> {
    collect_server(db, agent, notifier, db::servers::LOCAL_SERVER_ID, since).await
}

/// One collection pass over the jobs of the services on `server`, asking
/// that server's `agent`. Returns the newest record time seen, to use as
/// the server's next cursor.
pub async fn collect_server(
    db: &PgPool,
    agent: &dyn AdapterPort,
    notifier: &Notifier,
    server: Uuid,
    since: Option<DateTime<Utc>>,
) -> anyhow::Result<Option<DateTime<Utc>>> {
    let jobs: Vec<ScheduledJobRef> = db::scheduled_jobs::list_refs(db)
        .await?
        .into_iter()
        .filter(|job| job.server_id == server)
        .map(|job| ScheduledJobRef {
            project_slug: job.project_slug,
            job_id: job.job_id,
        })
        .collect();
    if jobs.is_empty() {
        return Ok(None);
    }
    let runs = agent
        .list_scheduled_job_runs(ListScheduledJobRunsParams { jobs, since })
        .await?;
    let latest = runs.iter().map(|run| run.updated_at).max();
    let mut touched = HashSet::new();
    for run in runs {
        let changed = match db::scheduled_jobs::upsert_run(db, &run).await {
            Ok(changed) => changed,
            // The job was deleted after the listing; its runs go with it.
            Err(sqlx::Error::Database(e)) if e.is_foreign_key_violation() => continue,
            Err(error) => return Err(error.into()),
        };
        let Some(status) = changed else { continue };
        db::scheduled_jobs::set_last_run(db, run.job_id, run.started_at, status).await?;
        touched.insert(run.job_id);
        if status == JobRunStatus::Failed {
            notifier.job_run_failed(db.clone(), run);
        }
    }
    for job_id in touched {
        db::scheduled_jobs::prune_runs(db, job_id, JOB_RUNS_RETAINED as i64).await?;
    }
    Ok(latest)
}
