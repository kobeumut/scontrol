//! Background work for D4 observability: reading the Nginx access-log
//! statistics once a minute and running the scheduled Docker cleanup.
//!
//! Costs when idle: one agent call a minute (a stat of the log file when
//! nothing is new) and one settings query every five minutes.

use crate::db;
use crate::db::observability::{CleanupRun, CleanupSettings};
use crate::error::{ApiError, ApiResult};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use panel_core::adapters::{AdapterPort, PruneDockerParams, ReadNginxStatsParams};
use panel_core::error::CoreError;
use sqlx::PgPool;
use std::sync::Arc;
use std::time::Duration;

const STATS_POLL: Duration = Duration::from_secs(60);
const CLEANUP_CHECK: Duration = Duration::from_secs(300);
/// Bytes of log asked for per agent call, and calls per poll (so a very
/// large first backlog is worked off over several polls, not one).
const READ_BYTES: u32 = 4 * 1024 * 1024;
const MAX_READS_PER_POLL: usize = 16;
/// Days of per-minute statistics kept.
pub const STATS_RETENTION_DAYS: i32 = 30;

pub fn start(db: PgPool, agent: Arc<dyn AdapterPort>) {
    let (pool, adapter) = (db.clone(), agent.clone());
    tokio::spawn(async move {
        let mut polls = 0u32;
        loop {
            if let Err(error) = collect_stats(&pool, adapter.as_ref()).await {
                tracing::warn!(%error, "reading nginx request statistics failed");
                let _ = db::observability::set_cursor_error(&pool, &error.to_string()).await;
            }
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
            if let Err(error) = scheduled_cleanup_tick(&db, agent.as_ref()).await {
                tracing::warn!(%error, "scheduled docker cleanup check failed");
            }
        }
    });
}

/// Reads the log from the stored cursor to its end (bounded per call).
/// Returns the number of requests counted.
pub async fn collect_stats(db: &PgPool, agent: &dyn AdapterPort) -> ApiResult<u64> {
    let mut total = 0;
    for _ in 0..MAX_READS_PER_POLL {
        let cursor = db::observability::cursor(db).await?;
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
            db::observability::apply_chunk(db, &chunk.buckets, chunk.file_id, chunk.next_offset)
                .await?;
        } else {
            db::observability::apply_chunk(db, &[], chunk.file_id, chunk.next_offset).await?;
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
    trigger: &str,
    older_than_hours: u32,
) -> ApiResult<CleanupRun> {
    panel_core::observability::prune_until_filter(older_than_hours)
        .map_err(|m| ApiError::Core(CoreError::Validation(m)))?;
    let run = db::observability::start_run(db, trigger, older_than_hours as i32)
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

async fn scheduled_cleanup_tick(db: &PgPool, agent: &dyn AdapterPort) -> ApiResult<()> {
    let settings = db::observability::cleanup_settings(db).await?;
    let last = db::observability::last_scheduled_start(db).await?;
    if cleanup_due(&settings, last, Utc::now()) {
        match run_cleanup(db, agent, "schedule", settings.older_than_hours as u32).await {
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
        let counted = collect_stats(&pool, &adapter).await.unwrap();
        assert_eq!(counted, 5);
        let cursor = db::observability::cursor(&pool).await.unwrap();
        assert_eq!((cursor.file_id, cursor.byte_offset), (Some(7), 250));
        let totals = db::observability::host_totals(&pool, minute - ChronoDuration::hours(1))
            .await
            .unwrap();
        let a = totals.iter().find(|t| t.host == "a.com").unwrap();
        assert_eq!((a.requests, a.status_2xx, a.bytes_sent), (5, 5, 50));
        assert!((a.avg_latency_ms - 20.0).abs() < 1e-9);
        // The mock now returns an empty chunk at the stored offset: no change.
        assert_eq!(collect_stats(&pool, &adapter).await.unwrap(), 0);
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
        let run = run_cleanup(&pool, &adapter, "manual", 168).await.unwrap();
        assert_eq!(run.status, "succeeded");
        assert_eq!(
            (run.images_freed_bytes, run.builder_freed_bytes),
            (1000, 234)
        );
        assert_eq!(adapter.prunes.lock().unwrap()[0].older_than_hours, 168);

        adapter.fail_next_call("prune_docker");
        let failed = run_cleanup(&pool, &adapter, "schedule", 24).await.unwrap();
        assert_eq!(failed.status, "failed");
        assert!(failed.error_message.is_some());

        // Out-of-range ages never reach the agent.
        assert!(run_cleanup(&pool, &adapter, "manual", 0).await.is_err());
        assert_eq!(adapter.prunes.lock().unwrap().len(), 1);

        // A running row blocks another run.
        let running = db::observability::start_run(&pool, "manual", 1)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            run_cleanup(&pool, &adapter, "manual", 168).await,
            Err(ApiError::Conflict(_))
        ));
        db::observability::finish_run(&pool, running.id, Ok((0, 0)))
            .await
            .unwrap();
    }
}
