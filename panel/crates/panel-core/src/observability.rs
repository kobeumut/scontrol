//! Request statistics from the Nginx JSON access log, and Docker prune
//! output. Pure parsing and aggregation, shared by the agent (which reads
//! the log) and panel-api (which stores the buckets).
//!
//! The log format is installed by `install/install.sh`:
//!
//! ```text
//! log_format scontrol_json escape=json '{"time":"$time_iso8601",...}';
//! ```
//!
//! It deliberately has no request or response header (no `Authorization`,
//! `Cookie`, ...), no query string (`$uri`, not `$request_uri`) and no
//! user agent or referer, so nothing secret can reach the log or the
//! statistics.

use chrono::{DateTime, DurationRound, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};

/// Longest hostname kept (a DNS name is at most 253 characters).
pub const MAX_HOST_LEN: usize = 253;
/// Distinct hosts kept per chunk. `$host` comes from the client's `Host`
/// header, so a flood of random names must not create unbounded rows; the
/// rest is folded into [`OTHER_HOST`].
pub const MAX_HOSTS_PER_CHUNK: usize = 200;
/// Where unknown, invalid or surplus host names are counted.
pub const OTHER_HOST: &str = "_other";

/// One access-log line, as far as statistics need it.
#[derive(Debug, Clone, PartialEq)]
pub struct NginxLogEntry {
    pub time: DateTime<Utc>,
    pub host: String,
    pub status: u16,
    /// `$request_time`, in seconds.
    pub request_time_secs: f64,
    pub bytes_sent: u64,
}

#[derive(Deserialize)]
struct RawLine {
    time: String,
    #[serde(default)]
    host: String,
    status: u16,
    #[serde(default)]
    request_time: f64,
    #[serde(default)]
    bytes_sent: u64,
}

/// A host name as Nginx logs it, lower-cased, without a trailing dot.
/// Anything that is not a plain DNS name or IP literal becomes
/// [`OTHER_HOST`].
pub fn normalize_host(raw: &str) -> String {
    let host = raw.trim().trim_end_matches('.').to_ascii_lowercase();
    let ok = !host.is_empty()
        && host.len() <= MAX_HOST_LEN
        && host.bytes().all(|b| {
            b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_' | b':' | b'[' | b']')
        });
    if ok {
        host
    } else {
        OTHER_HOST.to_string()
    }
}

/// Parses one line. `None` for anything that is not a complete, valid
/// entry (partial writes, other formats, bad numbers).
pub fn parse_nginx_line(line: &str) -> Option<NginxLogEntry> {
    let line = line.trim();
    if !line.starts_with('{') {
        return None;
    }
    let raw: RawLine = serde_json::from_str(line).ok()?;
    if !(100..=599).contains(&raw.status) {
        return None;
    }
    if !raw.request_time.is_finite() || raw.request_time < 0.0 {
        return None;
    }
    let time = DateTime::parse_from_rfc3339(&raw.time)
        .ok()?
        .with_timezone(&Utc);
    Some(NginxLogEntry {
        time,
        host: normalize_host(&raw.host),
        status: raw.status,
        request_time_secs: raw.request_time,
        bytes_sent: raw.bytes_sent,
    })
}

/// Requests of one host in one UTC minute.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RequestBucket {
    pub minute: DateTime<Utc>,
    pub host: String,
    pub requests: u64,
    pub status_2xx: u64,
    pub status_3xx: u64,
    pub status_4xx: u64,
    pub status_5xx: u64,
    /// 1xx and anything else.
    pub status_other: u64,
    pub bytes_sent: u64,
    /// Sum of `$request_time` in milliseconds; divide by `requests` for the
    /// average latency.
    pub request_time_ms_sum: u64,
}

/// Groups entries into per-minute, per-host buckets. At most
/// [`MAX_HOSTS_PER_CHUNK`] distinct hosts are kept.
pub fn aggregate<I: IntoIterator<Item = NginxLogEntry>>(entries: I) -> Vec<RequestBucket> {
    let mut hosts: std::collections::HashSet<String> = Default::default();
    let mut buckets: BTreeMap<(DateTime<Utc>, String), RequestBucket> = BTreeMap::new();
    for entry in entries {
        let minute = entry
            .time
            .duration_trunc(chrono::Duration::minutes(1))
            .unwrap_or(entry.time);
        let host = if hosts.contains(&entry.host) || hosts.len() < MAX_HOSTS_PER_CHUNK {
            hosts.insert(entry.host.clone());
            entry.host
        } else {
            OTHER_HOST.to_string()
        };
        let bucket = buckets
            .entry((minute, host.clone()))
            .or_insert_with(|| RequestBucket {
                minute,
                host,
                ..Default::default()
            });
        bucket.requests += 1;
        match entry.status / 100 {
            2 => bucket.status_2xx += 1,
            3 => bucket.status_3xx += 1,
            4 => bucket.status_4xx += 1,
            5 => bucket.status_5xx += 1,
            _ => bucket.status_other += 1,
        }
        bucket.bytes_sent = bucket.bytes_sent.saturating_add(entry.bytes_sent);
        bucket.request_time_ms_sum = bucket
            .request_time_ms_sum
            .saturating_add((entry.request_time_secs * 1000.0).round() as u64);
    }
    buckets.into_values().collect()
}

/// Longest `until=` age accepted for a prune (one year).
pub const MAX_PRUNE_AGE_HOURS: u32 = 8760;
/// The default: seven days.
pub const DEFAULT_PRUNE_AGE_HOURS: u32 = 168;

/// The `--filter` value for a prune, from a validated age.
pub fn prune_until_filter(older_than_hours: u32) -> Result<String, String> {
    if !(1..=MAX_PRUNE_AGE_HOURS).contains(&older_than_hours) {
        return Err(format!(
            "the prune age must be 1 to {MAX_PRUNE_AGE_HOURS} hours"
        ));
    }
    Ok(format!("until={older_than_hours}h"))
}

/// Bytes from the "reclaimed space" line of `docker image prune` or
/// `docker builder prune`:
///
/// ```text
/// Total reclaimed space: 1.234GB     (image prune)
/// Total:  512MB                      (builder prune)
/// ```
///
/// Docker prints decimal units (kB, MB, GB, TB). Output without such a
/// line (nothing to remove, other versions) counts as 0.
pub fn parse_reclaimed_bytes(output: &str) -> u64 {
    output
        .lines()
        .rev()
        .find_map(|line| {
            let line = line.trim();
            let value = line
                .strip_prefix("Total reclaimed space:")
                .or_else(|| line.strip_prefix("Total:"))?;
            parse_size(value.trim())
        })
        .unwrap_or(0)
}

/// The image total from `docker system df --format '{{.Type}}\t{{.Size}}'`
/// (`Images\t1.234GB`), or `None` when the row is missing.
pub fn parse_df_images_bytes(output: &str) -> Option<u64> {
    output.lines().find_map(|line| {
        let (kind, size) = line.trim().split_once('\t')?;
        (kind.trim() == "Images").then(|| parse_size(size.trim()))?
    })
}

/// Whether `reference` is a rollback release pin
/// (`<slug>/<service>:deploy-<deployment>`, see
/// [`crate::types::deployment_image_tag`]). A cleanup never removes an image
/// that carries one: it is often the only reference to an older release.
pub fn is_release_pin(reference: &str) -> bool {
    let Some((repository, tag)) = reference.rsplit_once(':') else {
        return false;
    };
    repository.contains('/')
        && tag
            .strip_prefix("deploy-")
            .is_some_and(|id| uuid::Uuid::parse_str(id).is_ok())
}

/// A reference as `docker image ls` prints it: Docker Hub prefixes dropped
/// and `:latest` added when there is no tag or digest.
pub fn normalize_image_reference(reference: &str) -> String {
    let name = reference
        .strip_prefix("docker.io/library/")
        .or_else(|| reference.strip_prefix("docker.io/"))
        .unwrap_or(reference);
    let last = name.rsplit('/').next().unwrap_or(name);
    if last.contains(':') || last.contains('@') {
        name.to_string()
    } else {
        format!("{name}:latest")
    }
}

/// `(image ID, reference)` rows from
/// `docker image ls --no-trunc --format '{{.ID}}\t{{.Repository}}:{{.Tag}}'`.
/// Untagged rows are skipped: the dangling prune handles them.
pub fn parse_image_list(output: &str) -> Vec<(String, String)> {
    output
        .lines()
        .filter_map(|line| {
            let (id, reference) = line.trim().split_once('\t')?;
            let (id, reference) = (id.trim(), reference.trim());
            (!id.is_empty() && !reference.is_empty() && !reference.contains("<none>"))
                .then(|| (id.to_string(), reference.to_string()))
        })
        .collect()
}

/// What containers (running or stopped) use, from
/// `docker container inspect --format '{{.Image}}\t{{.Config.Image}}'`:
/// image IDs and the normalized references they were created from.
#[derive(Debug, Default)]
pub struct ImagesInUse {
    pub ids: HashSet<String>,
    pub references: HashSet<String>,
}

impl ImagesInUse {
    pub fn add_inspect_output(&mut self, output: &str) {
        for line in output.lines() {
            let mut fields = line.trim().splitn(2, '\t');
            if let Some(id) = fields.next().map(str::trim).filter(|s| !s.is_empty()) {
                self.ids.insert(id.to_string());
            }
            if let Some(reference) = fields.next().map(str::trim).filter(|s| !s.is_empty()) {
                self.references.insert(normalize_image_reference(reference));
            }
        }
    }
}

/// An image a cleanup may remove, with every reference it carries.
#[derive(Debug, Clone, PartialEq)]
pub struct RemovableImage {
    pub id: String,
    pub references: Vec<String>,
}

/// Tagged images that no container uses and that carry no release pin.
/// An image counts as used when a container runs its ID or was created
/// from one of its references, stopped containers included, so the images
/// of stopped services are kept.
pub fn removable_images(listing: &[(String, String)], in_use: &ImagesInUse) -> Vec<RemovableImage> {
    let mut by_id: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (id, reference) in listing {
        by_id.entry(id).or_default().push(reference);
    }
    by_id
        .into_iter()
        .filter(|(id, references)| {
            !in_use.ids.contains(*id)
                && !references.iter().any(|r| {
                    is_release_pin(r) || in_use.references.contains(&normalize_image_reference(r))
                })
        })
        .map(|(id, references)| RemovableImage {
            id: id.to_string(),
            references: references.into_iter().map(str::to_string).collect(),
        })
        .collect()
}

/// IDs of the images created before `cutoff`, from
/// `docker image inspect --format '{{.Id}}\t{{.Created}}'`. Rows with an
/// unreadable time are left out (kept).
pub fn images_created_before(output: &str, cutoff: DateTime<Utc>) -> HashSet<String> {
    output
        .lines()
        .filter_map(|line| {
            let (id, created) = line.trim().split_once('\t')?;
            let created = DateTime::parse_from_rfc3339(created.trim()).ok()?;
            (created.with_timezone(&Utc) < cutoff).then(|| id.trim().to_string())
        })
        .collect()
}

fn parse_size(text: &str) -> Option<u64> {
    let split = text.find(|c: char| !(c.is_ascii_digit() || c == '.'))?;
    let (number, unit) = text.split_at(split);
    let number: f64 = number.parse().ok()?;
    let factor: f64 = match unit.trim().to_ascii_lowercase().as_str() {
        "b" => 1.0,
        "kb" => 1e3,
        "mb" => 1e6,
        "gb" => 1e9,
        "tb" => 1e12,
        "pb" => 1e15,
        _ => return None,
    };
    let bytes = number * factor;
    if bytes.is_finite() && bytes >= 0.0 {
        Some(bytes.round() as u64)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LINE: &str = r#"{"time":"2026-11-04T10:00:30+03:00","remote_addr":"203.0.113.9","host":"Example.com","status":200,"request_time":0.123,"bytes_sent":1500,"uri":"/index.html"}"#;

    #[test]
    fn parses_a_line_into_utc() {
        let entry = parse_nginx_line(LINE).unwrap();
        assert_eq!(entry.time.to_rfc3339(), "2026-11-04T07:00:30+00:00");
        assert_eq!(entry.host, "example.com");
        assert_eq!(entry.status, 200);
        assert_eq!(entry.bytes_sent, 1500);
    }

    #[test]
    fn rejects_partial_and_foreign_lines() {
        assert!(parse_nginx_line("").is_none());
        assert!(parse_nginx_line("127.0.0.1 - - [04/Nov/2026] \"GET /\" 200").is_none());
        assert!(parse_nginx_line(&LINE[..40]).is_none());
        assert!(parse_nginx_line(r#"{"time":"nope","status":200}"#).is_none());
        assert!(parse_nginx_line(r#"{"time":"2026-11-04T10:00:00Z","status":99}"#).is_none());
        assert!(parse_nginx_line(
            r#"{"time":"2026-11-04T10:00:00Z","status":200,"request_time":-1}"#
        )
        .is_none());
    }

    #[test]
    fn odd_hosts_are_counted_as_other() {
        assert_eq!(normalize_host("sub.Example.com."), "sub.example.com");
        assert_eq!(normalize_host("a b"), OTHER_HOST);
        assert_eq!(normalize_host("x'; DROP"), OTHER_HOST);
        assert_eq!(normalize_host(""), OTHER_HOST);
        assert_eq!(normalize_host(&"a".repeat(300)), OTHER_HOST);
    }

    fn entry(secs: i64, host: &str, status: u16, time: f64, bytes: u64) -> NginxLogEntry {
        NginxLogEntry {
            time: DateTime::from_timestamp(1_800_000_000 + secs, 0).unwrap(),
            host: host.into(),
            status,
            request_time_secs: time,
            bytes_sent: bytes,
        }
    }

    #[test]
    fn aggregates_per_minute_and_host() {
        // 1_800_000_000 is a multiple of 60, so +0..59 s is one minute.
        let buckets = aggregate([
            entry(0, "a.com", 200, 0.1, 100),
            entry(5, "a.com", 404, 0.3, 50),
            entry(10, "a.com", 502, 0.5, 10),
            entry(12, "b.com", 301, 0.0, 5),
            entry(70, "a.com", 200, 0.2, 1),
            entry(71, "a.com", 101, 0.0, 1),
        ]);
        assert_eq!(buckets.len(), 3);
        let a = buckets.iter().find(|b| b.host == "a.com").unwrap();
        assert_eq!(a.requests, 3);
        assert_eq!((a.status_2xx, a.status_4xx, a.status_5xx), (1, 1, 1));
        assert_eq!(a.bytes_sent, 160);
        assert_eq!(a.request_time_ms_sum, 900);
        let later = buckets.iter().filter(|b| b.host == "a.com").nth(1).unwrap();
        assert_eq!(
            (later.requests, later.status_2xx, later.status_other),
            (2, 1, 1)
        );
        assert_eq!(
            buckets
                .iter()
                .find(|b| b.host == "b.com")
                .unwrap()
                .status_3xx,
            1
        );
    }

    #[test]
    fn surplus_hosts_fold_into_other() {
        let entries = (0..MAX_HOSTS_PER_CHUNK + 5)
            .map(|i| entry(0, &format!("h{i}.com"), 200, 0.0, 1))
            .chain([entry(1, "h0.com", 200, 0.0, 1)]);
        let buckets = aggregate(entries);
        assert_eq!(buckets.len(), MAX_HOSTS_PER_CHUNK + 1);
        let other = buckets.iter().find(|b| b.host == OTHER_HOST).unwrap();
        assert_eq!(other.requests, 5);
        assert_eq!(
            buckets
                .iter()
                .find(|b| b.host == "h0.com")
                .unwrap()
                .requests,
            2
        );
    }

    #[test]
    fn prune_filter_is_bounded() {
        assert_eq!(prune_until_filter(168).unwrap(), "until=168h");
        assert!(prune_until_filter(0).is_err());
        assert!(prune_until_filter(MAX_PRUNE_AGE_HOURS + 1).is_err());
    }

    fn listing(rows: &[(&str, &str)]) -> Vec<(String, String)> {
        rows.iter()
            .map(|(id, r)| (id.to_string(), r.to_string()))
            .collect()
    }

    #[test]
    fn release_pins_are_recognised() {
        assert!(is_release_pin(
            "shop/web:deploy-0b7c3f1e-8a51-4d0c-9b8e-2f4a6c1d9e70"
        ));
        assert!(!is_release_pin("shop/web:deploy-latest"));
        assert!(!is_release_pin(
            "web:deploy-0b7c3f1e-8a51-4d0c-9b8e-2f4a6c1d9e70"
        ));
        assert!(!is_release_pin("nginx:1.27"));
        assert!(!is_release_pin("nginx"));
    }

    #[test]
    fn references_are_normalized_like_image_ls() {
        assert_eq!(normalize_image_reference("nginx"), "nginx:latest");
        assert_eq!(
            normalize_image_reference("docker.io/library/nginx:1"),
            "nginx:1"
        );
        assert_eq!(
            normalize_image_reference("docker.io/me/app"),
            "me/app:latest"
        );
        assert_eq!(
            normalize_image_reference("registry:5000/team/app"),
            "registry:5000/team/app:latest"
        );
        assert_eq!(
            normalize_image_reference("ghcr.io/o/app@sha256:ab"),
            "ghcr.io/o/app@sha256:ab"
        );
    }

    #[test]
    fn image_list_skips_untagged_rows() {
        let out = "sha256:a\tnginx:1.27\nsha256:b\t<none>:<none>\nsha256:c\tapp:<none>\n\nbroken\n";
        assert_eq!(
            parse_image_list(out),
            listing(&[("sha256:a", "nginx:1.27")])
        );
    }

    #[test]
    fn removable_images_keep_used_pinned_and_stopped_service_images() {
        let pin = "shop/web:deploy-0b7c3f1e-8a51-4d0c-9b8e-2f4a6c1d9e70";
        let images = listing(&[
            ("sha256:running", "nginx:1.27"),
            ("sha256:stopped-db", "postgres:16"),
            ("sha256:by-name", "redis:latest"),
            ("sha256:old-release", pin),
            ("sha256:old-release", "scontrol-app/shop:previous"),
            ("sha256:unused", "node:18"),
            ("sha256:unused", "node:18-old"),
            ("sha256:gone", "scontrol-app/deleted:latest"),
        ]);
        let mut in_use = ImagesInUse::default();
        // A running and a stopped container by ID; one created by name only.
        in_use.add_inspect_output(
            "sha256:running\tnginx:1.27\nsha256:stopped-db\tpostgres:16\nsha256:other\tredis\n",
        );
        let removable = removable_images(&images, &in_use);
        assert_eq!(
            removable,
            vec![
                RemovableImage {
                    id: "sha256:gone".into(),
                    references: vec!["scontrol-app/deleted:latest".into()],
                },
                RemovableImage {
                    id: "sha256:unused".into(),
                    references: vec!["node:18".into(), "node:18-old".into()],
                },
            ]
        );
    }

    #[test]
    fn image_age_uses_the_creation_time() {
        let cutoff = DateTime::parse_from_rfc3339("2026-11-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let out = "sha256:old\t2026-10-01T10:00:00.123456789Z\nsha256:new\t2026-11-04T10:00:00Z\nsha256:odd\tyesterday\n";
        let old = images_created_before(out, cutoff);
        assert_eq!(old, HashSet::from(["sha256:old".to_string()]));
    }

    #[test]
    fn parses_the_image_total_from_system_df() {
        let out = "Images\t1.5GB\nContainers\t20MB\nLocal Volumes\t0B\nBuild Cache\t3GB\n";
        assert_eq!(parse_df_images_bytes(out), Some(1_500_000_000));
        assert_eq!(parse_df_images_bytes("Containers\t1MB"), None);
    }

    #[test]
    fn parses_reclaimed_space() {
        let image = "Deleted Images:\nuntagged: x\n\nTotal reclaimed space: 1.234GB\n";
        assert_eq!(parse_reclaimed_bytes(image), 1_234_000_000);
        assert_eq!(parse_reclaimed_bytes("Total reclaimed space: 0B\n"), 0);
        assert_eq!(
            parse_reclaimed_bytes("ID\tRECLAIMABLE\nabc\ttrue\n\nTotal:\t512MB\n"),
            512_000_000
        );
        assert_eq!(
            parse_reclaimed_bytes("Total reclaimed space: 12.5kB"),
            12_500
        );
        assert_eq!(
            parse_reclaimed_bytes("Total reclaimed space: 2TB"),
            2_000_000_000_000
        );
        assert_eq!(parse_reclaimed_bytes("nothing here"), 0);
        assert_eq!(parse_reclaimed_bytes("Total reclaimed space: lots"), 0);
    }
}
