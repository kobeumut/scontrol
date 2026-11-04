//! Web terminal sessions: an interactive `docker exec -it <container> <shell>`
//! on a pseudoterminal.
//!
//! Safety properties, enforced by construction:
//! * The only program started is `docker` with the argv
//!   `exec -it -e TERM=xterm-256color <container-id> <shell>`; the shell is
//!   one of [`TerminalShell`]'s three values. No host shell exists anywhere
//!   on this path and no caller-supplied string reaches argv.
//! * The container is resolved from the Compose project label pair
//!   (`com.docker.compose.project` + `.service`), so only service containers
//!   of a scontrol project can be reached, and the resolved id must be a
//!   hexadecimal container id.
//! * Sessions are capped in number, buffered output is bounded, and idle
//!   sessions are closed.

use super::exec;
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use panel_core::adapters::{
    OpenTerminalParams, TerminalCloseParams, TerminalOutput, TerminalReadParams,
    TerminalResizeParams, TerminalSessionInfo, TerminalWriteParams,
};
use panel_core::error::{CoreError, CoreResult};
use panel_core::types::validate;
use rustix::pty::{grantpt, openpt, ptsname, unlockpt, OpenptFlags};
use rustix::termios::{tcsetwinsize, Winsize};
use std::collections::HashMap;
use std::io::Read;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt;
use tokio::sync::Notify;
use uuid::Uuid;

const MAX_SESSIONS: usize = 8;
const MAX_BUFFERED: usize = 1024 * 1024;
const MAX_WRITE: usize = 64 * 1024;
const MAX_WAIT: Duration = Duration::from_secs(25);
const IDLE_LIMIT: Duration = Duration::from_secs(15 * 60);

struct Session {
    buffer: Mutex<Vec<u8>>,
    /// Signalled when output arrives or the process exits.
    output: Notify,
    /// Signalled when the buffer was drained (reader backpressure).
    drained: Notify,
    exited: Mutex<Option<i32>>,
    writer: tokio::sync::Mutex<tokio::fs::File>,
    master: std::fs::File,
    last_activity: Mutex<Instant>,
    kill: tokio::sync::watch::Sender<bool>,
}

#[derive(Default)]
pub struct Terminals {
    sessions: Mutex<HashMap<Uuid, Arc<Session>>>,
}

fn clamp_size(cols: u16, rows: u16) -> CoreResult<Winsize> {
    if !(1..=500).contains(&cols) || !(1..=300).contains(&rows) {
        return Err(CoreError::Validation("terminal size out of range".into()));
    }
    Ok(Winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    })
}

/// argv of the one command a terminal runs (exposed for tests).
pub fn exec_argv(container_id: &str, shell: panel_core::adapters::TerminalShell) -> Vec<String> {
    vec![
        "exec".into(),
        "-it".into(),
        "-e".into(),
        "TERM=xterm-256color".into(),
        container_id.into(),
        shell.program().into(),
    ]
}

fn is_container_id(id: &str) -> bool {
    (12..=64).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_hexdigit())
}

async fn resolve_container(slug: &str, service: &str) -> CoreResult<String> {
    if !validate::is_valid_project_slug(slug) || !validate::is_valid_service_name(service) {
        return Err(CoreError::Validation("invalid project or service".into()));
    }
    let project = format!("label=com.docker.compose.project={slug}");
    let service = format!("label=com.docker.compose.service={service}");
    let out = exec::run(
        "docker",
        &[
            "ps",
            "-q",
            "--no-trunc",
            "--filter",
            &project,
            "--filter",
            &service,
        ],
    )
    .await?;
    let id = out.lines().next().map(str::trim).unwrap_or_default();
    if !is_container_id(id) {
        return Err(CoreError::NotFound(
            "service has no running container".into(),
        ));
    }
    Ok(id.to_string())
}

impl Terminals {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn get(&self, id: Uuid) -> CoreResult<Arc<Session>> {
        self.sessions
            .lock()
            .unwrap()
            .get(&id)
            .cloned()
            .ok_or_else(|| CoreError::NotFound("terminal session not found".into()))
    }

    pub async fn open(
        self: &Arc<Self>,
        params: OpenTerminalParams,
    ) -> CoreResult<TerminalSessionInfo> {
        let size = clamp_size(params.cols, params.rows)?;
        self.reap_idle();
        if self.sessions.lock().unwrap().len() >= MAX_SESSIONS {
            return Err(CoreError::PermissionDenied(
                "too many open terminals".into(),
            ));
        }
        let container = resolve_container(&params.project_slug, &params.service_name).await?;

        let master = openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY | OpenptFlags::CLOEXEC)
            .map_err(|e| CoreError::Io(format!("opening pty: {e}")))?;
        grantpt(&master).map_err(|e| CoreError::Io(format!("grantpt: {e}")))?;
        unlockpt(&master).map_err(|e| CoreError::Io(format!("unlockpt: {e}")))?;
        let slave_path =
            ptsname(&master, Vec::new()).map_err(|e| CoreError::Io(format!("ptsname: {e}")))?;
        let slave = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(std::ffi::OsStr::new(&*String::from_utf8_lossy(
                slave_path.to_bytes(),
            )))
            .map_err(|e| CoreError::Io(format!("opening pty slave: {e}")))?;
        tcsetwinsize(&master, size).map_err(|e| CoreError::Io(format!("pty size: {e}")))?;

        let argv = exec_argv(&container, params.shell);
        let argv_refs: Vec<&str> = argv.iter().map(String::as_str).collect();
        let mut child = exec::spawn_on_tty("docker", &argv_refs, &slave)?;
        drop(slave);

        let master_file = std::fs::File::from(master);
        let reader_file = master_file
            .try_clone()
            .map_err(|e| CoreError::Io(format!("cloning pty: {e}")))?;
        let writer_file = master_file
            .try_clone()
            .map_err(|e| CoreError::Io(format!("cloning pty: {e}")))?;
        let (kill, mut kill_rx) = tokio::sync::watch::channel(false);
        let session = Arc::new(Session {
            buffer: Mutex::new(Vec::new()),
            output: Notify::new(),
            drained: Notify::new(),
            exited: Mutex::new(None),
            writer: tokio::sync::Mutex::new(tokio::fs::File::from_std(writer_file)),
            master: master_file,
            last_activity: Mutex::new(Instant::now()),
            kill,
        });
        let id = Uuid::new_v4();
        self.sessions.lock().unwrap().insert(id, session.clone());
        tracing::info!(session = %id, project = %params.project_slug, service = %params.service_name, actor = %params.actor, "terminal opened");

        // Reader: blocking reads on a dedicated thread, bounded buffer.
        let reader_session = session.clone();
        let handle = tokio::runtime::Handle::current();
        std::thread::spawn(move || {
            let mut file = reader_file;
            let mut chunk = [0u8; 8192];
            loop {
                while reader_session.buffer.lock().unwrap().len() >= MAX_BUFFERED {
                    if *reader_session.kill.borrow() {
                        return;
                    }
                    handle.block_on(async {
                        tokio::time::timeout(
                            Duration::from_millis(200),
                            reader_session.drained.notified(),
                        )
                        .await
                        .ok();
                    });
                }
                match file.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        reader_session
                            .buffer
                            .lock()
                            .unwrap()
                            .extend_from_slice(&chunk[..n]);
                        reader_session.output.notify_one();
                    }
                }
            }
        });
        // Waiter: records the exit status; a kill request stops the child.
        let waiter_session = session.clone();
        tokio::spawn(async move {
            let code = tokio::select! {
                status = child.wait() => status.ok().and_then(|s| s.code()).unwrap_or(-1),
                _ = kill_rx.changed() => {
                    child.kill().await.ok();
                    -1
                }
            };
            *waiter_session.exited.lock().unwrap() = Some(code);
            waiter_session.output.notify_one();
        });
        Ok(TerminalSessionInfo { session_id: id })
    }

    pub async fn read(&self, params: TerminalReadParams) -> CoreResult<TerminalOutput> {
        let session = self.get(params.session_id)?;
        *session.last_activity.lock().unwrap() = Instant::now();
        let wait = Duration::from_millis(params.wait_ms).min(MAX_WAIT);
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            let (data, exited) = {
                let mut buffer = session.buffer.lock().unwrap();
                let data = std::mem::take(&mut *buffer);
                (data, *session.exited.lock().unwrap())
            };
            if !data.is_empty() {
                session.drained.notify_one();
                return Ok(TerminalOutput {
                    data: B64.encode(data),
                    exited: None,
                });
            }
            if exited.is_some() {
                // Output the reader had not delivered yet shows up as data
                // on the next pass; give it a moment before reporting exit.
                tokio::time::sleep(Duration::from_millis(50)).await;
                if session.buffer.lock().unwrap().is_empty() {
                    self.sessions.lock().unwrap().remove(&params.session_id);
                    return Ok(TerminalOutput {
                        data: String::new(),
                        exited,
                    });
                }
                continue;
            }
            if tokio::time::timeout_at(deadline, session.output.notified())
                .await
                .is_err()
            {
                return Ok(TerminalOutput::default());
            }
        }
    }

    pub async fn write(&self, params: TerminalWriteParams) -> CoreResult<()> {
        let session = self.get(params.session_id)?;
        let data = B64
            .decode(params.data.as_bytes())
            .map_err(|_| CoreError::Validation("terminal input is not base64".into()))?;
        if data.len() > MAX_WRITE {
            return Err(CoreError::Validation("terminal input too large".into()));
        }
        *session.last_activity.lock().unwrap() = Instant::now();
        let mut writer = session.writer.lock().await;
        writer
            .write_all(&data)
            .await
            .map_err(|e| CoreError::Io(format!("writing to terminal: {e}")))?;
        writer.flush().await.ok();
        Ok(())
    }

    pub async fn resize(&self, params: TerminalResizeParams) -> CoreResult<()> {
        let session = self.get(params.session_id)?;
        let size = clamp_size(params.cols, params.rows)?;
        tcsetwinsize(&session.master, size)
            .map_err(|e| CoreError::Io(format!("resizing terminal: {e}")))
    }

    pub async fn close(&self, params: TerminalCloseParams) -> CoreResult<()> {
        if let Some(session) = self.sessions.lock().unwrap().remove(&params.session_id) {
            session.kill.send(true).ok();
            tracing::info!(session = %params.session_id, "terminal closed");
        }
        Ok(())
    }

    fn reap_idle(&self) {
        let mut sessions = self.sessions.lock().unwrap();
        sessions.retain(|id, session| {
            let idle = session.last_activity.lock().unwrap().elapsed() > IDLE_LIMIT;
            if idle {
                session.kill.send(true).ok();
                tracing::info!(session = %id, "idle terminal closed");
            }
            !idle
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use panel_core::adapters::TerminalShell;

    #[test]
    fn argv_is_a_fixed_docker_exec_with_a_whitelisted_shell() {
        let id = "0123456789abcdef0123456789abcdef";
        let argv = exec_argv(id, TerminalShell::Bash);
        assert_eq!(
            argv,
            ["exec", "-it", "-e", "TERM=xterm-256color", id, "bash"]
        );
        for shell in [TerminalShell::Sh, TerminalShell::Ash, TerminalShell::Bash] {
            let argv = exec_argv(id, shell);
            assert!(["sh", "ash", "bash"].contains(&argv[5].as_str()));
            assert_eq!(argv.len(), 6);
        }
    }

    #[test]
    fn only_hex_container_ids_are_accepted() {
        assert!(is_container_id("0123456789abcdef0123456789abcdef"));
        assert!(!is_container_id("short"));
        assert!(!is_container_id("0123456789ab; rm -rf /"));
        assert!(!is_container_id("--privileged-----"));
    }

    #[tokio::test]
    async fn a_process_on_the_pty_slave_reaches_the_master_and_resizes() {
        let master =
            openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY | OpenptFlags::CLOEXEC).unwrap();
        grantpt(&master).unwrap();
        unlockpt(&master).unwrap();
        let name = ptsname(&master, Vec::new()).unwrap();
        let slave = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(std::ffi::OsStr::new(&*String::from_utf8_lossy(
                name.to_bytes(),
            )))
            .unwrap();
        tcsetwinsize(&master, clamp_size(100, 30).unwrap()).unwrap();
        let mut child = exec::spawn_on_tty("printf", &["hello-pty"], &slave).unwrap();
        child.wait().await.unwrap();
        let mut file = std::fs::File::from(master);
        let mut buf = [0u8; 64];
        let n = file.read(&mut buf).unwrap();
        assert!(String::from_utf8_lossy(&buf[..n]).contains("hello-pty"));
        assert!(clamp_size(501, 24).is_err());
    }

    #[tokio::test]
    async fn invalid_targets_and_sizes_are_rejected_before_any_process_starts() {
        let terminals = Terminals::new();
        let params = |slug: &str, service: &str, cols: u16| OpenTerminalParams {
            project_slug: slug.into(),
            service_name: service.into(),
            shell: TerminalShell::Sh,
            cols,
            rows: 24,
            actor: "t".into(),
        };
        assert!(terminals.open(params("../x", "web", 80)).await.is_err());
        assert!(terminals.open(params("shop", "we b", 80)).await.is_err());
        assert!(terminals.open(params("shop", "web", 0)).await.is_err());
        assert!(terminals
            .read(TerminalReadParams {
                session_id: Uuid::new_v4(),
                wait_ms: 0
            })
            .await
            .is_err());
    }
}
