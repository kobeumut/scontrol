//! The ONLY place in this crate that spawns processes.
//!
//! Security rule (see CLAUDE.md): NEVER `format!()` a shell command string.
//! Every call site here uses `Command::new(program).arg(...)` with each
//! argument passed as a discrete, already-validated `String`/`&str`. There is
//! no shell in between, so there is no argument-splitting or metacharacter
//! injection surface, regardless of what bytes are in an argument value.

use panel_core::error::{CoreError, CoreResult};
use tokio::process::Command;

/// Runs `program` with `args`, returning stdout (as UTF-8, lossily decoded)
/// on success. A non-zero exit status becomes a `CoreError::External` that
/// includes stderr for diagnostics.
pub async fn run(program: &str, args: &[&str]) -> CoreResult<String> {
    tracing::debug!(program, ?args, "exec");
    let output = Command::new(program)
        .args(args)
        .output()
        .await
        .map_err(|e| CoreError::Io(format!("failed to spawn {program}: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        return Err(CoreError::External(format!(
            "{program} exited with {status}: {stderr}{stdout}",
            status = output.status,
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Same as [`run`] but tolerates non-zero exit (some tools, e.g. `clamscan`,
/// use exit code 1 to mean "infected files found", which is not a failure of
/// the command itself).
pub async fn run_allow_nonzero(program: &str, args: &[&str]) -> CoreResult<(i32, String, String)> {
    tracing::debug!(program, ?args, "exec (allow non-zero)");
    let output = Command::new(program)
        .args(args)
        .output()
        .await
        .map_err(|e| CoreError::Io(format!("failed to spawn {program}: {e}")))?;
    Ok((
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    ))
}
