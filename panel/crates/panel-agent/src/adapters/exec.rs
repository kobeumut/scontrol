//! The ONLY place in this crate that spawns processes.
//!
//! Security rule (see CLAUDE.md): NEVER `format!()` a shell command string.
//! Every call site here uses `Command::new(program).arg(...)` with each
//! argument passed as a discrete, already-validated `String`/`&str`. There is
//! no shell in between, so there is no argument-splitting or metacharacter
//! injection surface, regardless of what bytes are in an argument value.

use panel_core::error::{CoreError, CoreResult};
use std::time::Duration;
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
    "openssl",
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

/// Upper bound for a one-shot command whose caller does not pass its own.
/// Generous enough for `docker compose down` on a large project or a
/// `certbot` challenge; anything that legitimately runs longer streams
/// through [`run_streamed`] or passes an explicit limit.
pub const DEFAULT_EXEC_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// Time a timed-out one-shot command's process group gets between SIGTERM
/// and SIGKILL.
const TIMEOUT_GRACE: Duration = Duration::from_secs(5);

/// Runs `program` with `args`, returning stdout (as UTF-8, lossily decoded)
/// on success. A non-zero exit status becomes a `CoreError::External` that
/// includes stderr for diagnostics. Bounded by [`DEFAULT_EXEC_TIMEOUT`].
pub async fn run(program: &str, args: &[&str]) -> CoreResult<String> {
    check_program(program)?;
    tracing::debug!(program, ?args, "exec");
    let output = output_within(program, args, &[], None, DEFAULT_EXEC_TIMEOUT).await?;
    checked_stdout(program, output)
}

/// Same as [`run`], bounded by `timeout`. When it elapses the child is
/// killed (not left running on the host) and the call fails.
#[cfg(feature = "docker")]
pub async fn run_with_timeout(
    program: &str,
    args: &[&str],
    timeout: std::time::Duration,
) -> CoreResult<String> {
    check_program(program)?;
    tracing::debug!(program, ?args, "exec (timeout)");
    let output = Command::new(program).args(args).kill_on_drop(true).output();
    let output = tokio::time::timeout(timeout, output)
        .await
        .map_err(|_| {
            CoreError::External(format!("{program} timed out after {} s", timeout.as_secs()))
        })?
        .map_err(|e| CoreError::Io(format!("failed to spawn {program}: {e}")))?;
    checked_stdout(program, output)
}

/// Same as [`run`], with `stdin` written to the child. Use it for any input
/// that must not be visible in the process table (`ps` shows argv to every
/// local user), such as SQL that carries a password.
pub async fn run_with_stdin(program: &str, args: &[&str], stdin: &[u8]) -> CoreResult<String> {
    check_program(program)?;
    tracing::debug!(program, ?args, "exec (stdin)");
    let output = output_within(program, args, &[], Some(stdin), DEFAULT_EXEC_TIMEOUT).await?;
    checked_stdout(program, output)
}

/// Same as [`run_with_stdin`], with extra environment variables for the
/// child (e.g. `DOCKER_CONFIG`), and a one-minute limit.
#[cfg(feature = "docker")]
pub async fn run_with_stdin_env(
    program: &str,
    args: &[&str],
    envs: &[(&str, &str)],
    stdin: &[u8],
) -> CoreResult<String> {
    check_program(program)?;
    tracing::debug!(program, ?args, "exec (stdin, env)");
    let output = output_within(program, args, envs, Some(stdin), Duration::from_secs(60)).await?;
    checked_stdout(program, output)
}

/// Runs an already-checked `program` to completion in its own process
/// group and collects its output, with a hard `timeout`.
///
/// Stdout and stderr are drained concurrently with the stdin write, so a
/// chatty child cannot deadlock on a full pipe. When `timeout` elapses
/// (or the future is dropped) the whole group — not only the direct child,
/// which for `docker` is just the CLI in front of a plugin — is stopped:
/// SIGTERM, then SIGKILL after [`TIMEOUT_GRACE`]. That is what keeps a hung
/// `docker compose logs` from piling up as orphaned processes.
async fn output_within(
    program: &str,
    args: &[&str],
    envs: &[(&str, &str)],
    stdin: Option<&[u8]>,
    timeout: Duration,
) -> CoreResult<std::process::Output> {
    use std::process::Stdio;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut child = Command::new(program)
        .args(args)
        .envs(envs.iter().copied())
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| CoreError::Io(format!("failed to spawn {program}: {e}")))?;
    let mut guard = GroupGuard::new(child.id());
    let stdin_pipe = child.stdin.take();
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");

    let finished = tokio::time::timeout(timeout, async {
        let write = async {
            if let (Some(mut pipe), Some(data)) = (stdin_pipe, stdin) {
                match pipe.write_all(data).await {
                    // A child that exits without reading its input is
                    // judged by its exit status, not by our write.
                    Err(e) if e.kind() != std::io::ErrorKind::BrokenPipe => {
                        return Err(CoreError::Io(format!("writing {program} stdin: {e}")));
                    }
                    _ => {}
                }
            }
            Ok(())
        };
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let (written, read_out, read_err) = tokio::join!(
            write,
            stdout.read_to_end(&mut out),
            stderr.read_to_end(&mut err)
        );
        written?;
        read_out.map_err(|e| CoreError::Io(format!("reading {program} stdout: {e}")))?;
        read_err.map_err(|e| CoreError::Io(format!("reading {program} stderr: {e}")))?;
        let status = child
            .wait()
            .await
            .map_err(|e| CoreError::Io(format!("waiting for {program}: {e}")))?;
        Ok(std::process::Output {
            status,
            stdout: out,
            stderr: err,
        })
    })
    .await;

    match finished {
        Ok(result) => {
            // Only a reaped leader means the command is over; on an I/O
            // error the guard stops the group as `child` drops.
            if result.is_ok() {
                guard.disarm();
            }
            result
        }
        Err(_) => {
            if let Some(pgid) = guard.0 {
                stop_group(pgid, &mut child, "-TERM", TIMEOUT_GRACE).await;
                guard.disarm();
            } else {
                child.kill().await.ok();
            }
            tracing::warn!(
                program,
                secs = timeout.as_secs(),
                "exec timed out; process group stopped"
            );
            Err(CoreError::External(format!(
                "{program} timed out after {}s and was stopped",
                timeout.as_secs()
            )))
        }
    }
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
/// the command itself). Bounded by [`DEFAULT_EXEC_TIMEOUT`].
pub async fn run_allow_nonzero(program: &str, args: &[&str]) -> CoreResult<(i32, String, String)> {
    allow_nonzero(program, args, DEFAULT_EXEC_TIMEOUT).await
}

/// [`run_allow_nonzero`] with an explicit limit. When `timeout` elapses the
/// command's whole process group is stopped and a `CoreError::External`
/// is returned.
#[cfg(any(feature = "docker", feature = "antivirus", test))]
pub async fn run_allow_nonzero_within(
    program: &str,
    args: &[&str],
    timeout: Duration,
) -> CoreResult<(i32, String, String)> {
    allow_nonzero(program, args, timeout).await
}

async fn allow_nonzero(
    program: &str,
    args: &[&str],
    timeout: Duration,
) -> CoreResult<(i32, String, String)> {
    check_program(program)?;
    tracing::debug!(program, ?args, ?timeout, "exec (allow non-zero)");
    let output = output_within(program, args, &[], None, timeout).await?;
    Ok((
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    ))
}

/// Starts `program` with its standard streams on the given terminal slave
/// (the web terminal's `docker exec -it`). The child gets its own process
/// group, and dropping the handle kills it.
#[cfg(feature = "docker")]
pub fn spawn_on_tty(
    program: &str,
    args: &[&str],
    slave: &std::fs::File,
) -> CoreResult<tokio::process::Child> {
    use std::process::Stdio;
    check_program(program)?;
    tracing::debug!(program, ?args, "exec (tty)");
    let dup = |what: &str| {
        slave
            .try_clone()
            .map(Stdio::from)
            .map_err(|e| CoreError::Io(format!("duplicating terminal {what}: {e}")))
    };
    Command::new(program)
        .args(args)
        .stdin(dup("stdin")?)
        .stdout(dup("stdout")?)
        .stderr(dup("stderr")?)
        .process_group(0)
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| CoreError::Io(format!("failed to spawn {program}: {e}")))
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
const KILL_GRACE: Duration = Duration::from_secs(10);

/// SIGINT the whole process group (so `docker compose` can stop its build
/// cleanly), then SIGKILL it after [`KILL_GRACE`].
#[cfg(feature = "docker")]
async fn terminate_group(pgid: u32, child: &mut tokio::process::Child) {
    stop_group(pgid, child, "-INT", KILL_GRACE).await;
}

/// Sends `first` to the process group `pgid`, gives the leader `grace` to
/// exit, then SIGKILLs the group regardless: a member that ignored the first
/// signal, or one the leader left behind, must not outlive the command. The
/// leader is reaped before returning, so it cannot linger as a zombie.
async fn stop_group(pgid: u32, child: &mut tokio::process::Child, first: &str, grace: Duration) {
    signal_group(first, pgid).await;
    let exited = tokio::time::timeout(grace, child.wait()).await.is_ok();
    signal_group("-KILL", pgid).await;
    if !exited {
        let _ = child.wait().await;
    }
}

/// `kill <signal> -- -<pgid>`. Spawned directly rather than through
/// [`run_allow_nonzero`] so stopping a group never recurses into another
/// bounded run; `kill` itself returns at once.
async fn signal_group(signal: &str, pgid: u32) {
    let group = format!("-{pgid}");
    let status = Command::new("kill")
        .args([signal, "--", &group])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .status();
    let _ = tokio::time::timeout(Duration::from_secs(5), status).await;
}

/// SIGKILLs a one-shot command's process group if the future running it is
/// dropped before the command finished (the RPC caller went away, an outer
/// timeout fired). `kill_on_drop` alone only reaches the direct child.
struct GroupGuard(Option<u32>);

impl GroupGuard {
    fn new(pgid: Option<u32>) -> Self {
        Self(pgid)
    }

    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for GroupGuard {
    fn drop(&mut self) {
        let Some(pgid) = self.0.take() else { return };
        // Dropped outside a runtime only at shutdown, when there is nothing
        // left to protect. Tokio reaps the dropped `kill` child itself.
        if tokio::runtime::Handle::try_current().is_ok() {
            let group = format!("-{pgid}");
            let _ = Command::new("kill")
                .args(["-KILL", "--", &group])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
        }
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

    #[tokio::test]
    async fn a_command_past_its_limit_is_stopped_and_reported() {
        use std::time::{Duration, Instant};

        let started = Instant::now();
        let error = run_allow_nonzero_within("sleep", &["30"], Duration::from_millis(200))
            .await
            .unwrap_err();
        assert!(matches!(error, CoreError::External(_)), "{error}");
        assert!(error.to_string().contains("timed out"), "{error}");
        // SIGTERM ends `sleep` at once; no wait for the SIGKILL grace.
        assert!(started.elapsed() < TIMEOUT_GRACE, "{:?}", started.elapsed());

        // Within the limit, output and exit status come back as before.
        let (code, stdout, _) =
            run_allow_nonzero_within("printf", &["a\\nb"], Duration::from_secs(10))
                .await
                .unwrap();
        assert_eq!((code, stdout.as_str()), (0, "a\nb"));
    }

    /// Spawns `sleep 30` as a group leader and a second `sleep 30` inside
    /// the same group, standing in for the plugin process `docker` starts.
    fn leader_and_member() -> (tokio::process::Child, tokio::process::Child, u32) {
        let leader = Command::new("sleep")
            .arg("30")
            .process_group(0)
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let pgid = leader.id().unwrap();
        let member = Command::new("sleep")
            .arg("30")
            .process_group(pgid as i32)
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        (leader, member, pgid)
    }

    async fn exits_soon(child: &mut tokio::process::Child) -> bool {
        tokio::time::timeout(std::time::Duration::from_secs(5), child.wait())
            .await
            .is_ok()
    }

    #[tokio::test]
    async fn stopping_a_group_reaches_every_member_not_only_the_leader() {
        let (mut leader, mut member, pgid) = leader_and_member();
        stop_group(pgid, &mut leader, "-TERM", TIMEOUT_GRACE).await;
        assert!(leader.try_wait().unwrap().is_some(), "leader still running");
        assert!(exits_soon(&mut member).await, "group member left behind");
    }

    #[tokio::test]
    async fn cancellation_during_timeout_grace_still_kills_the_group() {
        let dir = std::env::temp_dir().join(format!("panel-timeout-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let pid_file = dir.join("pid");
        let script = format!(
            "echo $$ > {}; trap '' TERM; exec sleep 30",
            pid_file.display()
        );
        let task = tokio::spawn(async move {
            output_within(
                "sh",
                &["-c", &script],
                &[],
                None,
                Duration::from_millis(500),
            )
            .await
        });
        let pgid: u32 = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(pid) = std::fs::read_to_string(&pid_file) {
                    if let Ok(pid) = pid.trim().parse() {
                        break pid;
                    }
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let mut member = Command::new("sh")
            .args(["-c", "trap '' TERM; exec sleep 30"])
            .process_group(pgid as i32)
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert!(member.try_wait().unwrap().is_none());
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(
            exits_soon(&mut member).await,
            "timeout cancellation orphaned a group member"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn dropping_an_unfinished_run_kills_its_group() {
        let (mut leader, mut member, pgid) = leader_and_member();
        drop(GroupGuard::new(Some(pgid)));
        assert!(exits_soon(&mut member).await, "group member left behind");
        assert!(exits_soon(&mut leader).await, "leader left behind");

        // A disarmed guard (the command finished) signals nothing.
        let (mut leader, mut member, pgid) = leader_and_member();
        let mut guard = GroupGuard::new(Some(pgid));
        guard.disarm();
        drop(guard);
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert!(member.try_wait().unwrap().is_none());
        stop_group(pgid, &mut leader, "-KILL", TIMEOUT_GRACE).await;
        assert!(exits_soon(&mut member).await);
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
            r"\b(run|run_allow_nonzero|run_allow_nonzero_within|run_streamed|run_with_stdin|run_with_stdin_env|spawn_piped|follow_lines|exec_replace)\(",
        )
        .unwrap();
        let literal = regex::Regex::new(
            r#"^\b(?:run|run_allow_nonzero|run_allow_nonzero_within|run_streamed|run_with_stdin|run_with_stdin_env|spawn_piped|follow_lines|exec_replace)\(\s*"([^"]+)""#,
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
