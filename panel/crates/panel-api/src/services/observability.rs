//! Background work for D4 observability: reading the Nginx access-log
//! statistics once a minute and running the scheduled Docker cleanup.
//!
//! Costs when idle: one agent call a minute (a stat of the log file when
//! nothing is new) and one settings query every five minutes.

use crate::db;
use crate::db::observability::{CleanupRun, CleanupSettings};
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
                    async move { scheduled_cleanup_tick(db, agent.as_ref(), server).await }
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

/// Runs one cleanup and records it. `Conflict` while one is running.
pub async fn run_cleanup(
    db: &PgPool,
    agent: &dyn AdapterPort,
    server: Uuid,
    trigger: &str,
    older_than_hours: u32,
) -> ApiResult<CleanupRun> {
    panel_core::observability::prune_until_filter(older_than_hours)
        .map_err(|m| ApiError::Core(CoreError::Validation(m)))?;
    let run = db::observability::start_run(db, server, trigger, older_than_hours as i32)
        .await?
        .ok_or_else(|| ApiError::Conflict("a Docker cleanup is already running".into()))?;
    let result = agent
        .prune_docker(PruneDockerParams { older_than_hours })
        .await;
    let finished = match &result {
        Ok(o) => {
            db::observability::finish_run(
                db,
                run.id,
                Ok((o.images_freed_bytes, o.builder_freed_bytes)),
            )
            .await?
        }
        Err(error) => {
            // The agent's message can carry Docker's stderr: keep it short.
            let message: String = error.to_string().chars().take(500).collect();
            db::observability::finish_run(db, run.id, Err(&message)).await?
        }
    };
    let _ = db::observability::trim_runs(db).await;
    Ok(finished)
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

async fn scheduled_cleanup_tick(
    db: &PgPool,
    agent: &dyn AdapterPort,
    server: Uuid,
) -> ApiResult<()> {
    let settings = db::observability::cleanup_settings(db).await?;
    let last = db::observability::last_scheduled_start(db, server).await?;
    if cleanup_due(&settings, last, Utc::now()) {
        match run_cleanup(
            db,
            agent,
            server,
            "schedule",
            settings.older_than_hours as u32,
        )
        .await
        {
            Ok(run) => tracing::info!(status = %run.status, "scheduled docker cleanup finished"),
            Err(ApiError::Conflict(_)) => {}
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

    #[tokio::test]
    async fn cleanup_records_freed_space_failures_and_overlap() {
        let _guard = QUEUE_TESTS.lock().await;
        let pool = test_pool().await;
        sqlx::query("DELETE FROM docker_cleanup_runs")
            .execute(&pool)
            .await
            .unwrap();
        let adapter = MockAdapter::new();
        *adapter.prune_outcome.lock().unwrap() = Some(DockerPruneOutcome {
            images_freed_bytes: 1000,
            builder_freed_bytes: 234,
        });
        let run = run_cleanup(&pool, &adapter, LOCAL, "manual", 168)
            .await
            .unwrap();
        assert_eq!(run.status, "succeeded");
        assert_eq!(
            (run.images_freed_bytes, run.builder_freed_bytes),
            (1000, 234)
        );
        assert_eq!(adapter.prunes.lock().unwrap()[0].older_than_hours, 168);

        adapter.fail_next_call("prune_docker");
        let failed = run_cleanup(&pool, &adapter, LOCAL, "schedule", 24)
            .await
            .unwrap();
        assert_eq!(failed.status, "failed");
        assert!(failed.error_message.is_some());

        // Out-of-range ages never reach the agent.
        assert!(run_cleanup(&pool, &adapter, LOCAL, "manual", 0)
            .await
            .is_err());
        assert_eq!(adapter.prunes.lock().unwrap().len(), 1);

        // A running row blocks another run.
        let running = db::observability::start_run(&pool, LOCAL, "manual", 1)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            run_cleanup(&pool, &adapter, LOCAL, "manual", 168).await,
            Err(ApiError::Conflict(_))
        ));
        db::observability::finish_run(&pool, running.id, Ok((0, 0)))
            .await
            .unwrap();
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
        let a = db::observability::start_run(&pool, id, "manual", 1)
            .await
            .unwrap()
            .unwrap();
        assert!(db::observability::start_run(&pool, id, "manual", 1)
            .await
            .unwrap()
            .is_none());
        db::observability::finish_run(&pool, a.id, Ok((0, 0)))
            .await
            .unwrap();
        db::servers::delete(&pool, id).await.unwrap();
    }
}
