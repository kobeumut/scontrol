//! In-process host and container metrics sampler.
//!
//! Every `metrics_interval_secs` (10–30 s) the agent reads a handful of
//! kernel files and keeps the result in memory for the `metrics_snapshot`
//! RPC; panel-api collects and stores it. A sample spawns no process (no
//! `docker stats`, `top`, `free`, or `vmstat`):
//!
//! - host: `/proc/stat`, `/proc/meminfo`, `/proc/loadavg`, `/proc/diskstats`,
//!   `/proc/self/mounts` + `statvfs(2)` per disk mount;
//! - containers: cgroup v2 `cpu.stat`, `memory.current`, `memory.max`,
//!   `memory.stat`, and `io.stat` of each `docker-<id>.scope` (systemd
//!   driver) or `docker/<id>` (cgroupfs driver) directory.
//!
//! Compose labels (project slug and service) come from Docker's
//! `containers/<id>/config.v2.json`, read once per container and cached.
//! Containers that are not part of a Compose project are ignored.
//!
//! All reads go through `std::fs::read_to_string` or a bounded `File` read,
//! whose descriptors close when the call returns, so a sample cannot leak
//! file descriptors however often it runs or fails.

use crate::config::AgentConfig;
use chrono::Utc;
use panel_core::metrics::{self, ContainerMetrics, CpuTimes, DiskUsage, HostMetrics};
use panel_core::metrics::{MetricsSnapshot, MAX_SAMPLE_INTERVAL_SECS, MIN_SAMPLE_INTERVAL_SECS};
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

/// Largest `config.v2.json` read for labels; real ones are a few KiB.
const MAX_CONTAINER_CONFIG_BYTES: u64 = 1024 * 1024;

/// Where the sampler reads from. Real paths in production, synthetic
/// directories in tests.
#[derive(Debug, Clone)]
pub struct SamplerPaths {
    pub proc_root: PathBuf,
    pub sys_block: PathBuf,
    pub cgroup_root: PathBuf,
    pub docker_data_root: PathBuf,
    /// Containers are sampled only on Docker-enabled installs.
    pub containers: bool,
}

impl SamplerPaths {
    pub fn from_config(config: &AgentConfig) -> Self {
        Self {
            proc_root: config.metrics_proc_root.clone(),
            sys_block: config.metrics_sys_block_dir.clone(),
            cgroup_root: config.cgroup_root.clone(),
            docker_data_root: config.docker_data_root.clone(),
            containers: config.docker_enabled(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ComposeLabels {
    name: String,
    project: String,
    service: String,
}

#[derive(Debug, Clone, Copy)]
struct ContainerCounters {
    usage_usec: u64,
    read_bytes: u64,
    write_bytes: u64,
}

struct Reading {
    at: Instant,
    cpu: CpuTimes,
    disk_sectors: (u64, u64),
    containers: HashMap<String, ContainerCounters>,
}

#[derive(Default)]
struct SamplerState {
    previous: Option<Reading>,
    /// `None` caches "not a Compose container" so its config is read once.
    labels: HashMap<String, Option<ComposeLabels>>,
}

pub struct MetricsSampler {
    paths: SamplerPaths,
    state: Mutex<SamplerState>,
    latest: RwLock<Option<MetricsSnapshot>>,
}

impl MetricsSampler {
    pub fn new(paths: SamplerPaths) -> Self {
        Self {
            paths,
            state: Mutex::default(),
            latest: RwLock::default(),
        }
    }

    pub fn latest(&self) -> Option<MetricsSnapshot> {
        self.latest
            .read()
            .map(|guard| guard.clone())
            .unwrap_or(None)
    }

    /// Takes one reading. The first call only primes the counters; later
    /// calls publish a snapshot with rates over the time since the last.
    pub fn sample(&self) -> std::io::Result<Option<MetricsSnapshot>> {
        let started = Instant::now();
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let reading_and_host = self.read_host()?;
        let (reading, host_parts) = reading_and_host;
        let containers = if self.paths.containers {
            self.read_containers(&mut state)
        } else {
            Vec::new()
        };
        let reading = Reading {
            containers: containers
                .iter()
                .map(|(id, _, counters, _)| (id.clone(), *counters))
                .collect(),
            ..reading
        };

        let snapshot = state.previous.as_ref().map(|previous| {
            let elapsed = reading.at.duration_since(previous.at).as_secs_f64();
            let host = HostMetrics {
                cpu_pct: metrics::cpu_percent(&previous.cpu, &reading.cpu),
                disk_read_bps: metrics::rate_per_sec(
                    previous.disk_sectors.0,
                    reading.disk_sectors.0,
                    elapsed,
                ) * metrics::DISKSTATS_SECTOR_BYTES as f64,
                disk_write_bps: metrics::rate_per_sec(
                    previous.disk_sectors.1,
                    reading.disk_sectors.1,
                    elapsed,
                ) * metrics::DISKSTATS_SECTOR_BYTES as f64,
                ..host_parts.clone()
            };
            let containers = containers
                .iter()
                .filter_map(|(id, labels, now, memory)| {
                    let before = previous.containers.get(id)?;
                    Some(ContainerMetrics {
                        container_id: id.clone(),
                        name: labels.name.clone(),
                        project: labels.project.clone(),
                        service: labels.service.clone(),
                        cpu_pct: metrics::container_cpu_percent(
                            before.usage_usec,
                            now.usage_usec,
                            elapsed,
                        ),
                        mem_bytes: memory.0,
                        mem_limit_bytes: memory.1,
                        io_read_bps: metrics::rate_per_sec(
                            before.read_bytes,
                            now.read_bytes,
                            elapsed,
                        ),
                        io_write_bps: metrics::rate_per_sec(
                            before.write_bytes,
                            now.write_bytes,
                            elapsed,
                        ),
                    })
                })
                .collect();
            MetricsSnapshot {
                sampled_at: Utc::now(),
                interval_secs: elapsed,
                sample_micros: started.elapsed().as_micros() as u64,
                host,
                containers,
            }
        });
        state.previous = Some(reading);
        drop(state);
        if let Some(snapshot) = &snapshot {
            if let Ok(mut latest) = self.latest.write() {
                *latest = Some(snapshot.clone());
            }
        }
        Ok(snapshot)
    }

    fn read_host(&self) -> std::io::Result<(Reading, HostMetrics)> {
        let proc_root = &self.paths.proc_root;
        let stat = std::fs::read_to_string(proc_root.join("stat"))?;
        let (cpu, cpu_count) = metrics::parse_proc_stat(&stat).ok_or_else(|| invalid("stat"))?;
        let meminfo = std::fs::read_to_string(proc_root.join("meminfo"))?;
        let mem = metrics::parse_meminfo(&meminfo).ok_or_else(|| invalid("meminfo"))?;
        let load = std::fs::read_to_string(proc_root.join("loadavg"))
            .ok()
            .and_then(|content| metrics::parse_loadavg(&content))
            .unwrap_or_default();
        let disk_sectors = std::fs::read_to_string(proc_root.join("diskstats"))
            .map(|content| {
                metrics::parse_diskstats(&content, |name| {
                    !metrics::is_virtual_block_device(name)
                        && is_plain_name(name)
                        && self.paths.sys_block.join(name).exists()
                })
            })
            .unwrap_or_default();
        let disks = std::fs::read_to_string(proc_root.join("self/mounts"))
            .map(|content| disk_usage(&content))
            .unwrap_or_default();
        let host = HostMetrics {
            cpu_pct: 0.0,
            cpu_count,
            mem_total_bytes: mem.total_bytes,
            mem_used_bytes: mem.used_bytes(),
            swap_total_bytes: mem.swap_total_bytes,
            swap_used_bytes: mem.swap_total_bytes.saturating_sub(mem.swap_free_bytes),
            load,
            disk_read_bps: 0.0,
            disk_write_bps: 0.0,
            disks,
        };
        Ok((
            Reading {
                at: Instant::now(),
                cpu,
                disk_sectors,
                containers: HashMap::new(),
            },
            host,
        ))
    }

    /// Reads every Compose container's cgroup. Returns id, labels,
    /// counters, and (working set, limit).
    #[allow(clippy::type_complexity)]
    fn read_containers(
        &self,
        state: &mut SamplerState,
    ) -> Vec<(String, ComposeLabels, ContainerCounters, (u64, Option<u64>))> {
        let mut found = Vec::new();
        let mut seen = Vec::new();
        for (id, dir) in container_cgroups(&self.paths.cgroup_root) {
            seen.push(id.clone());
            let labels = state
                .labels
                .entry(id.clone())
                .or_insert_with(|| read_compose_labels(&self.paths.docker_data_root, &id))
                .clone();
            let Some(labels) = labels else { continue };
            let Some((counters, memory)) = read_container_cgroup(&dir) else {
                continue;
            };
            found.push((id, labels, counters, memory));
        }
        // Forget containers that are gone, so the cache stays bounded.
        state.labels.retain(|id, _| seen.contains(id));
        found
    }
}

/// Runs the sampler forever on the blocking pool, once per interval.
pub fn spawn(sampler: Arc<MetricsSampler>, interval_secs: u64) {
    let interval = Duration::from_secs(
        interval_secs.clamp(MIN_SAMPLE_INTERVAL_SECS, MAX_SAMPLE_INTERVAL_SECS),
    );
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut failing = false;
        loop {
            ticker.tick().await;
            let sampler = sampler.clone();
            match tokio::task::spawn_blocking(move || sampler.sample()).await {
                Ok(Ok(_)) => failing = false,
                Ok(Err(error)) if !failing => {
                    failing = true;
                    tracing::warn!(%error, "metrics sample failed");
                }
                Ok(Err(_)) => {}
                Err(error) => tracing::warn!(%error, "metrics sampler task failed"),
            }
        }
    });
}

fn invalid(file: &str) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        format!("unexpected /proc/{file} format"),
    )
}

/// A device name used as a single path component under `/sys/block`.
fn is_plain_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        && name != "."
        && name != ".."
}

fn disk_usage(mounts: &str) -> Vec<DiskUsage> {
    metrics::parse_mounts(mounts)
        .into_iter()
        .filter_map(|mount| {
            let stat = rustix::fs::statvfs(mount.mount_point.as_str()).ok()?;
            let fragment = stat.f_frsize.max(1);
            let used = stat
                .f_blocks
                .saturating_sub(stat.f_bfree)
                .saturating_mul(fragment);
            let available = stat.f_bavail.saturating_mul(fragment);
            // Like `df`: blocks reserved for root count as neither, so the
            // percentage is what an unprivileged writer sees.
            let total = used.saturating_add(available);
            (total > 0).then_some(DiskUsage {
                mount_point: mount.mount_point,
                fs_type: mount.fs_type,
                total_bytes: total,
                used_bytes: used,
            })
        })
        .collect()
}

/// Container cgroup directories under both cgroup drivers.
fn container_cgroups(cgroup_root: &Path) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    for parent in [cgroup_root.join("system.slice"), cgroup_root.join("docker")] {
        let Ok(entries) = std::fs::read_dir(&parent) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if let Some(id) = metrics::container_id_from_cgroup_dir(name) {
                out.push((id.to_string(), entry.path()));
            }
        }
    }
    out
}

fn read_container_cgroup(dir: &Path) -> Option<(ContainerCounters, (u64, Option<u64>))> {
    let read = |file: &str| std::fs::read_to_string(dir.join(file)).ok();
    let usage_usec = metrics::parse_cgroup_cpu_usage_usec(&read("cpu.stat")?)?;
    let current = metrics::parse_cgroup_u64(&read("memory.current")?)?;
    let limit = read("memory.max").and_then(|c| metrics::parse_cgroup_memory_max(&c));
    let inactive = read("memory.stat")
        .and_then(|c| metrics::parse_cgroup_inactive_file(&c))
        .unwrap_or(0);
    let (read_bytes, write_bytes) = read("io.stat")
        .map(|c| metrics::parse_cgroup_io_stat(&c))
        .unwrap_or_default();
    Some((
        ContainerCounters {
            usage_usec,
            read_bytes,
            write_bytes,
        },
        (current.saturating_sub(inactive), limit),
    ))
}

/// Compose labels from Docker's container config. `None` for containers
/// outside Compose or with labels the panel would never generate.
fn read_compose_labels(docker_data_root: &Path, id: &str) -> Option<ComposeLabels> {
    if !metrics::is_container_id(id) {
        return None;
    }
    let path = docker_data_root
        .join("containers")
        .join(id)
        .join("config.v2.json");
    let mut content = String::new();
    std::fs::File::open(path)
        .ok()?
        .take(MAX_CONTAINER_CONFIG_BYTES)
        .read_to_string(&mut content)
        .ok()?;
    let value: serde_json::Value = serde_json::from_str(&content).ok()?;
    let labels = value.get("Config")?.get("Labels")?;
    let project = labels.get("com.docker.compose.project")?.as_str()?;
    let service = labels.get("com.docker.compose.service")?.as_str()?;
    if !is_label_value(project) || !is_label_value(service) {
        return None;
    }
    let name = value
        .get("Name")
        .and_then(|n| n.as_str())
        .map(|n| n.trim_start_matches('/'))
        .filter(|n| is_label_value(n))
        .unwrap_or(&id[..12]);
    Some(ComposeLabels {
        name: name.to_string(),
        project: project.to_string(),
        service: service.to_string(),
    })
}

fn is_label_value(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        root: PathBuf,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    impl Fixture {
        fn new() -> Self {
            let root =
                std::env::temp_dir().join(format!("scontrol-metrics-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(root.join("proc/self")).unwrap();
            std::fs::create_dir_all(root.join("sys/block/sda")).unwrap();
            std::fs::create_dir_all(root.join("disk")).unwrap();
            let fixture = Self { root };
            fixture.write(
                "proc/meminfo",
                "MemTotal: 1000000 kB\nMemAvailable: 250000 kB\nSwapTotal: 0 kB\nSwapFree: 0 kB\n",
            );
            fixture.write("proc/loadavg", "0.50 0.25 0.10 1/100 42\n");
            let disk = fixture.root.join("disk");
            fixture.write(
                "proc/self/mounts",
                &format!(
                    "proc /proc proc rw 0 0\n/dev/sda1 {} ext4 rw 0 0\n",
                    disk.display()
                ),
            );
            fixture.set_host(1000, 9000, 0, 0);
            fixture
        }

        fn write(&self, relative: &str, content: &str) {
            let path = self.root.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
        }

        fn set_host(&self, busy: u64, idle: u64, read_sectors: u64, write_sectors: u64) {
            self.write(
                "proc/stat",
                &format!("cpu  {busy} 0 0 {idle} 0 0 0 0 0 0\ncpu0 1 0 0 1\ncpu1 1 0 0 1\n"),
            );
            self.write(
                "proc/diskstats",
                &format!(
                    "   8 0 sda 1 0 {read_sectors} 0 1 0 {write_sectors} 0 0 0 0\n\
                        8 1 sda1 1 0 99999 0 1 0 99999 0 0 0 0\n\
                        7 0 loop0 1 0 77777 0 1 0 77777 0 0 0 0\n"
                ),
            );
        }

        fn add_container(&self, id: &str, project: Option<&str>, usage_usec: u64, io: u64) {
            let cgroup = format!("cgroup/system.slice/docker-{id}.scope");
            self.write(
                &format!("{cgroup}/cpu.stat"),
                &format!("usage_usec {usage_usec}\nuser_usec 1\n"),
            );
            self.write(&format!("{cgroup}/memory.current"), "300000000\n");
            self.write(&format!("{cgroup}/memory.max"), "1000000000\n");
            self.write(
                &format!("{cgroup}/memory.stat"),
                "anon 1\ninactive_file 100000000\n",
            );
            self.write(
                &format!("{cgroup}/io.stat"),
                &format!("8:0 rbytes={io} wbytes={} rios=1 wios=1\n", io * 2),
            );
            let labels = match project {
                Some(project) => format!(
                    r#"{{"com.docker.compose.project":"{project}","com.docker.compose.service":"web"}}"#
                ),
                None => "{}".into(),
            };
            self.write(
                &format!("docker/containers/{id}/config.v2.json"),
                &format!(
                    r#"{{"Name":"/{}-web-1","Config":{{"Labels":{labels}}}}}"#,
                    project.unwrap_or("x")
                ),
            );
        }

        fn sampler(&self, containers: bool) -> MetricsSampler {
            MetricsSampler::new(SamplerPaths {
                proc_root: self.root.join("proc"),
                sys_block: self.root.join("sys/block"),
                cgroup_root: self.root.join("cgroup"),
                docker_data_root: self.root.join("docker"),
                containers,
            })
        }
    }

    #[test]
    fn first_sample_primes_and_second_reports_rates() {
        let fixture = Fixture::new();
        let compose_id = "a".repeat(64);
        let plain_id = "b".repeat(64);
        fixture.add_container(&compose_id, Some("my-app"), 1_000_000, 1000);
        fixture.add_container(&plain_id, None, 0, 0);
        let sampler = fixture.sampler(true);

        assert!(sampler.sample().unwrap().is_none());
        assert!(sampler.latest().is_none());

        // 1000 more jiffies, 250 busy → 25 %. sda sectors +2048/+4096.
        fixture.set_host(1250, 9750, 2048, 4096);
        fixture.add_container(
            &compose_id,
            Some("my-app"),
            1_000_000 + 500_000,
            1000 + 4096,
        );
        std::thread::sleep(Duration::from_millis(50));
        let snapshot = sampler.sample().unwrap().expect("second sample");
        assert_eq!(sampler.latest().as_ref(), Some(&snapshot));

        let host = &snapshot.host;
        assert!((host.cpu_pct - 25.0).abs() < 1e-9);
        assert_eq!(host.cpu_count, 2);
        assert_eq!(host.mem_total_bytes, 1_000_000 * 1024);
        assert_eq!(host.mem_used_bytes, 750_000 * 1024);
        assert_eq!(host.load.one, 0.5);
        // Partition and loop device are not counted on top of sda.
        let elapsed = snapshot.interval_secs;
        assert!((host.disk_read_bps - 2048.0 * 512.0 / elapsed).abs() < 1e-6);
        assert!((host.disk_write_bps - 4096.0 * 512.0 / elapsed).abs() < 1e-6);
        assert_eq!(host.disks.len(), 1);
        assert!(host.disks[0].total_bytes > 0);

        // Only the Compose container is reported.
        assert_eq!(snapshot.containers.len(), 1);
        let container = &snapshot.containers[0];
        assert_eq!(container.project, "my-app");
        assert_eq!(container.service, "web");
        assert_eq!(container.name, "my-app-web-1");
        assert_eq!(container.mem_bytes, 200_000_000);
        assert_eq!(container.mem_limit_bytes, Some(1_000_000_000));
        assert!((container.cpu_pct - 0.5 / elapsed * 100.0).abs() < 1e-6);
        assert!((container.io_read_bps - 4096.0 / elapsed).abs() < 1e-6);
        assert!((container.io_write_bps - 8192.0 / elapsed).abs() < 1e-6);
    }

    #[test]
    fn containers_off_without_docker_and_vanished_containers_are_forgotten() {
        let fixture = Fixture::new();
        let id = "c".repeat(64);
        fixture.add_container(&id, Some("svc"), 10, 0);
        let without_docker = fixture.sampler(false);
        without_docker.sample().unwrap();
        assert!(without_docker
            .sample()
            .unwrap()
            .unwrap()
            .containers
            .is_empty());

        let sampler = fixture.sampler(true);
        sampler.sample().unwrap();
        assert_eq!(sampler.state.lock().unwrap().labels.len(), 1);
        std::fs::remove_dir_all(
            fixture
                .root
                .join(format!("cgroup/system.slice/docker-{id}.scope")),
        )
        .unwrap();
        let snapshot = sampler.sample().unwrap().unwrap();
        assert!(snapshot.containers.is_empty());
        assert!(sampler.state.lock().unwrap().labels.is_empty());
    }

    #[test]
    fn missing_or_garbled_proc_files_fail_cleanly() {
        let fixture = Fixture::new();
        fixture.write("proc/stat", "garbage\n");
        assert!(fixture.sampler(false).sample().is_err());
        std::fs::remove_file(fixture.root.join("proc/stat")).unwrap();
        assert!(fixture.sampler(false).sample().is_err());
    }

    #[test]
    fn hostile_labels_are_ignored() {
        let fixture = Fixture::new();
        let id = "d".repeat(64);
        fixture.add_container(&id, Some("ok"), 0, 0);
        fixture.write(
            &format!("docker/containers/{id}/config.v2.json"),
            r#"{"Config":{"Labels":{"com.docker.compose.project":"../etc","com.docker.compose.service":"web"}}}"#,
        );
        assert_eq!(read_compose_labels(&fixture.root.join("docker"), &id), None);
        assert_eq!(
            read_compose_labels(&fixture.root.join("docker"), "../x"),
            None
        );
    }

    /// Thousands of samples must not leave descriptors open.
    #[test]
    fn sampling_does_not_leak_file_descriptors() {
        let fixture = Fixture::new();
        fixture.add_container(&"e".repeat(64), Some("app"), 0, 0);
        let sampler = fixture.sampler(true);
        let open_fds = || std::fs::read_dir("/proc/self/fd").map(|d| d.count()).ok();
        sampler.sample().unwrap();
        let before = open_fds();
        for _ in 0..2000 {
            sampler.sample().unwrap();
        }
        // Other tests run in parallel threads, so allow a little noise.
        if let (Some(before), Some(after)) = (before, open_fds()) {
            assert!(after <= before + 8, "fds grew from {before} to {after}");
        }
    }

    #[test]
    fn real_host_sample_fits_the_budget() {
        // Only meaningful on Linux with /proc; skipped elsewhere.
        if !Path::new("/proc/stat").exists() {
            return;
        }
        let sampler = MetricsSampler::new(SamplerPaths {
            proc_root: "/proc".into(),
            sys_block: "/sys/block".into(),
            cgroup_root: "/sys/fs/cgroup".into(),
            docker_data_root: "/nonexistent".into(),
            containers: false,
        });
        sampler.sample().unwrap();
        let snapshot = sampler.sample().unwrap().unwrap();
        assert!(snapshot.host.mem_total_bytes > 0);
        assert!((0.0..=100.0).contains(&snapshot.host.cpu_pct));
        // Generous for debug builds and loaded CI; the release budget is 1 ms.
        assert!(
            snapshot.sample_micros < 50_000,
            "{} µs",
            snapshot.sample_micros
        );
    }
}
