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

/// Longest a whole cleanup may take on the agent. panel-api waits a little
/// longer (`services::observability::CLEANUP_TIMEOUT`).
#[cfg(feature = "docker")]
const PRUNE_BUDGET: std::time::Duration = std::time::Duration::from_secs(25 * 60);
/// Longest one prune command may take, and one listing or removal.
#[cfg(feature = "docker")]
const PRUNE_STEP: std::time::Duration = std::time::Duration::from_secs(10 * 60);
#[cfg(feature = "docker")]
const QUERY_STEP: std::time::Duration = std::time::Duration::from_secs(60);
/// IDs per `docker ... inspect` call.
#[cfg(feature = "docker")]
const INSPECT_CHUNK: usize = 100;

/// Fixed argv only; the `until=` value and the age cutoff come from a
/// range-checked number, and every image reference passed back to Docker
/// comes from Docker's own listing and is validated again.
///
/// 1. `docker image prune -f --filter until=<n>h`: dangling images only.
/// 2. Tagged images older than the cutoff that no container (running or
///    stopped) uses and that carry no rollback release pin are removed one
///    reference at a time with `docker image rm` (never `-f`, so Docker
///    still refuses an image a container started using meanwhile). This
///    replaces `image prune -a`, which deleted release pins and so broke
///    rollbacks to older releases.
/// 3. `docker builder prune -a -f --filter until=<n>h`.
///
/// The agent dispatcher excludes managed pulls/builds for this entire
/// operation, even after an API disconnect. External Docker users are not
/// coordinated. Every command and the complete cleanup have time limits.
#[cfg(feature = "docker")]
pub async fn prune_docker(
    params: panel_core::adapters::PruneDockerParams,
) -> CoreResult<panel_core::adapters::DockerPruneOutcome> {
    use super::exec::run_with_timeout;
    use panel_core::adapters::DockerPruneOutcome;
    use panel_core::observability::{
        parse_df_images_bytes, parse_reclaimed_bytes, prune_until_filter,
    };
    let work = async {
        let filter = prune_until_filter(params.older_than_hours).map_err(CoreError::Validation)?;
        let cutoff =
            chrono::Utc::now() - chrono::Duration::hours(i64::from(params.older_than_hours));
        let deadline = tokio::time::Instant::now() + PRUNE_BUDGET;

        let dangling = run_with_timeout(
            "docker",
            &["image", "prune", "-f", "--filter", &filter],
            PRUNE_STEP,
        )
        .await?;
        let images_total = || async {
            run_with_timeout(
                "docker",
                &["system", "df", "--format", "{{.Type}}\t{{.Size}}"],
                QUERY_STEP,
            )
            .await
            .ok()
            .and_then(|out| parse_df_images_bytes(&out))
        };
        let before = images_total().await;
        let removed = remove_unused_images(cutoff, deadline).await;
        let after = if removed > 0 {
            images_total().await
        } else {
            before
        };
        let removed_bytes = match (before, after) {
            (Some(b), Some(a)) => b.saturating_sub(a),
            _ => 0,
        };
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(CoreError::External(
                "docker cleanup ran out of time before the build cache prune".into(),
            ));
        }
        let builder = run_with_timeout(
            "docker",
            &["builder", "prune", "-a", "-f", "--filter", &filter],
            remaining.min(PRUNE_STEP),
        )
        .await?;
        Ok(DockerPruneOutcome {
            images_freed_bytes: parse_reclaimed_bytes(&dangling) + removed_bytes,
            builder_freed_bytes: parse_reclaimed_bytes(&builder),
        })
    };
    // Includes *all* image/container inspection batches. On expiry the
    // exec drop guard kills the active command's process group.
    tokio::time::timeout(PRUNE_BUDGET, work)
        .await
        .map_err(|_| CoreError::External("docker cleanup exceeded its total time budget".into()))?
}

/// Step 2 of [`prune_docker`]. Returns how many references were removed.
/// When what containers use cannot be read, nothing is removed: keeping an
/// image is always the safe side.
#[cfg(feature = "docker")]
async fn remove_unused_images(
    cutoff: chrono::DateTime<chrono::Utc>,
    deadline: tokio::time::Instant,
) -> usize {
    use super::exec::run_with_timeout;
    use panel_core::observability::{
        images_created_before, parse_image_list, removable_images, ImagesInUse,
    };
    use panel_core::types::validate;

    let listing = async {
        let images = run_with_timeout(
            "docker",
            &[
                "image",
                "ls",
                "--no-trunc",
                "--format",
                "{{.ID}}\t{{.Repository}}:{{.Tag}}",
            ],
            QUERY_STEP,
        )
        .await?;
        let containers = run_with_timeout(
            "docker",
            &["container", "ls", "-a", "-q", "--no-trunc"],
            QUERY_STEP,
        )
        .await?;
        let ids: Vec<&str> = containers.split_whitespace().collect();
        let mut in_use = ImagesInUse::default();
        for chunk in ids.chunks(INSPECT_CHUNK) {
            let mut args = vec![
                "container",
                "inspect",
                "--format",
                "{{.Image}}\t{{.Config.Image}}",
                "--",
            ];
            args.extend_from_slice(chunk);
            // A container removed since the listing fails the call; then
            // usage is unknown and nothing is removed.
            in_use.add_inspect_output(&run_with_timeout("docker", &args, QUERY_STEP).await?);
        }
        let candidates = removable_images(&parse_image_list(&images), &in_use);
        let mut old = std::collections::HashSet::new();
        let ids: Vec<&str> = candidates.iter().map(|c| c.id.as_str()).collect();
        for chunk in ids.chunks(INSPECT_CHUNK) {
            let mut args = vec![
                "image",
                "inspect",
                "--format",
                "{{.Id}}\t{{.Created}}",
                "--",
            ];
            args.extend_from_slice(chunk);
            old.extend(images_created_before(
                &run_with_timeout("docker", &args, QUERY_STEP).await?,
                cutoff,
            ));
        }
        CoreResult::Ok(
            candidates
                .into_iter()
                .filter(|c| old.contains(&c.id))
                .collect::<Vec<_>>(),
        )
    };
    let candidates = match listing.await {
        Ok(candidates) => candidates,
        Err(error) => {
            tracing::warn!(%error, "listing unused Docker images failed; none removed");
            return 0;
        }
    };
    let mut removed = 0;
    for reference in candidates.iter().flat_map(|c| &c.references) {
        if tokio::time::Instant::now() >= deadline {
            tracing::warn!("docker cleanup budget used up; remaining images kept");
            break;
        }
        if !validate::is_valid_image_reference(reference) {
            continue;
        }
        // A failure (in use since the listing, already gone) keeps it.
        match run_with_timeout("docker", &["image", "rm", "--", reference], QUERY_STEP).await {
            Ok(_) => removed += 1,
            Err(error) => tracing::debug!(%reference, %error, "image kept"),
        }
    }
    removed
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
