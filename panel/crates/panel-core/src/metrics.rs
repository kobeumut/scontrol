//! Host and container metrics: kernel-file parsers, rate math, the
//! snapshot the agent reports, and the threshold-alert state machine.
//!
//! Everything here is pure, so it can be tested against synthetic `/proc`
//! and cgroup v2 files. The agent (`panel-agent` `adapters/metrics.rs`)
//! reads the files with plain `std::fs` calls; no process is ever spawned
//! to sample (`top`, `free`, `vmstat`, and `docker stats` are not used).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Bytes per sector in `/proc/diskstats`; the kernel always counts 512.
pub const DISKSTATS_SECTOR_BYTES: u64 = 512;

/// Shortest and longest agent sampling interval.
pub const MIN_SAMPLE_INTERVAL_SECS: u64 = 10;
pub const MAX_SAMPLE_INTERVAL_SECS: u64 = 30;
pub const DEFAULT_SAMPLE_INTERVAL_SECS: u64 = 15;

/// Storage resolutions and how long each is kept.
pub const RAW_RESOLUTION_SECS: i32 = 30;
pub const MEDIUM_RESOLUTION_SECS: i32 = 300;
pub const COARSE_RESOLUTION_SECS: i32 = 3600;

// ---------------------------------------------------------------------------
// /proc parsers
// ---------------------------------------------------------------------------

/// Aggregate CPU jiffies from the first (`cpu `) line of `/proc/stat`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CpuTimes {
    pub user: u64,
    pub nice: u64,
    pub system: u64,
    pub idle: u64,
    pub iowait: u64,
    pub irq: u64,
    pub softirq: u64,
    pub steal: u64,
}

impl CpuTimes {
    /// `guest` and `guest_nice` are already included in `user`/`nice`.
    pub fn total(&self) -> u64 {
        self.user
            .saturating_add(self.nice)
            .saturating_add(self.system)
            .saturating_add(self.idle)
            .saturating_add(self.iowait)
            .saturating_add(self.irq)
            .saturating_add(self.softirq)
            .saturating_add(self.steal)
    }

    pub fn idle_total(&self) -> u64 {
        self.idle.saturating_add(self.iowait)
    }
}

/// Parses `/proc/stat`. Returns the aggregate CPU times and the number of
/// per-CPU lines (`cpu0`, `cpu1`, ...).
pub fn parse_proc_stat(content: &str) -> Option<(CpuTimes, u32)> {
    let mut aggregate = None;
    let mut cpus = 0u32;
    for line in content.lines() {
        let mut fields = line.split_ascii_whitespace();
        let Some(label) = fields.next() else { continue };
        if label == "cpu" {
            let values: Vec<u64> = fields.map_while(|f| f.parse().ok()).collect();
            if values.len() < 4 {
                return None;
            }
            let at = |i: usize| values.get(i).copied().unwrap_or(0);
            aggregate = Some(CpuTimes {
                user: at(0),
                nice: at(1),
                system: at(2),
                idle: at(3),
                iowait: at(4),
                irq: at(5),
                softirq: at(6),
                steal: at(7),
            });
        } else if label.starts_with("cpu") && label[3..].bytes().all(|b| b.is_ascii_digit()) {
            cpus += 1;
        }
    }
    aggregate.map(|times| (times, cpus.max(1)))
}

/// Busy share of all CPUs between two `/proc/stat` readings, 0–100.
pub fn cpu_percent(previous: &CpuTimes, current: &CpuTimes) -> f64 {
    let total = current.total().saturating_sub(previous.total());
    if total == 0 {
        return 0.0;
    }
    let idle = current.idle_total().saturating_sub(previous.idle_total());
    let busy = total.saturating_sub(idle);
    clamp_pct(busy as f64 * 100.0 / total as f64)
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MemInfo {
    pub total_bytes: u64,
    pub available_bytes: u64,
    pub swap_total_bytes: u64,
    pub swap_free_bytes: u64,
}

impl MemInfo {
    pub fn used_bytes(&self) -> u64 {
        self.total_bytes.saturating_sub(self.available_bytes)
    }
}

/// Parses `/proc/meminfo` (values in kB). Kernels before 3.14 have no
/// `MemAvailable`; free + buffers + cached is used there.
pub fn parse_meminfo(content: &str) -> Option<MemInfo> {
    let mut total = None;
    let mut available = None;
    let (mut free, mut buffers, mut cached) = (0u64, 0u64, 0u64);
    let (mut swap_total, mut swap_free) = (0u64, 0u64);
    for line in content.lines() {
        let Some((key, rest)) = line.split_once(':') else {
            continue;
        };
        let Some(kb) = rest
            .split_ascii_whitespace()
            .next()
            .and_then(|v| v.parse::<u64>().ok())
        else {
            continue;
        };
        let bytes = kb.saturating_mul(1024);
        match key {
            "MemTotal" => total = Some(bytes),
            "MemAvailable" => available = Some(bytes),
            "MemFree" => free = bytes,
            "Buffers" => buffers = bytes,
            "Cached" => cached = bytes,
            "SwapTotal" => swap_total = bytes,
            "SwapFree" => swap_free = bytes,
            _ => {}
        }
    }
    let total_bytes = total?;
    let available_bytes = available
        .unwrap_or_else(|| free.saturating_add(buffers).saturating_add(cached))
        .min(total_bytes);
    Some(MemInfo {
        total_bytes,
        available_bytes,
        swap_total_bytes: swap_total,
        swap_free_bytes: swap_free.min(swap_total),
    })
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct LoadAvg {
    pub one: f64,
    pub five: f64,
    pub fifteen: f64,
}

pub fn parse_loadavg(content: &str) -> Option<LoadAvg> {
    let mut fields = content.split_ascii_whitespace();
    let mut next = || fields.next()?.parse::<f64>().ok().filter(|v| v.is_finite());
    Some(LoadAvg {
        one: next()?,
        five: next()?,
        fifteen: next()?,
    })
}

/// Sectors read and written, summed over the devices `include` accepts.
/// The caller decides what a whole disk is (the agent checks `/sys/block`
/// and skips loop, RAM, device-mapper, and md devices), so partitions and
/// stacked devices are not counted twice.
pub fn parse_diskstats(content: &str, include: impl Fn(&str) -> bool) -> (u64, u64) {
    let mut read = 0u64;
    let mut written = 0u64;
    for line in content.lines() {
        let fields: Vec<&str> = line.split_ascii_whitespace().collect();
        if fields.len() < 10 || !include(fields[2]) {
            continue;
        }
        let (Ok(r), Ok(w)) = (fields[5].parse::<u64>(), fields[9].parse::<u64>()) else {
            continue;
        };
        read = read.saturating_add(r);
        written = written.saturating_add(w);
    }
    (read, written)
}

/// Block devices whose I/O is already counted on another device, or that
/// are not disks at all.
pub fn is_virtual_block_device(name: &str) -> bool {
    ["loop", "ram", "zram", "dm-", "md", "sr", "fd", "nbd"]
        .iter()
        .any(|prefix| name.starts_with(prefix))
}

/// One mount from `/proc/self/mounts`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountEntry {
    pub device: String,
    pub mount_point: String,
    pub fs_type: String,
}

/// File systems that hold data worth watching. Pseudo file systems,
/// overlay layers, squashfs snaps, and the EFI partition are left out.
const DISK_FS_TYPES: &[&str] = &[
    "ext2", "ext3", "ext4", "xfs", "btrfs", "zfs", "f2fs", "jfs", "reiserfs", "bcachefs",
];

/// Mount points under these prefixes are container internals, not disks.
const SKIPPED_MOUNT_PREFIXES: &[&str] = &[
    "/proc",
    "/sys",
    "/dev",
    "/run",
    "/var/lib/docker/",
    "/var/lib/containerd/",
    "/snap/",
];

/// Parses `/proc/self/mounts`, keeping one entry per device for the disk
/// file systems in [`DISK_FS_TYPES`] (the first, shortest mount point wins
/// for bind mounts). Octal escapes (`\040` for a space) are decoded.
pub fn parse_mounts(content: &str) -> Vec<MountEntry> {
    let mut entries: Vec<MountEntry> = Vec::new();
    for line in content.lines() {
        let mut fields = line.split_ascii_whitespace();
        let (Some(device), Some(mount_point), Some(fs_type)) =
            (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        if !DISK_FS_TYPES.contains(&fs_type) {
            continue;
        }
        let mount_point = unescape_mount(mount_point);
        if !mount_point.starts_with('/')
            || SKIPPED_MOUNT_PREFIXES
                .iter()
                .any(|p| mount_point == p.trim_end_matches('/') || mount_point.starts_with(p))
        {
            continue;
        }
        match entries.iter_mut().find(|e| e.device == device) {
            Some(existing) if mount_point.len() < existing.mount_point.len() => {
                existing.mount_point = mount_point;
            }
            Some(_) => {}
            None => entries.push(MountEntry {
                device: device.to_string(),
                mount_point,
                fs_type: fs_type.to_string(),
            }),
        }
    }
    entries
}

fn unescape_mount(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 3 < bytes.len() {
            let digits = &bytes[i + 1..i + 4];
            if digits.iter().all(|b| (b'0'..=b'7').contains(b)) {
                let value = digits
                    .iter()
                    .fold(0u32, |acc, d| acc * 8 + u32::from(d - b'0'));
                if let Ok(value) = u8::try_from(value) {
                    out.push(value);
                    i += 4;
                    continue;
                }
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ---------------------------------------------------------------------------
// cgroup v2 parsers
// ---------------------------------------------------------------------------

/// `usage_usec` from a cgroup v2 `cpu.stat`.
pub fn parse_cgroup_cpu_usage_usec(content: &str) -> Option<u64> {
    flat_keyed(content, "usage_usec")
}

/// A single-number cgroup file such as `memory.current`.
pub fn parse_cgroup_u64(content: &str) -> Option<u64> {
    content.trim().parse().ok()
}

/// `memory.max`: `None` means unlimited (`max`).
pub fn parse_cgroup_memory_max(content: &str) -> Option<u64> {
    match content.trim() {
        "max" => None,
        value => value.parse().ok(),
    }
}

/// `inactive_file` from `memory.stat`: page cache the kernel can drop.
/// Subtracting it from `memory.current` gives the working set, the number
/// `docker stats` reports.
pub fn parse_cgroup_inactive_file(content: &str) -> Option<u64> {
    flat_keyed(content, "inactive_file")
}

/// Bytes read and written, summed over every device in `io.stat`
/// (`8:0 rbytes=1 wbytes=2 rios=3 wios=4 dbytes=0 dios=0`).
pub fn parse_cgroup_io_stat(content: &str) -> (u64, u64) {
    let mut read = 0u64;
    let mut written = 0u64;
    for line in content.lines() {
        for field in line.split_ascii_whitespace().skip(1) {
            let Some((key, value)) = field.split_once('=') else {
                continue;
            };
            let Ok(value) = value.parse::<u64>() else {
                continue;
            };
            match key {
                "rbytes" => read = read.saturating_add(value),
                "wbytes" => written = written.saturating_add(value),
                _ => {}
            }
        }
    }
    (read, written)
}

fn flat_keyed(content: &str, wanted: &str) -> Option<u64> {
    content.lines().find_map(|line| {
        let (key, value) = line.split_once(' ')?;
        (key == wanted).then(|| value.trim().parse().ok()).flatten()
    })
}

/// A Docker container id (64 lowercase hex characters).
pub fn is_container_id(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// The container id of a cgroup directory name, for both cgroup drivers:
/// `docker-<id>.scope` (systemd) and `<id>` under `docker/` (cgroupfs).
pub fn container_id_from_cgroup_dir(name: &str) -> Option<&str> {
    let id = name
        .strip_prefix("docker-")
        .and_then(|rest| rest.strip_suffix(".scope"))
        .unwrap_or(name);
    is_container_id(id).then_some(id)
}

// ---------------------------------------------------------------------------
// Rates
// ---------------------------------------------------------------------------

/// Per-second rate of a monotonically increasing counter. A counter that
/// went backwards (reset, container restart) yields 0, not a huge spike.
pub fn rate_per_sec(previous: u64, current: u64, elapsed_secs: f64) -> f64 {
    if elapsed_secs <= 0.0 || current < previous {
        return 0.0;
    }
    (current - previous) as f64 / elapsed_secs
}

/// Container CPU use between two `usage_usec` readings, as `docker stats`
/// shows it: 100 means one full core, so it can exceed 100.
pub fn container_cpu_percent(previous_usec: u64, current_usec: u64, elapsed_secs: f64) -> f64 {
    let pct = rate_per_sec(previous_usec, current_usec, elapsed_secs) / 10_000.0;
    if pct.is_finite() {
        pct.max(0.0)
    } else {
        0.0
    }
}

pub fn percent_of(used: u64, total: u64) -> f64 {
    if total == 0 {
        return 0.0;
    }
    clamp_pct(used as f64 * 100.0 / total as f64)
}

fn clamp_pct(value: f64) -> f64 {
    if value.is_finite() {
        value.clamp(0.0, 100.0)
    } else {
        0.0
    }
}

// ---------------------------------------------------------------------------
// Snapshot reported by the agent
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DiskUsage {
    pub mount_point: String,
    pub fs_type: String,
    pub total_bytes: u64,
    pub used_bytes: u64,
}

impl DiskUsage {
    pub fn used_pct(&self) -> f64 {
        percent_of(self.used_bytes, self.total_bytes)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct HostMetrics {
    /// Busy share of all CPUs, 0–100.
    pub cpu_pct: f64,
    pub cpu_count: u32,
    pub mem_total_bytes: u64,
    pub mem_used_bytes: u64,
    pub swap_total_bytes: u64,
    pub swap_used_bytes: u64,
    pub load: LoadAvg,
    pub disk_read_bps: f64,
    pub disk_write_bps: f64,
    pub disks: Vec<DiskUsage>,
}

impl HostMetrics {
    pub fn mem_used_pct(&self) -> f64 {
        percent_of(self.mem_used_bytes, self.mem_total_bytes)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ContainerMetrics {
    pub container_id: String,
    pub name: String,
    /// `com.docker.compose.project` label: the panel's project slug.
    pub project: String,
    /// `com.docker.compose.service` label.
    pub service: String,
    /// 100 = one core (`docker stats` convention).
    pub cpu_pct: f64,
    /// Working set: `memory.current` minus inactive page cache.
    pub mem_bytes: u64,
    /// `memory.max`; `None` when the container has no memory limit.
    pub mem_limit_bytes: Option<u64>,
    pub io_read_bps: f64,
    pub io_write_bps: f64,
}

/// What the agent's sampler last measured.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct MetricsSnapshot {
    pub sampled_at: DateTime<Utc>,
    /// Seconds between this sample and the previous one (the rate window).
    pub interval_secs: f64,
    /// How long reading every file took, for the ≤ 1 ms budget.
    pub sample_micros: u64,
    pub host: HostMetrics,
    pub containers: Vec<ContainerMetrics>,
}

// ---------------------------------------------------------------------------
// Threshold alerts
// ---------------------------------------------------------------------------

/// When a metric counts as breached and when it counts as recovered.
///
/// A metric fires after `trigger_samples` consecutive samples at or above
/// `threshold`, and resolves after `clear_samples` consecutive samples
/// below `threshold - hysteresis`. Values inside the hysteresis band keep
/// the current state and reset the streak, so a metric hovering around the
/// threshold sends one alert, not one per sample.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ThresholdRule {
    pub threshold: f64,
    pub hysteresis: f64,
    pub trigger_samples: u32,
    pub clear_samples: u32,
}

impl ThresholdRule {
    pub fn validate(&self) -> Result<(), String> {
        if !(1.0..=100.0).contains(&self.threshold) {
            return Err("threshold must be between 1 and 100".into());
        }
        if !(0.0..=50.0).contains(&self.hysteresis) || self.hysteresis >= self.threshold {
            return Err("hysteresis must be between 0 and 50 and below the threshold".into());
        }
        if !(1..=60).contains(&self.trigger_samples) || !(1..=60).contains(&self.clear_samples) {
            return Err("sample counts must be between 1 and 60".into());
        }
        Ok(())
    }
}

/// Persistent state of one watched metric.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AlertTracker {
    pub firing: bool,
    /// Consecutive samples pointing towards the other state.
    pub streak: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlertTransition {
    None,
    Fired,
    Resolved,
}

impl AlertTracker {
    /// Feeds one sample and reports an edge, if this sample caused one.
    pub fn observe(&mut self, value: f64, rule: &ThresholdRule) -> AlertTransition {
        if !value.is_finite() {
            return AlertTransition::None;
        }
        let towards_other_state = if self.firing {
            value < rule.threshold - rule.hysteresis
        } else {
            value >= rule.threshold
        };
        if !towards_other_state {
            self.streak = 0;
            return AlertTransition::None;
        }
        self.streak = self.streak.saturating_add(1);
        let needed = if self.firing {
            rule.clear_samples
        } else {
            rule.trigger_samples
        };
        if self.streak < needed.max(1) {
            return AlertTransition::None;
        }
        self.streak = 0;
        self.firing = !self.firing;
        if self.firing {
            AlertTransition::Fired
        } else {
            AlertTransition::Resolved
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROC_STAT: &str = "cpu  4705 356 584 3699 23 23 0 0 0 0\n\
cpu0 1393280 32966 572056 13343292 6130 0 17875 0 23933 0\n\
cpu1 1335 2 3 4 5 6 7 0 0 0\n\
intr 1462898\nctxt 115315133\nbtime 1700000000\nprocesses 1\n";

    #[test]
    fn proc_stat_aggregate_and_cpu_count() {
        let (times, cpus) = parse_proc_stat(PROC_STAT).unwrap();
        assert_eq!(cpus, 2);
        assert_eq!(times.user, 4705);
        assert_eq!(times.idle, 3699);
        assert_eq!(times.iowait, 23);
        assert_eq!(times.total(), 4705 + 356 + 584 + 3699 + 23 + 23);
        assert!(parse_proc_stat("intr 1\n").is_none());
        assert!(parse_proc_stat("cpu 1 2\n").is_none());
    }

    #[test]
    fn cpu_percent_from_deltas() {
        let a = CpuTimes {
            user: 100,
            idle: 900,
            ..Default::default()
        };
        let b = CpuTimes {
            user: 150,
            system: 25,
            idle: 1000,
            iowait: 25,
            ..Default::default()
        };
        // 200 jiffies passed, 125 idle (idle + iowait) → 37.5 % busy.
        assert!((cpu_percent(&a, &b) - 37.5).abs() < 1e-9);
        assert_eq!(cpu_percent(&a, &a), 0.0);
        // A counter reset never yields a negative or >100 value.
        assert_eq!(cpu_percent(&b, &a), 0.0);
    }

    #[test]
    fn meminfo_uses_mem_available_or_falls_back() {
        let modern =
            "MemTotal:       16000000 kB\nMemFree:  1000000 kB\nMemAvailable:   12000000 kB\n\
Buffers: 100 kB\nCached: 200 kB\nSwapTotal: 2048 kB\nSwapFree: 1024 kB\n";
        let info = parse_meminfo(modern).unwrap();
        assert_eq!(info.total_bytes, 16_000_000 * 1024);
        assert_eq!(info.used_bytes(), 4_000_000 * 1024);
        assert_eq!(info.swap_total_bytes - info.swap_free_bytes, 1024 * 1024);

        let old = "MemTotal: 1000 kB\nMemFree: 100 kB\nBuffers: 50 kB\nCached: 250 kB\n";
        let info = parse_meminfo(old).unwrap();
        assert_eq!(info.available_bytes, 400 * 1024);
        assert!(parse_meminfo("MemFree: 1 kB\n").is_none());
    }

    #[test]
    fn loadavg() {
        let load = parse_loadavg("0.52 1.05 2.50 3/412 12345\n").unwrap();
        assert_eq!(
            load,
            LoadAvg {
                one: 0.52,
                five: 1.05,
                fifteen: 2.5
            }
        );
        assert!(parse_loadavg("nan").is_none());
        assert!(parse_loadavg("").is_none());
    }

    #[test]
    fn diskstats_counts_whole_disks_only() {
        let content = "   8       0 sda 100 0 2000 0 50 0 4000 0 0 0 0\n\
   8       1 sda1 90 0 1900 0 40 0 3900 0 0 0 0\n\
   7       0 loop0 5 0 80 0 0 0 0 0 0 0 0\n\
 259       0 nvme0n1 10 0 300 0 20 0 600 0 0 0 0\n\
 253       0 dm-0 1 0 999 0 1 0 999 0 0 0 0\n\
 bogus\n";
        let whole = ["sda", "nvme0n1", "loop0", "dm-0"];
        let (read, written) = parse_diskstats(content, |name| {
            whole.contains(&name) && !is_virtual_block_device(name)
        });
        assert_eq!((read, written), (2300, 4600));
    }

    #[test]
    fn mounts_keep_real_disks_once() {
        let content = "sysfs /sys sysfs rw 0 0\n\
proc /proc proc rw 0 0\n\
/dev/sda1 / ext4 rw,relatime 0 0\n\
/dev/sda2 /srv/my\\040data xfs rw 0 0\n\
/dev/sda1 /var/lib/docker ext4 rw 0 0\n\
overlay /var/lib/docker/overlay2/abc/merged overlay rw 0 0\n\
/dev/sda3 /var/lib/docker/volumes ext4 rw 0 0\n\
/dev/loop0 /snap/core/1 squashfs ro 0 0\n\
/dev/sda15 /boot/efi vfat rw 0 0\n\
tmpfs /run tmpfs rw 0 0\n";
        let mounts = parse_mounts(content);
        let points: Vec<&str> = mounts.iter().map(|m| m.mount_point.as_str()).collect();
        assert_eq!(points, vec!["/", "/srv/my data"]);
        assert_eq!(mounts[1].fs_type, "xfs");
    }

    #[test]
    fn cgroup_v2_files() {
        let cpu = "usage_usec 8000000\nuser_usec 6000000\nsystem_usec 2000000\nnr_periods 0\n";
        assert_eq!(parse_cgroup_cpu_usage_usec(cpu), Some(8_000_000));
        assert_eq!(parse_cgroup_cpu_usage_usec("user_usec 1\n"), None);
        assert_eq!(parse_cgroup_u64("524288\n"), Some(524_288));
        assert_eq!(parse_cgroup_u64("junk"), None);
        assert_eq!(parse_cgroup_memory_max("max\n"), None);
        assert_eq!(parse_cgroup_memory_max("1073741824\n"), Some(1 << 30));
        let stat = "anon 1000\nfile 5000\nactive_file 3000\ninactive_file 2000\n";
        assert_eq!(parse_cgroup_inactive_file(stat), Some(2000));
        let io = "8:0 rbytes=1000 wbytes=2000 rios=1 wios=2 dbytes=0 dios=0\n\
259:0 rbytes=10 wbytes=20 rios=1 wios=1 dbytes=0 dios=0\n";
        assert_eq!(parse_cgroup_io_stat(io), (1010, 2020));
        assert_eq!(parse_cgroup_io_stat(""), (0, 0));
    }

    #[test]
    fn container_ids_from_both_cgroup_drivers() {
        let id = "a".repeat(64);
        assert_eq!(
            container_id_from_cgroup_dir(&format!("docker-{id}.scope")),
            Some(id.as_str())
        );
        assert_eq!(container_id_from_cgroup_dir(&id), Some(id.as_str()));
        assert_eq!(container_id_from_cgroup_dir("docker-abc.scope"), None);
        assert_eq!(container_id_from_cgroup_dir("ssh.service"), None);
        assert_eq!(
            container_id_from_cgroup_dir(&format!("docker-{}.scope", "A".repeat(64))),
            None
        );
    }

    #[test]
    fn rates_ignore_counter_resets() {
        assert_eq!(rate_per_sec(100, 400, 10.0), 30.0);
        assert_eq!(rate_per_sec(400, 100, 10.0), 0.0);
        assert_eq!(rate_per_sec(1, 2, 0.0), 0.0);
        // 1.5 s of CPU time in 1 s = 150 % (one and a half cores).
        assert!((container_cpu_percent(0, 1_500_000, 1.0) - 150.0).abs() < 1e-9);
        assert_eq!(percent_of(50, 200), 25.0);
        assert_eq!(percent_of(1, 0), 0.0);
    }

    fn rule() -> ThresholdRule {
        ThresholdRule {
            threshold: 85.0,
            hysteresis: 5.0,
            trigger_samples: 3,
            clear_samples: 2,
        }
    }

    fn feed(tracker: &mut AlertTracker, values: &[f64]) -> Vec<AlertTransition> {
        values
            .iter()
            .map(|v| tracker.observe(*v, &rule()))
            .collect()
    }

    #[test]
    fn alert_fires_once_after_consecutive_breaches() {
        use AlertTransition::*;
        let mut tracker = AlertTracker::default();
        // A single spike, then a broken streak: nothing.
        assert_eq!(
            feed(&mut tracker, &[90.0, 50.0, 90.0, 90.0, 84.9]),
            vec![None; 5]
        );
        assert!(!tracker.firing);
        // Three in a row fire exactly once; staying high sends nothing more.
        assert_eq!(
            feed(&mut tracker, &[85.0, 95.0, 99.0, 99.0, 99.0, 100.0]),
            vec![None, None, Fired, None, None, None]
        );
        assert!(tracker.firing);
    }

    #[test]
    fn alert_hysteresis_prevents_flapping() {
        use AlertTransition::*;
        let mut tracker = AlertTracker {
            firing: true,
            streak: 0,
        };
        // Hovering inside the band (80..85) and around the threshold never
        // resolves, so it never re-fires either.
        let hovering = [84.0, 86.0, 81.0, 84.9, 80.0, 86.0, 83.0];
        assert_eq!(feed(&mut tracker, &hovering), vec![None; hovering.len()]);
        assert!(tracker.firing);
        // One sample below the band is not enough; a broken streak restarts.
        assert_eq!(feed(&mut tracker, &[79.0, 82.0, 79.0]), vec![None; 3]);
        // Two consecutive clear samples resolve exactly once.
        assert_eq!(
            feed(&mut tracker, &[70.0, 10.0, 10.0]),
            vec![Resolved, None, None]
        );
        assert!(!tracker.firing);
        // NaN never changes anything.
        assert_eq!(tracker.observe(f64::NAN, &rule()), None);
    }

    #[test]
    fn full_cycle_produces_alternating_edges() {
        let mut tracker = AlertTracker::default();
        let mut edges = Vec::new();
        for _ in 0..3 {
            for v in [95.0, 95.0, 95.0, 95.0, 20.0, 20.0, 20.0] {
                match tracker.observe(v, &rule()) {
                    AlertTransition::None => {}
                    edge => edges.push(edge),
                }
            }
        }
        assert_eq!(
            edges,
            [
                AlertTransition::Fired,
                AlertTransition::Resolved,
                AlertTransition::Fired,
                AlertTransition::Resolved,
                AlertTransition::Fired,
                AlertTransition::Resolved
            ]
        );
    }

    #[test]
    fn rule_validation() {
        assert!(rule().validate().is_ok());
        let mut bad = rule();
        bad.threshold = 0.0;
        assert!(bad.validate().is_err());
        bad = rule();
        bad.hysteresis = 90.0;
        assert!(bad.validate().is_err());
        bad = rule();
        bad.trigger_samples = 0;
        assert!(bad.validate().is_err());
    }
}
