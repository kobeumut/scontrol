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
use std::collections::BTreeMap;

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
