//! On-demand ClamAV scanning (only compiled in when the `antivirus` cargo
//! feature is enabled).

use super::exec::run_allow_nonzero;
use panel_core::error::{CoreError, CoreResult};
use panel_core::types::ScanResult;

pub async fn scan_path(path: &str) -> CoreResult<ScanResult> {
    if path.is_empty() || !path.starts_with('/') || path.contains("..") {
        return Err(CoreError::Validation(format!("invalid scan path: {path}")));
    }
    let started_at = chrono::Utc::now();

    // clamscan exits 0 (clean), 1 (infected files found), or 2 (error).
    let (code, stdout, stderr) =
        run_allow_nonzero("clamscan", &["-r", "--infected", "--no-summary", path]).await?;
    if code == 2 {
        return Err(CoreError::External(format!("clamscan error: {stderr}")));
    }

    let infected_files: Vec<String> = stdout
        .lines()
        .filter_map(|l| l.strip_suffix(" FOUND"))
        .map(|l| l.trim().to_string())
        .collect();

    let scanned_files = count_scanned_files(path).await;
    let finished_at = chrono::Utc::now();

    Ok(ScanResult {
        path: path.to_string(),
        infected_files,
        scanned_files,
        started_at,
        finished_at,
    })
}

async fn count_scanned_files(path: &str) -> u64 {
    let meta = tokio::fs::metadata(path).await;
    match meta {
        Ok(m) if m.is_file() => 1,
        _ => walk_count(std::path::Path::new(path)).await,
    }
}

fn walk_count(
    dir: &std::path::Path,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = u64> + Send + '_>> {
    Box::pin(async move {
        let mut count = 0u64;
        if let Ok(mut entries) = tokio::fs::read_dir(dir).await {
            while let Ok(Some(entry)) = entries.next_entry().await {
                let path = entry.path();
                if path.is_dir() {
                    count += walk_count(&path).await;
                } else {
                    count += 1;
                }
            }
        }
        count
    })
}
