//! Request statistics from the Nginx JSON access log and scheduled Docker
//! cleanup.
//!
//! The reader is stateless: panel-api passes the file identity (inode) and
//! byte offset it stored, and gets back the buckets for the complete lines
//! after that offset plus the next offset. A partial last line (Nginx is
//! mid-write) is left for the next call, and a truncated or rotated file
//! restarts at 0.

use crate::config::AgentConfig;
use panel_core::adapters::{NginxStatsChunk, ReadNginxStatsParams};
use panel_core::error::{CoreError, CoreResult};
use panel_core::observability::{aggregate, parse_nginx_line};
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

const MIN_READ_BYTES: u64 = 64 * 1024;
const MAX_READ_BYTES: u64 = 8 * 1024 * 1024;

#[cfg(unix)]
fn file_id(meta: &std::fs::Metadata) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    Some(meta.ino())
}

#[cfg(not(unix))]
fn file_id(_meta: &std::fs::Metadata) -> Option<u64> {
    None
}

pub async fn read_stats(
    config: &AgentConfig,
    params: ReadNginxStatsParams,
) -> CoreResult<NginxStatsChunk> {
    let path = config.nginx_stats_log.clone();
    tokio::task::spawn_blocking(move || read_stats_blocking(&path, &params))
        .await
        .map_err(|e| CoreError::Io(format!("reading the access log: {e}")))?
}

pub fn read_stats_blocking(
    path: &Path,
    params: &ReadNginxStatsParams,
) -> CoreResult<NginxStatsChunk> {
    let missing = || NginxStatsChunk {
        buckets: Vec::new(),
        file_id: None,
        next_offset: 0,
        lines: 0,
        skipped: 0,
        log_present: false,
    };
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(missing()),
        Err(e) => return Err(CoreError::Io(format!("opening the access log: {e}"))),
    };
    let meta = file
        .metadata()
        .map_err(|e| CoreError::Io(format!("reading access log metadata: {e}")))?;
    if !meta.is_file() {
        return Err(CoreError::Validation(
            "the access log is not a regular file".into(),
        ));
    }
    let id = file_id(&meta);
    let size = meta.len();
    // A different file, or one shorter than the cursor (truncated): start over.
    let mut offset = params.offset;
    if (params.file_id.is_some() && params.file_id != id) || offset > size {
        offset = 0;
    }
    let limit = u64::from(params.max_bytes).clamp(MIN_READ_BYTES, MAX_READ_BYTES);
    let want = (size - offset).min(limit);
    file.seek(SeekFrom::Start(offset))
        .map_err(|e| CoreError::Io(format!("seeking the access log: {e}")))?;
    let mut data = Vec::with_capacity(want as usize);
    file.take(want)
        .read_to_end(&mut data)
        .map_err(|e| CoreError::Io(format!("reading the access log: {e}")))?;
    // Only complete lines count; the rest waits for the next call. A single
    // line longer than the whole window is skipped so the reader cannot stall.
    let end = match data.iter().rposition(|b| *b == b'\n') {
        Some(i) => i + 1,
        None if data.len() as u64 >= limit => data.len(),
        None => 0,
    };
    let mut entries = Vec::new();
    let (mut lines, mut skipped) = (0u64, 0u64);
    for raw in data[..end].split(|b| *b == b'\n') {
        if raw.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        match std::str::from_utf8(raw).ok().and_then(parse_nginx_line) {
            Some(entry) => {
                lines += 1;
                entries.push(entry);
            }
            None => skipped += 1,
        }
    }
    Ok(NginxStatsChunk {
        buckets: aggregate(entries),
        file_id: id,
        next_offset: offset + end as u64,
        lines,
        skipped,
        log_present: true,
    })
}

/// Fixed argv, run in order. Only the `until=` value varies, and it is built
/// from a number that was range-checked.
#[cfg(feature = "docker")]
pub async fn prune_docker(
    params: panel_core::adapters::PruneDockerParams,
) -> CoreResult<panel_core::adapters::DockerPruneOutcome> {
    use super::exec::run;
    use panel_core::adapters::DockerPruneOutcome;
    use panel_core::observability::{parse_reclaimed_bytes, prune_until_filter};
    let filter = prune_until_filter(params.older_than_hours).map_err(CoreError::Validation)?;
    let timeout = std::time::Duration::from_secs(15 * 60);
    let step = |args: Vec<&'static str>, filter: String| async move {
        let mut argv = args;
        argv.push("--filter");
        let owned = filter;
        let mut refs: Vec<&str> = argv.clone();
        refs.push(&owned);
        tokio::time::timeout(timeout, run("docker", &refs))
            .await
            .map_err(|_| CoreError::External("docker prune timed out".into()))?
    };
    let images = step(vec!["image", "prune", "-a", "-f"], filter.clone()).await?;
    let builder = step(vec!["builder", "prune", "-a", "-f"], filter).await?;
    Ok(DockerPruneOutcome {
        images_freed_bytes: parse_reclaimed_bytes(&images),
        builder_freed_bytes: parse_reclaimed_bytes(&builder),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("scontrol-stats-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn params(offset: u64, file_id: Option<u64>) -> ReadNginxStatsParams {
        ReadNginxStatsParams {
            file_id,
            offset,
            max_bytes: 1 << 20,
        }
    }

    fn line(sec: u32, status: u16) -> String {
        format!(
            "{{\"time\":\"2026-11-04T10:00:{sec:02}+00:00\",\"remote_addr\":\"1.2.3.4\",\"host\":\"a.com\",\"status\":{status},\"request_time\":0.010,\"bytes_sent\":100,\"uri\":\"/\"}}\n"
        )
    }

    #[test]
    fn reads_incrementally_and_waits_for_partial_lines() {
        let dir = TempDir::new();
        let path = dir.0.join("access.json.log");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(line(1, 200).as_bytes()).unwrap();
        f.write_all(line(2, 500).as_bytes()).unwrap();
        f.write_all(b"not json\n").unwrap();
        let partial = line(3, 200);
        f.write_all(&partial.as_bytes()[..30]).unwrap();

        let first = read_stats_blocking(&path, &params(0, None)).unwrap();
        assert_eq!((first.lines, first.skipped), (2, 1));
        assert_eq!(first.buckets.len(), 1);
        assert_eq!(first.buckets[0].status_5xx, 1);
        assert!(
            first.next_offset > 0 && first.next_offset < std::fs::metadata(&path).unwrap().len()
        );

        // Nothing new complete yet: same offset, no lines.
        let again = read_stats_blocking(&path, &params(first.next_offset, first.file_id)).unwrap();
        assert_eq!((again.lines, again.next_offset), (0, first.next_offset));

        f.write_all(&partial.as_bytes()[30..]).unwrap();
        let second = read_stats_blocking(&path, &params(first.next_offset, first.file_id)).unwrap();
        assert_eq!((second.lines, second.skipped), (1, 0));
        assert_eq!(second.next_offset, std::fs::metadata(&path).unwrap().len());
    }

    #[test]
    fn restarts_after_truncation_or_rotation_and_tolerates_a_missing_file() {
        let dir = TempDir::new();
        let path = dir.0.join("access.json.log");
        assert!(
            !read_stats_blocking(&path, &params(5, None))
                .unwrap()
                .log_present
        );

        std::fs::write(&path, line(1, 200) + &line(2, 200)).unwrap();
        let first = read_stats_blocking(&path, &params(0, None)).unwrap();
        assert_eq!(first.lines, 2);

        // Truncated: the offset is past the end.
        std::fs::write(&path, line(9, 404)).unwrap();
        let after = read_stats_blocking(&path, &params(first.next_offset, first.file_id)).unwrap();
        assert_eq!(after.lines, 1);
        assert_eq!(after.buckets[0].status_4xx, 1);

        // Rotated: a new file (new inode) even if it is longer than the cursor.
        std::fs::rename(&path, dir.0.join("old.log")).unwrap();
        std::fs::write(&path, line(1, 200).repeat(5)).unwrap();
        let rotated = read_stats_blocking(&path, &params(10, after.file_id)).unwrap();
        #[cfg(unix)]
        assert_eq!(rotated.lines, 5);
        #[cfg(not(unix))]
        let _ = rotated;
    }

    #[test]
    fn a_giant_line_cannot_stall_the_reader() {
        let dir = TempDir::new();
        let path = dir.0.join("access.json.log");
        std::fs::write(&path, vec![b'x'; (MIN_READ_BYTES * 2) as usize]).unwrap();
        let chunk = read_stats_blocking(
            &path,
            &ReadNginxStatsParams {
                file_id: None,
                offset: 0,
                max_bytes: 1,
            },
        )
        .unwrap();
        assert_eq!(chunk.next_offset, MIN_READ_BYTES);
        assert_eq!(chunk.skipped, 1);
    }
}
