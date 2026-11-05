//! Metrics collection, downsampling, and threshold alerts.
//!
//! The agent samples `/proc` and cgroup v2 in-process (see `panel-agent`
//! `adapters/metrics.rs`). Every [`COLLECT_INTERVAL`] this collector takes
//! the agent's latest snapshot, stores it as a raw 30 s bucket, and feeds
//! each watched metric through its [`AlertTracker`]. Only an edge (normal →
//! firing, firing → normal) sends a notification (`metric_alert`,
//! `metric_resolved`); hysteresis and consecutive-sample counts keep a value
//! hovering at the threshold from sending a stream of alerts. Tracker state
//! lives in `metric_alert_state`, so a restart neither repeats nor loses an
//! alert.
//!
//! Every server is collected from its own agent, at once and each bounded
//! by [`SERVER_TIMEOUT`]. Host metrics and alert state are kept per server;
//! a remote server's alerts name it.
//!
//! Every [`MAINTENANCE_INTERVAL`] raw rows are averaged into 5 min buckets,
//! those into 1 h buckets, and each resolution is pruned to its retention
//! (24 h / 7 d / 30 d, `db::metrics::RETENTION`).

use crate::db;
use crate::db::metrics::{AlertSettings, ContainerSample, ServerSample};
use crate::db::servers::Server;
use crate::services::agent_pool::AgentPool;
use crate::services::notifications::{Notification, Notifier};
use chrono::{DateTime, Utc};
use panel_core::adapters::AdapterPort;
use panel_core::metrics::{
    AlertTransition, MetricsSnapshot, COARSE_RESOLUTION_SECS, MEDIUM_RESOLUTION_SECS,
    RAW_RESOLUTION_SECS,
};
use serde_json::json;
use sqlx::PgPool;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use uuid::Uuid;

pub const COLLECT_INTERVAL: Duration = Duration::from_secs(RAW_RESOLUTION_SECS as u64);
const MAINTENANCE_INTERVAL: Duration = Duration::from_secs(5 * 60);
/// Longest one server's collection may take.
const SERVER_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Clone, Default)]
pub struct MetricsCollector {
    latest: Arc<RwLock<HashMap<Uuid, MetricsSnapshot>>>,
}

impl MetricsCollector {
    pub fn new() -> Self {
        Self::default()
    }

    /// The most recent snapshot collected from `server`'s agent.
    pub fn latest_for(&self, server: Uuid) -> Option<MetricsSnapshot> {
        self.latest
            .read()
            .ok()
            .and_then(|latest| latest.get(&server).cloned())
    }

    #[cfg(test)]
    pub fn set_latest(&self, snapshot: MetricsSnapshot) {
        self.set_latest_for(db::servers::LOCAL_SERVER_ID, snapshot);
    }

    pub fn set_latest_for(&self, server: Uuid, snapshot: MetricsSnapshot) {
        if let Ok(mut latest) = self.latest.write() {
            latest.insert(server, snapshot);
        }
    }

    /// Spawns the collector loop. Call once, from the HTTP process.
    pub fn start(&self, db: PgPool, agents: AgentPool, notifier: Notifier) {
        let collector = self.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(COLLECT_INTERVAL);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let mut last_maintenance: Option<tokio::time::Instant> = None;
            let mut warned: HashSet<Uuid> = HashSet::new();
            loop {
                ticker.tick().await;
                let servers = match db::servers::list(&db).await {
                    Ok(servers) => servers,
                    Err(error) => {
                        tracing::warn!(%error, "listing servers for metrics failed");
                        Vec::new()
                    }
                };
                let passes = servers.iter().map(|server| {
                    let (db, agents, notifier, collector) = (&db, &agents, &notifier, &collector);
                    async move {
                        let previous = collector.latest_for(server.id).map(|s| s.sampled_at);
                        let pass = async {
                            let agent = agents
                                .for_server_id(db, server.id)
                                .await
                                .map_err(|e| anyhow::anyhow!(e.to_string()))?;
                            collect_server(db, agent.as_ref(), notifier, server, previous).await
                        };
                        let result = tokio::time::timeout(SERVER_TIMEOUT, pass)
                            .await
                            .unwrap_or_else(|_| Err(anyhow::anyhow!("timed out")));
                        (server, result)
                    }
                });
                for (server, result) in futures::future::join_all(passes).await {
                    match result {
                        Ok(Some(snapshot)) => {
                            warned.remove(&server.id);
                            collector.set_latest_for(server.id, snapshot);
                        }
                        Ok(None) => {}
                        Err(error) => {
                            // Once per outage, not every 30 s.
                            if warned.insert(server.id) {
                                tracing::warn!(server = %server.name, %error, "collecting metrics failed");
                            }
                        }
                    }
                }
                if last_maintenance.is_none_or(|at| at.elapsed() >= MAINTENANCE_INTERVAL) {
                    last_maintenance = Some(tokio::time::Instant::now());
                    if let Err(error) = maintain(&db, Utc::now()).await {
                        tracing::warn!(%error, "metrics rollup failed");
                    }
                }
            }
        });
    }
}

/// Rolls raw rows up into 5 min and 1 h buckets and prunes old rows. The
/// windows overlap the previous pass, so a missed pass loses nothing.
pub async fn maintain(db: &PgPool, now: DateTime<Utc>) -> sqlx::Result<()> {
    db::metrics::rollup(
        db,
        RAW_RESOLUTION_SECS,
        MEDIUM_RESOLUTION_SECS,
        now - chrono::Duration::hours(1),
    )
    .await?;
    db::metrics::rollup(
        db,
        MEDIUM_RESOLUTION_SECS,
        COARSE_RESOLUTION_SECS,
        now - chrono::Duration::hours(3),
    )
    .await?;
    db::metrics::prune(db, now).await?;
    Ok(())
}

/// One collection pass on this host.
#[cfg(test)]
pub async fn collect_once(
    db: &PgPool,
    agent: &dyn AdapterPort,
    notifier: &Notifier,
    previous: Option<DateTime<Utc>>,
) -> anyhow::Result<Option<MetricsSnapshot>> {
    let local = db::servers::find(db, db::servers::LOCAL_SERVER_ID)
        .await?
        .ok_or_else(|| anyhow::anyhow!("the local server row is missing"))?;
    collect_server(db, agent, notifier, &local, previous).await
}

/// One collection pass on `server`, asking its `agent`. Returns the
/// snapshot when it was new (a snapshot already collected, same
/// `sampled_at`, is not stored or evaluated again).
pub async fn collect_server(
    db: &PgPool,
    agent: &dyn AdapterPort,
    notifier: &Notifier,
    server: &Server,
    previous: Option<DateTime<Utc>>,
) -> anyhow::Result<Option<MetricsSnapshot>> {
    let Some(snapshot) = agent.metrics_snapshot().await? else {
        return Ok(None);
    };
    if previous == Some(snapshot.sampled_at) {
        return Ok(None);
    }
    let projects = db::metrics::project_slugs_on(db, server.id).await?;
    let bucket = db::metrics::bucket_start(snapshot.sampled_at, RAW_RESOLUTION_SECS);
    db::metrics::insert_server_sample(db, server.id, bucket, &server_sample(&snapshot)).await?;
    let services = service_samples(&snapshot, &projects);
    db::metrics::insert_container_samples(db, bucket, &services).await?;

    let label = (server.id != db::servers::LOCAL_SERVER_ID).then_some(server.name.as_str());
    for alert in evaluate_alerts(db, server.id, label, &snapshot, &services, &projects).await? {
        notifier.broadcast(db.clone(), alert.notification());
    }
    Ok(Some(snapshot))
}

pub fn server_sample(snapshot: &MetricsSnapshot) -> ServerSample {
    let host = &snapshot.host;
    let (used, total) = host.disks.iter().fold((0u64, 0u64), |(u, t), d| {
        (
            u.saturating_add(d.used_bytes),
            t.saturating_add(d.total_bytes),
        )
    });
    ServerSample {
        cpu_pct: host.cpu_pct,
        mem_used_bytes: to_i64(host.mem_used_bytes),
        mem_total_bytes: to_i64(host.mem_total_bytes),
        load1: host.load.one,
        disk_used_bytes: to_i64(used),
        disk_total_bytes: to_i64(total),
        disk_read_bps: host.disk_read_bps,
        disk_write_bps: host.disk_write_bps,
    }
}

/// Sums containers per Compose service of a panel project. Containers of
/// other Compose projects on the host are left out. A service's limit is
/// the sum of its replicas' limits, or none if any replica is unlimited.
pub fn service_samples(
    snapshot: &MetricsSnapshot,
    projects: &[(Uuid, String, String)],
) -> Vec<ContainerSample> {
    let by_slug: HashMap<&str, Uuid> = projects
        .iter()
        .map(|(id, slug, _)| (slug.as_str(), *id))
        .collect();
    let mut services: BTreeMap<(Uuid, String), ContainerSample> = BTreeMap::new();
    for container in &snapshot.containers {
        let Some(&project_id) = by_slug.get(container.project.as_str()) else {
            continue;
        };
        let limit = container.mem_limit_bytes.map(to_i64);
        services
            .entry((project_id, container.service.clone()))
            .and_modify(|s| {
                s.cpu_pct += container.cpu_pct;
                s.mem_bytes = s.mem_bytes.saturating_add(to_i64(container.mem_bytes));
                s.mem_limit_bytes = s
                    .mem_limit_bytes
                    .zip(limit)
                    .map(|(a, b)| a.saturating_add(b));
                s.io_read_bps += container.io_read_bps;
                s.io_write_bps += container.io_write_bps;
            })
            .or_insert_with(|| ContainerSample {
                compose_project_id: project_id,
                service: container.service.clone(),
                cpu_pct: container.cpu_pct,
                mem_bytes: to_i64(container.mem_bytes),
                mem_limit_bytes: limit,
                io_read_bps: container.io_read_bps,
                io_write_bps: container.io_write_bps,
            });
    }
    services.into_values().collect()
}

fn to_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

/// One watched value in this snapshot.
#[derive(Debug, Clone, PartialEq)]
pub struct Watched {
    pub key: String,
    pub label: String,
    pub value: f64,
    pub threshold: f32,
}

/// The metrics alerts watch: host CPU, host memory, every disk mount, and
/// the memory of each service that has a memory limit.
pub fn watched_metrics(
    snapshot: &MetricsSnapshot,
    services: &[ContainerSample],
    projects: &[(Uuid, String, String)],
    settings: &AlertSettings,
) -> Vec<Watched> {
    let host = &snapshot.host;
    let mut watched = vec![
        Watched {
            key: "host:cpu".into(),
            label: "CPU usage".into(),
            value: host.cpu_pct,
            threshold: settings.cpu_pct,
        },
        Watched {
            key: "host:memory".into(),
            label: "Memory usage".into(),
            value: host.mem_used_pct(),
            threshold: settings.memory_pct,
        },
    ];
    for disk in &host.disks {
        watched.push(Watched {
            key: format!("disk:{}", disk.mount_point),
            label: format!("Disk usage of {}", disk.mount_point),
            value: disk.used_pct(),
            threshold: settings.disk_pct,
        });
    }
    let names: HashMap<Uuid, &str> = projects
        .iter()
        .map(|(id, _, name)| (*id, name.as_str()))
        .collect();
    for service in services {
        let Some(limit) = service.mem_limit_bytes.filter(|l| *l > 0) else {
            continue;
        };
        let project = names
            .get(&service.compose_project_id)
            .copied()
            .unwrap_or("?");
        watched.push(Watched {
            key: format!(
                "container:{}:{}:memory",
                service.compose_project_id, service.service
            ),
            label: format!("Memory of {project}/{} (of its limit)", service.service),
            value: panel_core::metrics::percent_of(service.mem_bytes.max(0) as u64, limit as u64),
            threshold: settings.container_memory_pct,
        });
    }
    watched
}

/// A fired or resolved alert.
#[derive(Debug, Clone, PartialEq)]
pub struct AlertEvent {
    pub metric: Watched,
    pub fired: bool,
    pub resolve_below: f64,
}

impl AlertEvent {
    pub fn notification(&self) -> Notification {
        let m = &self.metric;
        let (event, title, body) = if self.fired {
            (
                "metric_alert",
                format!("{} above {:.0}%: {:.1}%", m.label, m.threshold, m.value),
                format!(
                    "{} is {:.1}%, at or above the {:.0}% threshold.\nIt resolves below {:.0}%.",
                    m.label, m.value, m.threshold, self.resolve_below
                ),
            )
        } else {
            (
                "metric_resolved",
                format!("Resolved: {} back to {:.1}%", m.label, m.value),
                format!(
                    "{} is {:.1}%, below {:.0}% again (threshold {:.0}%).",
                    m.label, m.value, self.resolve_below, m.threshold
                ),
            )
        };
        Notification {
            event,
            title,
            body,
            data: json!({
                "metric": {
                    "key": m.key,
                    "label": m.label,
                    "value": m.value,
                    "threshold": m.threshold,
                    "resolve_below": self.resolve_below,
                    "state": if self.fired { "firing" } else { "resolved" },
                },
            }),
            urgent: self.fired,
        }
    }
}

/// Feeds every watched metric through its stored tracker and returns the
/// edges this snapshot caused. Disabled alerts forget all state, so turning
/// them back on starts clean.
pub async fn evaluate_alerts(
    db: &PgPool,
    server: Uuid,
    server_name: Option<&str>,
    snapshot: &MetricsSnapshot,
    services: &[ContainerSample],
    projects: &[(Uuid, String, String)],
) -> sqlx::Result<Vec<AlertEvent>> {
    let settings = db::metrics::alert_settings(db).await?;
    if !settings.enabled {
        db::metrics::clear_alert_states(db).await?;
        return Ok(Vec::new());
    }
    let states = db::metrics::alert_states(db, server).await?;
    let mut watched = watched_metrics(snapshot, services, projects, &settings);
    // A remote server's alerts say which server they are about.
    if let Some(name) = server_name {
        for metric in &mut watched {
            metric.label = format!("{} on {name}", metric.label);
        }
    }
    let mut events = Vec::new();
    for metric in &watched {
        let rule = settings.rule(metric.threshold);
        let mut tracker = states
            .get(&metric.key)
            .map(|row| row.tracker())
            .unwrap_or_default();
        let transition = tracker.observe(metric.value, &rule);
        let changed = transition != AlertTransition::None;
        db::metrics::save_alert_state(db, server, &metric.key, tracker, metric.value, changed)
            .await?;
        if changed {
            events.push(AlertEvent {
                metric: metric.clone(),
                fired: transition == AlertTransition::Fired,
                resolve_below: rule.threshold - rule.hysteresis,
            });
        }
    }
    let keep: Vec<String> = watched.into_iter().map(|m| m.key).collect();
    db::metrics::delete_alert_states_except(db, server, &keep).await?;
    Ok(events)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    const LOCAL: Uuid = db::servers::LOCAL_SERVER_ID;
    use crate::test_support;
    use panel_core::adapters::mock::MockAdapter;
    use panel_core::metrics::{ContainerMetrics, DiskUsage, HostMetrics, LoadAvg};

    pub(crate) fn snapshot(at: DateTime<Utc>, cpu: f64) -> MetricsSnapshot {
        MetricsSnapshot {
            sampled_at: at,
            interval_secs: 15.0,
            sample_micros: 120,
            host: HostMetrics {
                cpu_pct: cpu,
                cpu_count: 4,
                mem_total_bytes: 8 << 30,
                mem_used_bytes: 2 << 30,
                swap_total_bytes: 0,
                swap_used_bytes: 0,
                load: LoadAvg {
                    one: 0.5,
                    five: 0.4,
                    fifteen: 0.3,
                },
                disk_read_bps: 1000.0,
                disk_write_bps: 2000.0,
                disks: vec![DiskUsage {
                    mount_point: "/".into(),
                    fs_type: "ext4".into(),
                    total_bytes: 100 << 30,
                    used_bytes: 40 << 30,
                }],
            },
            containers: vec![],
        }
    }

    fn container(project: &str, service: &str, mem: u64, limit: Option<u64>) -> ContainerMetrics {
        ContainerMetrics {
            container_id: format!("{:0>64}", service.len()),
            name: format!("{project}-{service}-1"),
            project: project.into(),
            service: service.into(),
            cpu_pct: 50.0,
            mem_bytes: mem,
            mem_limit_bytes: limit,
            io_read_bps: 1.0,
            io_write_bps: 2.0,
        }
    }

    #[test]
    fn services_sum_replicas_and_skip_foreign_projects() {
        let project = Uuid::new_v4();
        let projects = vec![(project, "shop".to_string(), "Shop".to_string())];
        let mut snap = snapshot(Utc::now(), 10.0);
        snap.containers = vec![
            container("shop", "web", 100, Some(1000)),
            container("shop", "web", 300, Some(1000)),
            container("shop", "worker", 50, None),
            container("shop", "cache", 10, Some(100)),
            container("shop", "cache", 10, None),
            container("someone-else", "web", 999, None),
        ];
        let services = service_samples(&snap, &projects);
        assert_eq!(services.len(), 3);
        let web = services.iter().find(|s| s.service == "web").unwrap();
        assert_eq!(web.mem_bytes, 400);
        assert_eq!(web.mem_limit_bytes, Some(2000));
        assert_eq!(web.cpu_pct, 100.0);
        let cache = services.iter().find(|s| s.service == "cache").unwrap();
        assert_eq!(cache.mem_limit_bytes, None);

        let settings = AlertSettings {
            enabled: true,
            cpu_pct: 85.0,
            memory_pct: 85.0,
            disk_pct: 85.0,
            container_memory_pct: 90.0,
            hysteresis_pct: 5.0,
            trigger_samples: 3,
            clear_samples: 3,
        };
        let watched = watched_metrics(&snap, &services, &projects, &settings);
        let keys: Vec<&str> = watched.iter().map(|w| w.key.as_str()).collect();
        assert_eq!(
            keys,
            vec![
                "host:cpu",
                "host:memory",
                "disk:/",
                &format!("container:{project}:web:memory")
            ]
        );
        assert_eq!(watched[1].value, 25.0);
        assert_eq!(watched[2].value, 40.0);
        assert_eq!(watched[3].value, 20.0);
    }

    #[test]
    fn notifications_describe_both_edges() {
        let metric = Watched {
            key: "host:cpu".into(),
            label: "CPU usage".into(),
            value: 93.25,
            threshold: 85.0,
        };
        let fired = AlertEvent {
            metric: metric.clone(),
            fired: true,
            resolve_below: 80.0,
        }
        .notification();
        assert_eq!(fired.event, "metric_alert");
        assert_eq!(fired.title, "CPU usage above 85%: 93.2%");
        assert!(fired.urgent);
        assert_eq!(fired.data["metric"]["state"], "firing");
        let resolved = AlertEvent {
            metric: Watched {
                value: 42.0,
                ..metric
            },
            fired: false,
            resolve_below: 80.0,
        }
        .notification();
        assert_eq!(resolved.event, "metric_resolved");
        assert!(!resolved.urgent);
    }

    /// Collector against PostgreSQL: storage, dedup of a re-read snapshot,
    /// edge-triggered alerts with hysteresis, persistence, and rollups.
    #[tokio::test]
    async fn collector_stores_samples_and_alerts_on_edges_only() {
        let _serial = test_support::METRICS_TESTS.lock().await;
        let pool = test_support::test_pool().await;
        test_support::reset_metrics(&pool).await;
        let adapter = Arc::new(MockAdapter::new());
        let notifier = test_support::app_state(pool.clone(), adapter.clone()).notifier;
        db::metrics::update_alert_settings(
            &pool,
            &AlertSettings {
                enabled: true,
                cpu_pct: 85.0,
                memory_pct: 85.0,
                disk_pct: 85.0,
                container_memory_pct: 90.0,
                hysteresis_pct: 5.0,
                trigger_samples: 2,
                clear_samples: 2,
            },
        )
        .await
        .unwrap();

        // No snapshot yet: nothing stored.
        assert!(collect_once(&pool, adapter.as_ref(), &notifier, None)
            .await
            .unwrap()
            .is_none());

        let start = db::metrics::bucket_start(Utc::now(), 300) - chrono::Duration::minutes(30);
        // CPU: high, high (fire), hovering in the band, low, low (resolve).
        let cpu = [90.0, 95.0, 83.0, 86.0, 82.0, 40.0, 30.0, 20.0];
        let mut edges = Vec::new();
        let mut previous = None;
        for (i, value) in cpu.iter().enumerate() {
            let at = start + chrono::Duration::seconds(30 * i as i64);
            *adapter.metrics.lock().unwrap() = Some(snapshot(at, *value));
            let collected = collect_once(&pool, adapter.as_ref(), &notifier, previous)
                .await
                .unwrap()
                .expect("new snapshot");
            // The same snapshot read again is not stored or evaluated twice.
            assert!(collect_once(&pool, adapter.as_ref(), &notifier, Some(at))
                .await
                .unwrap()
                .is_none());
            previous = Some(collected.sampled_at);
            let state = db::metrics::alert_states(&pool, LOCAL).await.unwrap();
            edges.push(state["host:cpu"].firing);
        }
        assert_eq!(
            edges,
            vec![false, true, true, true, true, true, false, false]
        );

        let raw = db::metrics::server_series(&pool, LOCAL, 30, start)
            .await
            .unwrap();
        assert_eq!(raw.len(), cpu.len());
        assert_eq!(raw[1].cpu_pct, 95.0);
        assert_eq!(raw[0].disk_total_bytes, 100 << 30);

        // Rollup averages into 5 min buckets; repeating it is idempotent.
        maintain(&pool, start + chrono::Duration::minutes(10))
            .await
            .unwrap();
        maintain(&pool, start + chrono::Duration::minutes(10))
            .await
            .unwrap();
        let medium = db::metrics::server_series(&pool, LOCAL, 300, start)
            .await
            .unwrap();
        assert_eq!(medium.len(), 1);
        let mean = cpu.iter().sum::<f64>() / cpu.len() as f64;
        assert!((f64::from(medium[0].cpu_pct) - mean).abs() < 0.01);

        // Retention: raw rows older than 24 h are pruned.
        maintain(&pool, start + chrono::Duration::hours(25))
            .await
            .unwrap();
        assert!(db::metrics::server_series(&pool, LOCAL, 30, start)
            .await
            .unwrap()
            .is_empty());
        assert_eq!(
            db::metrics::server_series(&pool, LOCAL, 300, start)
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn disk_and_service_alerts_and_stale_state_cleanup() {
        let _serial = test_support::METRICS_TESTS.lock().await;
        let pool = test_support::test_pool().await;
        test_support::reset_metrics(&pool).await;
        let project = test_support::compose_project(&pool).await;
        let projects = db::metrics::project_slugs(&pool).await.unwrap();
        db::metrics::update_alert_settings(
            &pool,
            &AlertSettings {
                enabled: true,
                cpu_pct: 85.0,
                memory_pct: 85.0,
                disk_pct: 85.0,
                container_memory_pct: 90.0,
                hysteresis_pct: 5.0,
                trigger_samples: 1,
                clear_samples: 1,
            },
        )
        .await
        .unwrap();
        let mut snap = snapshot(Utc::now(), 10.0);
        snap.host.disks[0].used_bytes = 95 << 30;
        snap.containers = vec![container(&project.1, "web", 950, Some(1000))];
        let services = service_samples(&snap, &projects);
        let events = evaluate_alerts(&pool, LOCAL, None, &snap, &services, &projects)
            .await
            .unwrap();
        let fired: Vec<&str> = events.iter().map(|e| e.metric.key.as_str()).collect();
        assert_eq!(
            fired,
            vec!["disk:/", &format!("container:{}:web:memory", project.0)]
        );
        assert!(events.iter().all(|e| e.fired));
        // Container metrics of the project are stored.
        db::metrics::insert_container_samples(&pool, snap.sampled_at, &services)
            .await
            .unwrap();
        let series = db::metrics::container_series(
            &pool,
            project.0,
            30,
            snap.sampled_at - chrono::Duration::minutes(1),
        )
        .await
        .unwrap();
        assert_eq!(series.len(), 1);

        // The service went away: its state is dropped without a "resolved".
        snap.containers.clear();
        let events = evaluate_alerts(&pool, LOCAL, None, &snap, &[], &projects)
            .await
            .unwrap();
        assert!(events.is_empty());
        let states = db::metrics::alert_states(&pool, LOCAL).await.unwrap();
        assert!(states.keys().all(|k| !k.starts_with("container:")));
        assert!(states["disk:/"].firing);

        // Disabling alerts forgets every state.
        let mut settings = db::metrics::alert_settings(&pool).await.unwrap();
        settings.enabled = false;
        db::metrics::update_alert_settings(&pool, &settings)
            .await
            .unwrap();
        evaluate_alerts(&pool, LOCAL, None, &snap, &[], &projects)
            .await
            .unwrap();
        assert!(db::metrics::alert_states(&pool, LOCAL)
            .await
            .unwrap()
            .is_empty());
        settings.enabled = true;
        db::metrics::update_alert_settings(&pool, &settings)
            .await
            .unwrap();
    }

    /// A remote server's host metrics and alerts are its own: they neither
    /// mix with this host's series nor share its alert state, and its alerts
    /// name it.
    #[tokio::test]
    async fn servers_keep_separate_metrics_and_alerts() {
        let _serial = test_support::METRICS_TESTS.lock().await;
        let pool = test_support::test_pool().await;
        test_support::reset_metrics(&pool).await;
        let adapter = Arc::new(MockAdapter::new());
        let notifier = test_support::app_state(pool.clone(), adapter.clone()).notifier;
        let mut settings = db::metrics::alert_settings(&pool).await.unwrap();
        settings.trigger_samples = 1;
        settings.clear_samples = 1;
        db::metrics::update_alert_settings(&pool, &settings)
            .await
            .unwrap();
        let id = Uuid::new_v4();
        let name = format!("m-{}", &id.simple().to_string()[..8]);
        db::servers::insert(
            &pool,
            db::servers::NewServer {
                id,
                name: &name,
                transport: db::servers::Transport::SshSocket,
                address: "/run/scontrol/remote/m.sock",
                max_services: None,
            },
        )
        .await
        .unwrap();
        let remote = db::servers::find(&pool, id).await.unwrap().unwrap();

        let at = Utc::now();
        *adapter.metrics.lock().unwrap() = Some(snapshot(at, 97.0));
        collect_server(&pool, adapter.as_ref(), &notifier, &remote, None)
            .await
            .unwrap()
            .expect("a new snapshot");
        *adapter.metrics.lock().unwrap() = Some(snapshot(at, 10.0));
        collect_once(&pool, adapter.as_ref(), &notifier, None)
            .await
            .unwrap()
            .expect("a new snapshot");

        let since = at - chrono::Duration::minutes(5);
        let remote_points = db::metrics::server_series(&pool, id, 30, since)
            .await
            .unwrap();
        let local_points = db::metrics::server_series(&pool, LOCAL, 30, since)
            .await
            .unwrap();
        assert_eq!(remote_points.len(), 1);
        assert_eq!(local_points.len(), 1);
        assert_eq!(remote_points[0].cpu_pct, 97.0);
        assert_eq!(local_points[0].cpu_pct, 10.0);

        let remote_state = db::metrics::alert_states(&pool, id).await.unwrap();
        assert!(remote_state["host:cpu"].firing);
        assert!(!db::metrics::alert_states(&pool, LOCAL).await.unwrap()["host:cpu"].firing);
        let firing = db::metrics::firing_alerts(&pool).await.unwrap();
        assert!(firing
            .iter()
            .any(|a| a.server_id == id && a.server_name == name));

        // The remote label names the server.
        let events = evaluate_alerts(&pool, id, Some(&name), &snapshot(at, 10.0), &[], &[])
            .await
            .unwrap();
        assert_eq!(events.len(), 1);
        assert!(
            events[0].metric.label.ends_with(&format!("on {name}")),
            "{}",
            events[0].metric.label
        );

        db::servers::delete(&pool, id).await.unwrap();
        test_support::reset_metrics(&pool).await;
    }
}
