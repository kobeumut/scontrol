//! The ONLY place in this crate that spawns processes.
//!
//! Security rule (see CLAUDE.md): NEVER `format!()` a shell command string.
//! Every call site here uses `Command::new(program).arg(...)` with each
//! argument passed as a discrete, already-validated `String`/`&str`. There is
//! no shell in between, so there is no argument-splitting or metacharacter
//! injection surface, regardless of what bytes are in an argument value.

use panel_core::error::{CoreError, CoreResult};
use tokio::process::Command;

/// Every program the agent may spawn, by bare name (resolved through the
/// service's fixed `PATH`). No shell or interpreter is on the list, so even
/// a call site that slipped past review cannot run `sh -c`. The source scan
/// in this module's tests keeps call sites and this list in step.
const ALLOWED_PROGRAMS: &[&str] = &[
    "certbot",
    "clamscan",
    "crontab",
    "docker",
    "doveadm",
    "git",
    "kill",
    "mongodump",
    "mongorestore",
    "mysql",
    "mysqldump",
    "nginx",
    "nixpacks",
    "pack",
    "pg_dump",
    "postmap",
    "psql",
    "railpack",
    "ssh",
    "ssh-keygen",
    "sudo",
    "systemctl",
    "systemd-analyze",
    "tar",
    "ufw",
    "useradd",
    "userdel",
];

/// Harmless programs that tests may also spawn.
const TEST_PROGRAMS: &[&str] = &["sleep", "printf"];

/// Whether `program` is on the allow-list.
#[cfg(all(test, feature = "builders"))]
pub fn is_allowed_program(program: &str) -> bool {
    ALLOWED_PROGRAMS.contains(&program)
}

fn check_program(program: &str) -> CoreResult<()> {
    if ALLOWED_PROGRAMS.contains(&program) || (cfg!(test) && TEST_PROGRAMS.contains(&program)) {
        Ok(())
    } else {
        Err(CoreError::Validation(format!(
            "refusing to spawn {program}: not an allowed program"
        )))
    }
}

/// Runs `program` with `args`, returning stdout (as UTF-8, lossily decoded)
/// on success. A non-zero exit status becomes a `CoreError::External` that
/// includes stderr for diagnostics.
pub async fn run(program: &str, args: &[&str]) -> CoreResult<String> {
    check_program(program)?;
    tracing::debug!(program, ?args, "exec");
    let output = Command::new(program)
        .args(args)
        .output()
        .await
        .map_err(|e| CoreError::Io(format!("failed to spawn {program}: {e}")))?;
    checked_stdout(program, output)
}

/// Same as [`run`], with `stdin` written to the child. Use it for any input
/// that must not be visible in the process table (`ps` shows argv to every
/// local user), such as SQL that carries a password.
pub async fn run_with_stdin(program: &str, args: &[&str], stdin: &[u8]) -> CoreResult<String> {
    use std::process::Stdio;
    use tokio::io::AsyncWriteExt;

    check_program(program)?;
    tracing::debug!(program, ?args, "exec (stdin)");
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| CoreError::Io(format!("failed to spawn {program}: {e}")))?;
    let mut pipe = child.stdin.take().expect("piped stdin");
    pipe.write_all(stdin)
        .await
        .map_err(|e| CoreError::Io(format!("writing {program} stdin: {e}")))?;
    drop(pipe);
    let output = child
        .wait_with_output()
        .await
        .map_err(|e| CoreError::Io(format!("waiting for {program}: {e}")))?;
    checked_stdout(program, output)
}

/// Same as [`run_with_stdin`], with extra environment variables for the
/// child (e.g. `DOCKER_CONFIG`).
#[cfg(feature = "docker")]
pub async fn run_with_stdin_env(
    program: &str,
    args: &[&str],
    envs: &[(&str, &str)],
    stdin: &[u8],
) -> CoreResult<String> {
    use std::process::Stdio;
    use tokio::io::AsyncWriteExt;

    check_program(program)?;
    tracing::debug!(program, ?args, "exec (stdin, env)");
    let mut child = Command::new(program)
        .args(args)
        .envs(envs.iter().copied())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| CoreError::Io(format!("failed to spawn {program}: {e}")))?;
    let mut pipe = child.stdin.take().expect("piped stdin");
    pipe.write_all(stdin)
        .await
        .map_err(|e| CoreError::Io(format!("writing {program} stdin: {e}")))?;
    drop(pipe);
    let output = tokio::time::timeout(std::time::Duration::from_secs(60), child.wait_with_output())
        .await
        .map_err(|_| CoreError::External(format!("{program} timed out")))?
        .map_err(|e| CoreError::Io(format!("waiting for {program}: {e}")))?;
    checked_stdout(program, output)
}

/// Starts `program` for a streaming pipeline (backups and restores): the
/// caller reads `stdout` and/or writes `stdin` itself, and stderr is
/// captured for diagnostics. `envs` carries credentials, never argv.
pub fn spawn_piped(
    program: &str,
    args: &[&str],
    envs: &[(&str, &str)],
    pipe_stdin: bool,
) -> CoreResult<tokio::process::Child> {
    use std::process::Stdio;

    check_program(program)?;
    tracing::debug!(program, ?args, "exec (piped)");
    Command::new(program)
        .args(args)
        .envs(envs.iter().copied())
        .stdin(if pipe_stdin {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(if pipe_stdin {
            Stdio::null()
        } else {
            Stdio::piped()
        })
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| CoreError::Io(format!("failed to spawn {program}: {e}")))
}

fn checked_stdout(program: &str, output: std::process::Output) -> CoreResult<String> {
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

/// Replaces the current process with `program`. Only the agent's Git
/// helper mode uses it (see `git_auth`), so Git reaches `ssh` with a fixed
/// argv and no shell. Returns only when the exec fails.
#[cfg(feature = "docker")]
pub fn exec_replace(program: &str, args: &[String]) -> CoreError {
    use std::os::unix::process::CommandExt;

    if let Err(error) = check_program(program) {
        return error;
    }
    let error = std::process::Command::new(program).args(args).exec();
    CoreError::Io(format!("failed to exec {program}: {error}"))
}

/// Same as [`run`] but tolerates non-zero exit (some tools, e.g. `clamscan`,
/// use exit code 1 to mean "infected files found", which is not a failure of
/// the command itself).
pub async fn run_allow_nonzero(program: &str, args: &[&str]) -> CoreResult<(i32, String, String)> {
    check_program(program)?;
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

/// Runs `program` and streams stdout/stderr line by line into a deployment
/// log while the process is still running, so the panel can tail it.
///
/// The child runs in its own process group. If the deployment is cancelled
/// or `timeout` elapses, that group (and only that group, unlike a host-wide
/// `pkill`) gets SIGINT, then SIGKILL after [`KILL_GRACE`].
#[cfg(feature = "docker")]
pub async fn run_streamed(
    program: &str,
    args: &[&str],
    envs: &[(&str, &str)],
    log: &mut super::deploy_log::DeployLog,
    timeout: std::time::Duration,
) -> CoreResult<i32> {
    use std::process::Stdio;
    use tokio::io::{AsyncBufReadExt, BufReader};

    check_program(program)?;
    if log.is_cancelled() {
        return Err(CoreError::Cancelled("deployment cancelled".into()));
    }
    tracing::debug!(program, ?args, "exec (streamed)");
    let mut child = Command::new(program)
        .args(args)
        .envs(envs.iter().copied())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| CoreError::Io(format!("failed to spawn {program}: {e}")))?;
    let pgid = child.id();
    let mut stdout = BufReader::new(child.stdout.take().expect("piped stdout"));
    let mut stderr = BufReader::new(child.stderr.take().expect("piped stderr"));
    let deadline = tokio::time::Instant::now() + timeout;

    let (mut out_buf, mut err_buf) = (Vec::new(), Vec::new());
    let (mut out_done, mut err_done) = (false, false);
    let stop_reason = loop {
        if out_done && err_done {
            break None;
        }
        tokio::select! {
            read = stdout.read_until(b'\n', &mut out_buf), if !out_done => {
                let n = read.map_err(|e| CoreError::Io(format!("reading {program} stdout: {e}")))?;
                if n == 0 { out_done = true } else { log.line(&out_buf).await?; out_buf.clear(); }
            }
            read = stderr.read_until(b'\n', &mut err_buf), if !err_done => {
                let n = read.map_err(|e| CoreError::Io(format!("reading {program} stderr: {e}")))?;
                if n == 0 { err_done = true } else { log.line(&err_buf).await?; err_buf.clear(); }
            }
            _ = log.cancelled() => break Some(CoreError::Cancelled("deployment cancelled".into())),
            _ = tokio::time::sleep_until(deadline) => {
                break Some(CoreError::External(format!(
                    "{program} timed out after {}s",
                    timeout.as_secs()
                )))
            }
        }
    };

    if let Some(error) = stop_reason {
        if let Some(pgid) = pgid {
            terminate_group(pgid, &mut child).await;
        }
        return Err(error);
    }
    let status = child
        .wait()
        .await
        .map_err(|e| CoreError::Io(format!("waiting for {program}: {e}")))?;
    Ok(status.code().unwrap_or(-1))
}

/// Runs `program` until it exits, sending each output line (stdout and
/// stderr interleaved) through `map` and on to `tx`. Like [`run_streamed`]
/// the child gets its own process group, and that group is stopped when the
/// receiver goes away or `max_duration` elapses. Lines longer than
/// [`MAX_FOLLOW_LINE`] bytes are cut.
#[cfg(feature = "docker")]
pub async fn follow_lines(
    program: &str,
    args: &[&str],
    tx: &tokio::sync::mpsc::Sender<CoreResult<String>>,
    max_duration: std::time::Duration,
    mut map: impl FnMut(&str) -> Option<String>,
) -> CoreResult<()> {
    use std::process::Stdio;
    use tokio::io::{AsyncBufReadExt, BufReader};

    check_program(program)?;
    tracing::debug!(program, ?args, "exec (follow)");
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| CoreError::Io(format!("failed to spawn {program}: {e}")))?;
    let pgid = child.id();
    let mut stdout = BufReader::new(child.stdout.take().expect("piped stdout"));
    let mut stderr = BufReader::new(child.stderr.take().expect("piped stderr"));
    let deadline = tokio::time::Instant::now() + max_duration;

    let (mut out_buf, mut err_buf) = (Vec::new(), Vec::new());
    let (mut out_done, mut err_done) = (false, false);
    let stop = loop {
        if out_done && err_done {
            break false;
        }
        let (read, buf, done) = tokio::select! {
            read = stdout.read_until(b'\n', &mut out_buf), if !out_done => (read, &mut out_buf, &mut out_done),
            read = stderr.read_until(b'\n', &mut err_buf), if !err_done => (read, &mut err_buf, &mut err_done),
            _ = tx.closed() => break true,
            _ = tokio::time::sleep_until(deadline) => break true,
        };
        if read.map_err(|e| CoreError::Io(format!("reading {program} output: {e}")))? == 0 {
            *done = true;
            continue;
        }
        buf.truncate(MAX_FOLLOW_LINE);
        let text = String::from_utf8_lossy(buf);
        if let Some(line) = map(text.trim_end_matches(['\n', '\r'])) {
            if tx.send(Ok(line)).await.is_err() {
                break true;
            }
        }
        buf.clear();
    };
    if stop {
        if let Some(pgid) = pgid {
            terminate_group(pgid, &mut child).await;
        }
        return Ok(());
    }
    child
        .wait()
        .await
        .map_err(|e| CoreError::Io(format!("waiting for {program}: {e}")))?;
    Ok(())
}

/// Longest line [`follow_lines`] forwards, in bytes.
#[cfg(feature = "docker")]
pub const MAX_FOLLOW_LINE: usize = 16 * 1024;

/// Time a cancelled process group gets to exit after SIGINT.
#[cfg(feature = "docker")]
const KILL_GRACE: std::time::Duration = std::time::Duration::from_secs(10);

/// SIGINT the whole process group (so `docker compose` can stop its build
/// cleanly), then SIGKILL it if it is still alive after [`KILL_GRACE`].
#[cfg(feature = "docker")]
async fn terminate_group(pgid: u32, child: &mut tokio::process::Child) {
    let group = format!("-{pgid}");
    let _ = run_allow_nonzero("kill", &["-INT", "--", &group]).await;
    if tokio::time::timeout(KILL_GRACE, child.wait())
        .await
        .is_err()
    {
        let _ = run_allow_nonzero("kill", &["-KILL", "--", &group]).await;
        let _ = child.wait().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[tokio::test]
    async fn shells_and_unlisted_programs_are_refused() {
        for program in [
            "sh", "bash", "/bin/sh", "dash", "zsh", "env", "python3", "busybox", "", "docker ",
        ] {
            let error = run(program, &["-c", "true"]).await.unwrap_err();
            assert!(
                matches!(error, CoreError::Validation(_)),
                "{program}: {error}"
            );
            assert!(run_allow_nonzero(program, &[]).await.is_err());
            assert!(run_with_stdin(program, &[], b"").await.is_err());
        }
    }

    #[tokio::test]
    async fn stdin_reaches_the_child_without_touching_argv() {
        // `sleep` is the only program tests may add; it ignores stdin, so
        // this checks the pipe is closed (no hang) and stdout is captured.
        assert_eq!(
            run_with_stdin("sleep", &["0"], b"secret").await.unwrap(),
            ""
        );
    }

    #[cfg(feature = "docker")]
    #[tokio::test]
    async fn follower_maps_lines_and_stops_when_the_reader_leaves() {
        use std::time::{Duration, Instant};

        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let filter = |line: &str| (!line.contains("skip")).then(|| line.replace("k3y", "***"));
        follow_lines(
            "printf",
            &["a\\nskip me\\nk3y=1\\n"],
            &tx,
            Duration::from_secs(10),
            filter,
        )
        .await
        .unwrap();
        drop(tx);
        let mut lines = Vec::new();
        while let Some(line) = rx.recv().await {
            lines.push(line.unwrap());
        }
        assert_eq!(lines, ["a", "***=1"]);

        // A long-running follower ends promptly once its receiver is gone.
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        let started = Instant::now();
        let follower = tokio::spawn(async move {
            follow_lines("sleep", &["30"], &tx, Duration::from_secs(60), |l| {
                Some(l.into())
            })
            .await
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        drop(rx);
        follower.await.unwrap().unwrap();
        assert!(started.elapsed() < Duration::from_secs(5));

        // And when its time limit elapses.
        let (tx, _rx) = tokio::sync::mpsc::channel(16);
        let started = Instant::now();
        follow_lines("sleep", &["30"], &tx, Duration::from_millis(200), |l| {
            Some(l.into())
        })
        .await
        .unwrap();
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    fn rust_files(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                rust_files(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }

    /// Static guard for the "typed argv, no shell" rule: processes are only
    /// spawned in this module, and every call site names its program as a
    /// string literal from [`ALLOWED_PROGRAMS`], never a computed value.
    #[test]
    fn every_spawn_goes_through_exec_with_a_literal_allowed_program() {
        let call = regex::Regex::new(
            r"\b(run|run_allow_nonzero|run_streamed|run_with_stdin|run_with_stdin_env|spawn_piped|follow_lines|exec_replace)\(",
        )
        .unwrap();
        let literal = regex::Regex::new(
            r#"^\b(?:run|run_allow_nonzero|run_streamed|run_with_stdin|run_with_stdin_env|spawn_piped|follow_lines|exec_replace)\(\s*"([^"]+)""#,
        )
        .unwrap();
        let mut files = Vec::new();
        rust_files(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
            &mut files,
        );
        assert!(files.len() > 10, "source scan found too few files");

        let mut sites = 0;
        for file in &files {
            // Comments may name `Command::new`; only code counts.
            let source: String = std::fs::read_to_string(file)
                .unwrap()
                .lines()
                .filter(|line| !line.trim_start().starts_with("//"))
                .map(|line| format!("{line}\n"))
                .collect();
            let is_exec = file.ends_with("adapters/exec.rs");
            // This module's own tests deliberately pass refused programs.
            let source = match source.find("mod tests {") {
                Some(end) if is_exec => source[..end].to_string(),
                _ => source,
            };
            if !is_exec {
                for forbidden in ["Command::new", "process::Command", "std::process::"] {
                    assert!(
                        !source.contains(forbidden),
                        "{} uses {forbidden}; spawn through adapters::exec",
                        file.display()
                    );
                }
            }
            for found in call.find_iter(&source) {
                let before = &source[..found.start()];
                if before.ends_with("fn ") || before.ends_with("fn  ") {
                    continue;
                }
                let rest = &source[found.start()..];
                let program = literal
                    .captures(rest)
                    .map(|c| c[1].to_string())
                    .unwrap_or_else(|| {
                        panic!(
                            "{}: process call without a literal program: {}",
                            file.display(),
                            rest.lines().next().unwrap_or_default()
                        )
                    });
                assert!(
                    ALLOWED_PROGRAMS.contains(&program.as_str())
                        || TEST_PROGRAMS.contains(&program.as_str()),
                    "{}: {program} is not in ALLOWED_PROGRAMS",
                    file.display()
                );
                sites += 1;
            }
        }
        assert!(sites > 30, "source scan found only {sites} call sites");
    }
}
