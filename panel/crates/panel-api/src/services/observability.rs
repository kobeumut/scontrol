//! Background work for D4 observability: reading the Nginx access-log
//! statistics once a minute and running the scheduled Docker cleanup.
//!
//! Costs when idle: one agent call a minute (a stat of the log file when
//! nothing is new) and one settings query every five minutes.
//!
//! A cleanup never overlaps a deployment on the same server: it does not
//! start while one is queued or running there, and the deployment queue
//! claims nothing on a server while its cleanup runs (both checks hold the
//! claim lock, see `db::observability::start_run`). Docker 29's containerd
//! image store loses in-flight pull leases to a concurrent prune
//! (moby/moby#53321), and a builder prune can pull the cache from under a
//! running build. A scheduled cleanup that finds the server busy is simply
//! retried at the next check.

use crate::db;
use crate::db::observability::{CleanupRun, CleanupSettings, StartRun};
use crate::error::{ApiError, ApiResult};
use crate::services::agent_pool::AgentPool;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use panel_core::adapters::{AdapterPort, PruneDockerParams, ReadNginxStatsParams};
use panel_core::error::CoreError;
use sqlx::PgPool;
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

const STATS_POLL: Duration = Duration::from_secs(60);
const CLEANUP_CHECK: Duration = Duration::from_secs(300);
/// Bytes of log asked for per agent call, and calls per poll (so a very
/// large first backlog is worked off over several polls, not one).
const READ_BYTES: u32 = 4 * 1024 * 1024;
const MAX_READS_PER_POLL: usize = 16;
/// Days of per-minute statistics kept.
pub const STATS_RETENTION_DAYS: i32 = 30;

/// Longest one server's statistics read or cleanup check may take.
const SERVER_TIMEOUT: Duration = Duration::from_secs(120);
/// Longest panel-api waits for the agent's cleanup (the agent bounds its own
/// work at 25 minutes); the run is then recorded as failed. Below
/// `db::observability::CLEANUP_STALE_SECS`.
pub const CLEANUP_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// Every server: the panel host and each remote server.
async fn server_ids(db: &PgPool) -> sqlx::Result<Vec<Uuid>> {
    Ok(db::servers::list(db)
        .await?
        .into_iter()
        .map(|s| s.id)
        .collect())
}

/// Runs `work` for every server at once, each bounded by
/// [`SERVER_TIMEOUT`], so one unreachable server delays no other.
async fn for_each_server<F, Fut>(db: &PgPool, agents: &AgentPool, label: &str, work: F)
where
    F: Fn(Uuid, Arc<dyn AdapterPort>) -> Fut,
    Fut: std::future::Future<Output = ApiResult<()>>,
{
    let servers = match server_ids(db).await {
        Ok(servers) => servers,
        Err(error) => {
            tracing::warn!(%error, "listing servers failed");
            return;
        }
    };
    let passes = servers.into_iter().map(|server| {
        let work = &work;
        async move {
            let pass = async {
                let agent = agents.for_server_id(db, server).await?;
                work(server, agent).await
            };
            let result = tokio::time::timeout(SERVER_TIMEOUT, pass)
                .await
                .unwrap_or_else(|_| Err(ApiError::Core(CoreError::External("timed out".into()))));
            (server, result)
        }
    });
    for (server, result) in futures::future::join_all(passes).await {
        if let Err(error) = result {
            tracing::warn!(%server, %error, "{label} failed");
            if label == "reading request statistics" {
                let _ = db::observability::set_cursor_error(db, server, &error.to_string()).await;
            }
        }
    }
}

pub fn start(db: PgPool, agents: AgentPool) {
    let (pool, stats_agents) = (db.clone(), agents.clone());
    tokio::spawn(async move {
        let mut polls = 0u32;
        loop {
            for_each_server(
                &pool,
                &stats_agents,
                "reading request statistics",
                |server, agent| {
                    let pool = &pool;
                    async move {
                        collect_stats(pool, agent.as_ref(), server)
                            .await
                            .map(|_| ())
                    }
                },
            )
            .await;
            polls = polls.wrapping_add(1);
            if polls % 60 == 1 {
                if let Err(error) =
                    db::observability::prune_stats(&pool, STATS_RETENTION_DAYS).await
                {
                    tracing::warn!(%error, "trimming request statistics failed");
                }
            }
            tokio::time::sleep(STATS_POLL).await;
        }
    });
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(CLEANUP_CHECK).await;
            for_each_server(
                &db,
                &agents,
                "scheduled docker cleanup check",
                |server, agent| {
                    let db = &db;
                    async move { scheduled_cleanup_tick(db, agent, server).await }
                },
            )
            .await;
        }
    });
}

/// Reads the log from the stored cursor to its end (bounded per call).
/// Returns the number of requests counted.
pub async fn collect_stats(db: &PgPool, agent: &dyn AdapterPort, server: Uuid) -> ApiResult<u64> {
    let mut total = 0;
    for _ in 0..MAX_READS_PER_POLL {
        let cursor = db::observability::cursor(db, server).await?;
        let chunk = agent
            .read_nginx_stats(ReadNginxStatsParams {
                file_id: cursor.file_id.map(|v| v as u64),
                offset: cursor.byte_offset.max(0) as u64,
                max_bytes: READ_BYTES,
            })
            .await?;
        if !chunk.log_present {
            return Ok(total);
        }
        let moved = chunk.next_offset != cursor.byte_offset.max(0) as u64
            || chunk.file_id.map(|v| v as i64) != cursor.file_id;
        if moved {
            db::observability::apply_chunk(
                db,
                server,
                &chunk.buckets,
                chunk.file_id,
                chunk.next_offset,
            )
            .await?;
        } else {
            db::observability::apply_chunk(db, server, &[], chunk.file_id, chunk.next_offset)
                .await?;
        }
        total += chunk.lines;
        if !moved {
            break;
        }
    }
    Ok(total)
}

/// Runs one cleanup and records it. `Conflict` while another cleanup runs
/// or deployments are queued or running on the server.
pub async fn run_cleanup(
    db: &PgPool,
    agent: Arc<dyn AdapterPort>,
    server: Uuid,
    trigger: &str,
    older_than_hours: u32,
) -> ApiResult<CleanupRun> {
    let (_, task) = start_cleanup(
        db,
        agent,
        server,
        trigger,
        older_than_hours,
        CLEANUP_TIMEOUT,
    )
    .await?;
    task.await
        .map_err(|e| ApiError::Internal(anyhow::anyhow!("docker cleanup task failed: {e}")))?
}

/// Records a `running` cleanup and spawns the task that runs it on the
/// agent and finishes the row. The task owns both, so a caller that goes
/// away (a closed HTTP request, the scheduler's per-server timeout) never
/// leaves the row `running`, which would hold back every deployment on the
/// server. An agent that gives no result within `timeout` fails the run.
async fn start_cleanup(
    db: &PgPool,
    agent: Arc<dyn AdapterPort>,
    server: Uuid,
    trigger: &str,
    older_than_hours: u32,
    timeout: Duration,
) -> ApiResult<(CleanupRun, tokio::task::JoinHandle<ApiResult<CleanupRun>>)> {
    panel_core::observability::prune_until_filter(older_than_hours)
        .map_err(|m| ApiError::Core(CoreError::Validation(m)))?;
    let run =
        match db::observability::start_run(db, server, trigger, older_than_hours as i32).await? {
            StartRun::Started(run) => run,
            StartRun::CleanupRunning => {
                return Err(ApiError::Conflict(
                    "a Docker cleanup is already running".into(),
                ))
            }
            StartRun::DeploymentsActive(n) => {
                return Err(ApiError::Conflict(format!(
                    "{n} deployment(s) are queued or running on this server; \
                 Docker cleanup waits until they finish"
                )))
            }
        };
    let (db, id) = (db.clone(), run.id);
    let task = tokio::spawn(async move {
        let prune = agent.prune_docker(PruneDockerParams { older_than_hours });
        let result = match tokio::time::timeout(timeout, prune).await {
            Ok(Ok(o)) => Ok((o.images_freed_bytes, o.builder_freed_bytes)),
            // The agent's message can carry Docker's stderr: keep it short.
            Ok(Err(error)) => Err(error.to_string().chars().take(500).collect::<String>()),
            Err(_) => Err(format!(
                "the agent gave no result within {} s",
                timeout.as_secs_f64()
            )),
        };
        let finished = db::observability::finish_run(
            &db,
            id,
            result.as_ref().copied().map_err(String::as_str),
        )
        .await?;
        let _ = db::observability::trim_runs(&db).await;
        tracing::info!(run_id = %id, status = %finished.status, "docker cleanup finished");
        Ok(finished)
    });
    Ok((run, task))
}

/// Whether an automatic cleanup is due. The first one runs a full interval
/// after the last scheduled run (or right away if there never was one).
pub fn cleanup_due(
    settings: &CleanupSettings,
    last_scheduled: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> bool {
    settings.enabled
        && last_scheduled.is_none_or(|last| {
            now - last >= ChronoDuration::hours(i64::from(settings.interval_hours))
        })
}

/// Starts a due cleanup and returns without waiting for it: the run can
/// take longer than [`SERVER_TIMEOUT`], and its task records the result.
/// A busy server (cleanup or deployments) is tried again at the next check.
async fn scheduled_cleanup_tick(
    db: &PgPool,
    agent: Arc<dyn AdapterPort>,
    server: Uuid,
) -> ApiResult<()> {
    let settings = db::observability::cleanup_settings(db).await?;
    let last = db::observability::last_scheduled_start(db, server).await?;
    if cleanup_due(&settings, last, Utc::now()) {
        match start_cleanup(
            db,
            agent,
            server,
            "schedule",
            settings.older_than_hours as u32,
            CLEANUP_TIMEOUT,
        )
        .await
        {
            Ok((run, _)) => {
                tracing::info!(%server, run_id = %run.id, "scheduled docker cleanup started")
            }
            Err(ApiError::Conflict(reason)) => {
                tracing::info!(%server, %reason, "scheduled docker cleanup deferred")
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{test_pool, QUEUE_TESTS};
    use panel_core::adapters::mock::MockAdapter;
    use panel_core::adapters::{DockerPruneOutcome, NginxStatsChunk};
    use panel_core::observability::RequestBucket;

    const LOCAL: Uuid = db::servers::LOCAL_SERVER_ID;

    fn settings(enabled: bool, hours: i32) -> CleanupSettings {
        CleanupSettings {
            enabled,
            interval_hours: hours,
            older_than_hours: 168,
        }
    }

    #[tokio::test]
    async fn trimming_cleanup_history_never_deletes_an_active_run() {
        let _guard = QUEUE_TESTS.lock().await;
        let pool = test_pool().await;
        let server = server(&pool).await;
        let active = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO docker_cleanup_runs (id, server_id, trigger, status, older_than_hours, started_at)
             VALUES ($1, $2, 'manual', 'running', 168, now() - interval '2 hours')",
        ).bind(active).bind(server).execute(&pool).await.unwrap();
        sqlx::query(
            "INSERT INTO docker_cleanup_runs (id, server_id, trigger, status, older_than_hours)
             SELECT gen_random_uuid(), $1, 'manual', 'succeeded', 168 FROM generate_series(1, 101)",
        )
        .bind(server)
        .execute(&pool)
        .await
        .unwrap();
        db::observability::trim_runs(&pool).await.unwrap();
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM docker_cleanup_runs WHERE id = $1 AND status = 'running')",
        )
        .bind(active)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(exists);
        let completed: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM docker_cleanup_runs WHERE status != 'running'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(completed, 100);
        db::observability::finish_run(&pool, active, Ok((0, 0)))
            .await
            .unwrap();
    }

    #[test]
    fn cleanup_due_follows_the_interval() {
        let now = Utc::now();
        assert!(!cleanup_due(&settings(false, 24), None, now));
        assert!(cleanup_due(&settings(true, 24), None, now));
        assert!(!cleanup_due(
            &settings(true, 24),
            Some(now - ChronoDuration::hours(23)),
            now
        ));
        assert!(cleanup_due(
            &settings(true, 24),
            Some(now - ChronoDuration::hours(24)),
            now
        ));
    }

    fn bucket(host: &str, minute: DateTime<Utc>, requests: u64) -> RequestBucket {
        RequestBucket {
            minute,
            host: host.into(),
            requests,
            status_2xx: requests,
            bytes_sent: requests * 10,
            request_time_ms_sum: requests * 20,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn stats_accumulate_across_polls_and_move_the_cursor() {
        let _guard = QUEUE_TESTS.lock().await;
        let pool = test_pool().await;
        sqlx::query("DELETE FROM request_stats_minute")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("UPDATE nginx_log_cursor SET file_id = NULL, byte_offset = 0")
            .execute(&pool)
            .await
            .unwrap();
        let minute = DateTime::from_timestamp(1_800_000_000, 0).unwrap();
        let adapter = MockAdapter::new();
        {
            let mut q = adapter.nginx_chunks.lock().unwrap();
            for (offset, n) in [(100u64, 2u64), (250, 3)] {
                q.push_back(NginxStatsChunk {
                    buckets: vec![bucket("a.com", minute, n)],
                    file_id: Some(7),
                    next_offset: offset,
                    lines: n,
                    skipped: 0,
                    log_present: true,
                });
            }
        }
        let counted = collect_stats(&pool, &adapter, LOCAL).await.unwrap();
        assert_eq!(counted, 5);
        let cursor = db::observability::cursor(&pool, LOCAL).await.unwrap();
        assert_eq!((cursor.file_id, cursor.byte_offset), (Some(7), 250));
        let totals = db::observability::host_totals(&pool, minute - ChronoDuration::hours(1))
            .await
            .unwrap();
        let a = totals.iter().find(|t| t.host == "a.com").unwrap();
        assert_eq!((a.requests, a.status_2xx, a.bytes_sent), (5, 5, 50));
        assert!((a.avg_latency_ms - 20.0).abs() < 1e-9);
        // The mock now returns an empty chunk at the stored offset: no change.
        assert_eq!(collect_stats(&pool, &adapter, LOCAL).await.unwrap(), 0);
        let series =
            db::observability::series(&pool, minute - ChronoDuration::hours(1), 5, Some("a.com"))
                .await
                .unwrap();
        assert_eq!(series.len(), 1);
        assert_eq!(series[0].requests, 5);
    }

    /// A fresh remote server, so leftovers of other tests on this host
    /// (queued deployments, cleanup rows) cannot interfere.
    async fn server(pool: &PgPool) -> Uuid {
        let id = Uuid::new_v4();
        db::servers::insert(
            pool,
            db::servers::NewServer {
                id,
                name: &format!("c-{}", &id.simple().to_string()[..8]),
                transport: db::servers::Transport::SshSocket,
                address: "/run/scontrol/remote/c.sock",
                max_services: None,
            },
        )
        .await
        .unwrap();
        id
    }

    async fn service_on(pool: &PgPool, server: Uuid) -> Uuid {
        let (id, _) = crate::test_support::compose_project(pool).await;
        sqlx::query("UPDATE compose_projects SET server_id = $2 WHERE id = $1")
            .bind(id)
            .bind(server)
            .execute(pool)
            .await
            .unwrap();
        id
    }

    async fn enqueue(pool: &PgPool, project: Uuid) -> Uuid {
        let id = Uuid::new_v4();
        db::compose_projects::enqueue_deployment(
            pool,
            id,
            project,
            "agent://t",
            false,
            "manual",
            None,
        )
        .await
        .unwrap();
        id
    }

    async fn finish(pool: &PgPool, deployment: Uuid) {
        db::compose_projects::finish_deployment(
            pool,
            deployment,
            panel_core::types::DeploymentStatus::Succeeded,
            None,
            None,
        )
        .await
        .unwrap();
    }

    async fn latest_run(pool: &PgPool, server: Uuid) -> (String, Option<String>) {
        sqlx::query_as(
            "SELECT status, error_message FROM docker_cleanup_runs
             WHERE server_id = $1 ORDER BY started_at DESC LIMIT 1",
        )
        .bind(server)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    async fn remove(pool: &PgPool, services: &[Uuid], servers: &[Uuid]) {
        for id in services {
            db::compose_projects::delete(pool, *id).await.unwrap();
        }
        for id in servers {
            db::servers::delete(pool, *id).await.unwrap();
        }
    }

    #[tokio::test]
    async fn cleanup_records_freed_space_failures_and_overlap() {
        let _guard = QUEUE_TESTS.lock().await;
        let pool = test_pool().await;
        let server = server(&pool).await;
        let adapter = Arc::new(MockAdapter::new());
        *adapter.prune_outcome.lock().unwrap() = Some(DockerPruneOutcome {
            images_freed_bytes: 1000,
            builder_freed_bytes: 234,
        });
        let run = run_cleanup(&pool, adapter.clone(), server, "manual", 168)
            .await
            .unwrap();
        assert_eq!(run.status, "succeeded");
        assert_eq!(
            (run.images_freed_bytes, run.builder_freed_bytes),
            (1000, 234)
        );
        assert_eq!(adapter.prunes.lock().unwrap()[0].older_than_hours, 168);

        adapter.fail_next_call("prune_docker");
        let failed = run_cleanup(&pool, adapter.clone(), server, "schedule", 24)
            .await
            .unwrap();
        assert_eq!(failed.status, "failed");
        assert!(failed.error_message.is_some());

        // Out-of-range ages never reach the agent.
        assert!(run_cleanup(&pool, adapter.clone(), server, "manual", 0)
            .await
            .is_err());
        assert_eq!(adapter.prunes.lock().unwrap().len(), 1);

        // A running row blocks another run.
        let StartRun::Started(running) = db::observability::start_run(&pool, server, "manual", 1)
            .await
            .unwrap()
        else {
            panic!("expected a started run");
        };
        assert!(matches!(
            run_cleanup(&pool, adapter.clone(), server, "manual", 168).await,
            Err(ApiError::Conflict(_))
        ));
        db::observability::finish_run(&pool, running.id, Ok((0, 0)))
            .await
            .unwrap();
        remove(&pool, &[], &[server]).await;
    }

    #[tokio::test]
    async fn cleanup_and_deployments_on_one_server_exclude_each_other() {
        let _guard = QUEUE_TESTS.lock().await;
        let pool = test_pool().await;
        let (busy, other) = (server(&pool).await, server(&pool).await);
        let (web, api) = (
            service_on(&pool, busy).await,
            service_on(&pool, other).await,
        );
        let adapter = Arc::new(MockAdapter::new());

        // A queued deployment holds back a cleanup of its server only.
        let queued = enqueue(&pool, web).await;
        match run_cleanup(&pool, adapter.clone(), busy, "manual", 168).await {
            Err(ApiError::Conflict(message)) => assert!(message.contains("deployment")),
            other => panic!("expected a conflict, got {other:?}"),
        }
        assert!(adapter.prunes.lock().unwrap().is_empty());
        let elsewhere = run_cleanup(&pool, adapter.clone(), other, "manual", 168)
            .await
            .unwrap();
        assert_eq!(elsewhere.status, "succeeded");

        // So does a running one.
        let job = db::compose_projects::claim_next_deployment_of(&pool, Some(web))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(job.id, queued);
        assert!(matches!(
            run_cleanup(&pool, adapter.clone(), busy, "schedule", 168).await,
            Err(ApiError::Conflict(_))
        ));
        assert_eq!(adapter.prunes.lock().unwrap().len(), 1);
        finish(&pool, job.id).await;

        // While the cleanup runs, nothing is claimed on its server; other
        // servers keep deploying.
        *adapter.prune_delay.lock().unwrap() = Some(Duration::from_millis(400));
        let (run, task) =
            start_cleanup(&pool, adapter.clone(), busy, "manual", 168, CLEANUP_TIMEOUT)
                .await
                .unwrap();
        assert_eq!(run.status, "running");
        let blocked = enqueue(&pool, web).await;
        let free = enqueue(&pool, api).await;
        assert!(
            db::compose_projects::claim_next_deployment_of(&pool, Some(web))
                .await
                .unwrap()
                .is_none()
        );
        let claimed = db::compose_projects::claim_next_deployment_of(&pool, Some(api))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(claimed.id, free);
        finish(&pool, free).await;

        // Once it finished, the held-back deployment is claimed.
        assert_eq!(task.await.unwrap().unwrap().status, "succeeded");
        let claimed = db::compose_projects::claim_next_deployment_of(&pool, Some(web))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(claimed.id, blocked);
        finish(&pool, blocked).await;
        remove(&pool, &[web, api], &[busy, other]).await;
    }

    #[tokio::test]
    async fn a_cleanup_always_finishes_its_row() {
        let _guard = QUEUE_TESTS.lock().await;
        let pool = test_pool().await;
        let server = server(&pool).await;
        let adapter = Arc::new(MockAdapter::new());

        // The caller goes away (closed request, scheduler timeout): the run
        // still finishes and stops holding back deployments.
        *adapter.prune_delay.lock().unwrap() = Some(Duration::from_millis(200));
        let dropped = tokio::time::timeout(
            Duration::from_millis(20),
            run_cleanup(&pool, adapter.clone(), server, "manual", 168),
        )
        .await;
        assert!(dropped.is_err());
        assert_eq!(latest_run(&pool, server).await.0, "running");
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert_eq!(latest_run(&pool, server).await.0, "succeeded");

        // An agent that never answers fails the run at the time limit.
        *adapter.prune_delay.lock().unwrap() = Some(Duration::from_secs(30));
        let (_, task) = start_cleanup(
            &pool,
            adapter.clone(),
            server,
            "schedule",
            168,
            Duration::from_millis(50),
        )
        .await
        .unwrap();
        let failed = task.await.unwrap().unwrap();
        assert_eq!(failed.status, "failed");
        assert!(failed.error_message.unwrap().contains("no result"));
        remove(&pool, &[], &[server]).await;
    }

    #[tokio::test]
    async fn a_lost_cleanup_does_not_hold_deployments_forever() {
        let _guard = QUEUE_TESTS.lock().await;
        let pool = test_pool().await;
        let server = server(&pool).await;
        let web = service_on(&pool, server).await;
        // A run left `running` by a panel-api that died long ago.
        sqlx::query(
            "INSERT INTO docker_cleanup_runs (id, trigger, status, older_than_hours, server_id, started_at)
             VALUES ($1, 'schedule', 'running', 168, $2, now() - interval '2 hours')",
        )
        .bind(Uuid::new_v4())
        .bind(server)
        .execute(&pool)
        .await
        .unwrap();
        let queued = enqueue(&pool, web).await;
        let claimed = db::compose_projects::claim_next_deployment_of(&pool, Some(web))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(claimed.id, queued);
        finish(&pool, queued).await;

        // The next start fails it and runs.
        let run = run_cleanup(&pool, Arc::new(MockAdapter::new()), server, "manual", 168)
            .await
            .unwrap();
        assert_eq!(run.status, "succeeded");
        let failed: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM docker_cleanup_runs WHERE server_id = $1 AND status = 'failed'",
        )
        .bind(server)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(failed, 1);
        remove(&pool, &[web], &[server]).await;
    }

    #[tokio::test]
    async fn every_server_has_its_own_log_cursor() {
        let _guard = QUEUE_TESTS.lock().await;
        let pool = test_pool().await;
        let id = Uuid::new_v4();
        db::servers::insert(
            &pool,
            db::servers::NewServer {
                id,
                name: &format!("o-{}", &id.simple().to_string()[..8]),
                transport: db::servers::Transport::SshSocket,
                address: "/run/scontrol/remote/o.sock",
                max_services: None,
            },
        )
        .await
        .unwrap();
        let local_before = db::observability::cursor(&pool, LOCAL).await.unwrap();
        let adapter = MockAdapter::new();
        adapter
            .nginx_chunks
            .lock()
            .unwrap()
            .push_back(NginxStatsChunk {
                log_present: true,
                file_id: Some(7),
                next_offset: 512,
                lines: 3,
                buckets: vec![bucket("remote.example.com", Utc::now(), 3)],
                skipped: 0,
            });
        assert_eq!(collect_stats(&pool, &adapter, id).await.unwrap(), 3);
        let remote = db::observability::cursor(&pool, id).await.unwrap();
        assert_eq!((remote.file_id, remote.byte_offset), (Some(7), 512));
        let local_after = db::observability::cursor(&pool, LOCAL).await.unwrap();
        assert_eq!(local_after.byte_offset, local_before.byte_offset);
        assert_eq!(local_after.file_id, local_before.file_id);

        // Cleanups run one at a time per server, not across servers.
        let StartRun::Started(a) = db::observability::start_run(&pool, id, "manual", 1)
            .await
            .unwrap()
        else {
            panic!("expected a started run");
        };
        assert!(matches!(
            db::observability::start_run(&pool, id, "manual", 1)
                .await
                .unwrap(),
            StartRun::CleanupRunning
        ));
        db::observability::finish_run(&pool, a.id, Ok((0, 0)))
            .await
            .unwrap();
        db::servers::delete(&pool, id).await.unwrap();
    }
}
