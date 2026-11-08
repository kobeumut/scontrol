//! Per-deployment log files for Compose projects.
//!
//! Each deployment streams into `<project>/logs/<deployment_id>.log` while it
//! runs, so the API can tail it by byte offset instead of waiting for the
//! whole build. Output is bounded, and every known secret value (the
//! project's environment values) is masked before it reaches disk.
//!
//! A [`DeployLog`] is also the deployment's run context: it carries the
//! cancellation signal that [`super::exec::run_streamed`] watches.

use panel_core::error::{CoreError, CoreResult};
use panel_core::types::DeploymentLogChunk;
use std::path::{Path, PathBuf};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::sync::watch;
use uuid::Uuid;

const MASK: &str = "********";
/// Values shorter than this are too likely to match ordinary output
/// ("1", "true") to be masked usefully.
const MIN_SECRET_LEN: usize = 4;
const MAX_READ_BYTES: u32 = 256 * 1024;

pub struct DeployLog {
    file: tokio::fs::File,
    written: u64,
    limit: u64,
    truncated: bool,
    secrets: Vec<String>,
    cancel: watch::Receiver<bool>,
}

impl DeployLog {
    /// The values this log masks, longest first (see [`maskable_secrets`]).
    pub fn secrets(&self) -> &[String] {
        &self.secrets
    }

    pub fn extend_secrets(&mut self, secrets: Vec<String>) {
        self.secrets.extend(secrets);
        self.secrets = maskable_secrets(std::mem::take(&mut self.secrets));
    }

    pub async fn create(
        logs_dir: &Path,
        deployment_id: Uuid,
        limit: u64,
        secrets: Vec<String>,
        cancel: watch::Receiver<bool>,
    ) -> CoreResult<Self> {
        tokio::fs::create_dir_all(logs_dir)
            .await
            .map_err(|e| CoreError::Io(format!("creating deployment logs: {e}")))?;
        let path = log_path(logs_dir, deployment_id);
        let mut options = tokio::fs::OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        options.mode(0o640);
        let file = options
            .open(&path)
            .await
            .map_err(|e| CoreError::Io(format!("opening deployment log: {e}")))?;
        let secrets = maskable_secrets(secrets);
        Ok(Self {
            file,
            written: 0,
            limit,
            truncated: false,
            secrets,
            cancel,
        })
    }

    /// Bytes written so far, including panel-authored lines.
    pub fn bytes_written(&self) -> u64 {
        self.written
    }

    /// Whether output reached the limit and the rest was dropped.
    pub fn truncated(&self) -> bool {
        self.truncated
    }

    pub fn is_cancelled(&self) -> bool {
        *self.cancel.borrow()
    }

    /// Resolves once cancellation is requested; never resolves if the
    /// sender is gone without having requested it.
    pub async fn cancelled(&mut self) {
        loop {
            if *self.cancel.borrow_and_update() {
                return;
            }
            if self.cancel.changed().await.is_err() {
                std::future::pending::<()>().await;
            }
        }
    }

    /// Writes a panel-authored step marker, e.g. `==> cloning repository`.
    pub async fn step(&mut self, message: &str) -> CoreResult<()> {
        self.line(format!("==> {message}").as_bytes()).await
    }

    /// Appends one line of tool output (with or without a trailing newline).
    pub async fn line(&mut self, raw: &[u8]) -> CoreResult<()> {
        if self.truncated {
            return Ok(());
        }
        let mut text = redact(&String::from_utf8_lossy(raw), &self.secrets);
        if !text.ends_with('\n') {
            text.push('\n');
        }
        if self.written + text.len() as u64 > self.limit {
            self.truncated = true;
            text = "==> log limit reached; further output is discarded\n".into();
        }
        self.written += text.len() as u64;
        self.file
            .write_all(text.as_bytes())
            .await
            .map_err(|e| CoreError::Io(format!("writing deployment log: {e}")))
    }
}

pub fn log_path(logs_dir: &Path, deployment_id: Uuid) -> PathBuf {
    logs_dir.join(format!("{deployment_id}.log"))
}

/// Prepares secret values for [`redact`]: drops values too short to mask
/// usefully and orders the rest longest first, so a secret containing
/// another is masked whole.
pub fn maskable_secrets(secrets: Vec<String>) -> Vec<String> {
    let mut secrets: Vec<String> = secrets
        .into_iter()
        // Output is line-oriented; a multiline secret must also mask each
        // printed fragment, not only the complete value containing newlines.
        .flat_map(|s| {
            let mut lines: Vec<String> = s.lines().map(str::to_string).collect();
            lines.push(s);
            lines
        })
        .filter(|s| s.len() >= MIN_SECRET_LEN)
        .collect();
    secrets.sort_unstable_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
    secrets.dedup();
    secrets
}

/// Returned errors reach API records as well as logs. Preserve their kind
/// (especially cancellation) while removing the same values as log output.
pub fn redact_error(error: CoreError, secrets: &[String]) -> CoreError {
    match error {
        CoreError::Validation(m) => CoreError::Validation(redact(&m, secrets)),
        CoreError::NotFound(m) => CoreError::NotFound(redact(&m, secrets)),
        CoreError::AlreadyExists(m) => CoreError::AlreadyExists(redact(&m, secrets)),
        CoreError::PermissionDenied(m) => CoreError::PermissionDenied(redact(&m, secrets)),
        CoreError::Io(m) => CoreError::Io(redact(&m, secrets)),
        CoreError::External(m) => CoreError::External(redact(&m, secrets)),
        CoreError::Cancelled(m) => CoreError::Cancelled(redact(&m, secrets)),
    }
}

/// Masks every value of `secrets` (from [`maskable_secrets`]) in `line`.
pub fn redact(line: &str, secrets: &[String]) -> String {
    let mut out = line.to_string();
    for secret in secrets {
        if out.contains(secret.as_str()) {
            out = out.replace(secret.as_str(), MASK);
        }
    }
    out
}

/// Reads up to `max_bytes` starting at `offset`. A chunk never ends inside a
/// multi-byte UTF-8 sequence; the remainder is returned by the next read.
pub async fn read_chunk(
    logs_dir: &Path,
    deployment_id: Uuid,
    offset: u64,
    max_bytes: u32,
) -> CoreResult<DeploymentLogChunk> {
    let path = log_path(logs_dir, deployment_id);
    let mut file = match tokio::fs::File::open(&path).await {
        Ok(file) => file,
        // Queued deployments have not created their log yet.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(DeploymentLogChunk {
                content: String::new(),
                next_offset: offset,
                eof: true,
            })
        }
        Err(e) => return Err(CoreError::Io(format!("opening deployment log: {e}"))),
    };
    let len = file
        .metadata()
        .await
        .map_err(|e| CoreError::Io(format!("reading deployment log metadata: {e}")))?
        .len();
    let start = offset.min(len);
    file.seek(std::io::SeekFrom::Start(start))
        .await
        .map_err(|e| CoreError::Io(format!("seeking deployment log: {e}")))?;
    let want = max_bytes.clamp(1, MAX_READ_BYTES) as u64;
    let mut buf = Vec::with_capacity(want.min(len - start) as usize);
    (&mut file)
        .take(want)
        .read_to_end(&mut buf)
        .await
        .map_err(|e| CoreError::Io(format!("reading deployment log: {e}")))?;
    let valid = match std::str::from_utf8(&buf) {
        Ok(_) => buf.len(),
        // An incomplete trailing sequence: stop before it.
        Err(e) if e.error_len().is_none() => e.valid_up_to(),
        // Genuinely invalid bytes cannot occur (lines are written lossily),
        // but stay total and let the lossy decode below handle them.
        Err(_) => buf.len(),
    };
    let next_offset = start + valid as u64;
    Ok(DeploymentLogChunk {
        content: String::from_utf8_lossy(&buf[..valid]).into_owned(),
        next_offset,
        eof: next_offset >= len,
    })
}

/// Keeps the `keep` most recent deployment logs and removes the rest.
pub async fn prune(logs_dir: &Path, keep: usize) -> CoreResult<()> {
    let mut entries = match tokio::fs::read_dir(logs_dir).await {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(CoreError::Io(format!("listing deployment logs: {e}"))),
    };
    let mut logs = Vec::new();
    while let Some(entry) = entries
        .next_entry()
        .await
        .map_err(|e| CoreError::Io(format!("listing deployment logs: {e}")))?
    {
        let path = entry.path();
        let is_deployment_log = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .is_some_and(|stem| Uuid::parse_str(stem).is_ok())
            && path.extension().is_some_and(|ext| ext == "log");
        if !is_deployment_log {
            continue;
        }
        if let Ok(modified) = entry.metadata().await.and_then(|m| m.modified()) {
            logs.push((modified, path));
        }
    }
    logs.sort_by_key(|entry| std::cmp::Reverse(entry.0));
    for (_, path) in logs.into_iter().skip(keep) {
        tokio::fs::remove_file(&path).await.ok();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn never() -> watch::Receiver<bool> {
        watch::channel(false).1
    }

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("scontrol-log-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn masks_secret_values_and_reads_incrementally() {
        let dir = temp_dir();
        let id = Uuid::new_v4();
        let mut log = DeployLog::create(
            &dir,
            id,
            1024,
            vec!["s3cr3t-token".into(), "1".into()],
            never(),
        )
        .await
        .unwrap();
        log.step("building").await.unwrap();
        log.line(b"using s3cr3t-token for 1 registry\n")
            .await
            .unwrap();

        let first = read_chunk(&dir, id, 0, 8).await.unwrap();
        assert_eq!(first.content, "==> buil");
        assert!(!first.eof);
        let rest = read_chunk(&dir, id, first.next_offset, 4096).await.unwrap();
        assert!(rest.eof);
        let all = format!("{}{}", first.content, rest.content);
        assert!(!all.contains("s3cr3t-token"));
        assert!(all.contains("using ******** for 1 registry"));
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn stops_writing_at_the_limit() {
        let dir = temp_dir();
        let id = Uuid::new_v4();
        let mut log = DeployLog::create(&dir, id, 16, vec![], never())
            .await
            .unwrap();
        log.line(b"0123456789").await.unwrap();
        log.line(b"0123456789").await.unwrap();
        log.line(b"dropped").await.unwrap();
        let chunk = read_chunk(&dir, id, 0, 4096).await.unwrap();
        assert!(chunk
            .content
            .starts_with("0123456789\n==> log limit reached"));
        assert!(!chunk.content.contains("dropped"));
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn chunk_never_splits_a_utf8_character() {
        let dir = temp_dir();
        let id = Uuid::new_v4();
        let mut log = DeployLog::create(&dir, id, 1024, vec![], never())
            .await
            .unwrap();
        log.line("aç".as_bytes()).await.unwrap();
        // "a" (1 byte) + "ç" (2 bytes): a 2-byte read must stop after "a".
        let chunk = read_chunk(&dir, id, 0, 2).await.unwrap();
        assert_eq!(chunk.content, "a");
        assert_eq!(chunk.next_offset, 1);
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn missing_log_reads_as_empty() {
        let dir = temp_dir();
        let chunk = read_chunk(&dir, Uuid::new_v4(), 0, 1024).await.unwrap();
        assert!(chunk.content.is_empty() && chunk.eof);
        std::fs::remove_dir_all(dir).ok();
    }
}
