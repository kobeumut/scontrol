//! Streaming backups to S3-compatible storage.
//!
//! A database dump (or a volume `tar`) is read from the child's stdout,
//! gzip-compressed in-process, and fed straight into a multipart upload, so
//! no dump file is ever written to disk and memory is bounded by one part
//! (see `s3.rs`). Restores run the pipe the other way: S3 download, gunzip,
//! the restore tool's stdin.
//!
//! Secrets: database passwords reach the tools through their environment
//! (`PGPASSWORD`, `MYSQL_PWD`) or, for MongoDB, a 0600 config file that is
//! deleted right after. S3 keys never leave [`S3Client`]. Every error text
//! and tool stderr that is surfaced passes through [`redact`].

use super::exec::spawn_piped;
use super::s3::{redact, S3Client};
use async_compression::tokio::bufread::{GzipDecoder, GzipEncoder};
use chrono::Utc;
use panel_core::adapters::*;
use panel_core::error::{CoreError, CoreResult};
use panel_core::types::validate;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::process::Child;

fn ensure_target(target: &S3Target) -> CoreResult<()> {
    if validate::normalize_s3_endpoint(&target.endpoint).is_none()
        || !validate::is_valid_s3_bucket(&target.bucket)
        || !validate::is_valid_s3_region(&target.region)
        || !validate::is_valid_s3_access_key(&target.access_key)
        || !validate::is_valid_s3_secret_key(&target.secret_key)
    {
        return Err(CoreError::Validation("invalid S3 destination".into()));
    }
    Ok(())
}

fn ensure_path(path: &str, what: &str) -> CoreResult<()> {
    if validate::is_valid_backup_path(path) {
        Ok(())
    } else {
        Err(CoreError::Validation(format!("invalid backup {what}")))
    }
}

fn ensure_connection(c: &DatabaseConnection) -> CoreResult<()> {
    if !validate::is_valid_identifier(&c.database)
        || !validate::is_valid_identifier(&c.username)
        || !validate::is_valid_db_host(&c.host)
        || c.port == 0
        || !validate::is_valid_db_password(&c.password)
    {
        return Err(CoreError::Validation("invalid database connection".into()));
    }
    Ok(())
}

/// `<prefix>/<name>-<UTC timestamp>.<ext>`. Timestamps sort lexically, which
/// retention relies on.
pub fn object_key(prefix: &str, name: &str, extension: &str) -> String {
    format!(
        "{prefix}/{name}-{}.{extension}",
        Utc::now().format("%Y%m%dT%H%M%SZ")
    )
}

/// Deletes the oldest backups named `<prefix>/<name>-*` beyond `keep`.
pub async fn prune(client: &S3Client, prefix: &str, name: &str, keep: u32) -> CoreResult<u32> {
    if keep == 0 {
        return Ok(0);
    }
    let listing = client.list(&format!("{prefix}/{name}-")).await?;
    let excess = listing.len().saturating_sub(keep as usize);
    for object in listing.iter().take(excess) {
        client.delete(&object.key).await?;
    }
    Ok(excess as u32)
}

/// Compresses `source` and uploads it. Returns the compressed size.
pub async fn upload_compressed<R: AsyncRead + Unpin>(
    client: &S3Client,
    key: &str,
    source: R,
) -> CoreResult<u64> {
    let mut gz = GzipEncoder::new(BufReader::new(source));
    client.upload_stream(key, &mut gz).await
}

/// Downloads `key`, decompresses it, and writes it to `sink`.
pub async fn download_decompressed<W: AsyncWrite + Unpin + Send>(
    client: &S3Client,
    key: &str,
    sink: &mut W,
) -> CoreResult<()> {
    let (mut tx, rx) = tokio::io::duplex(256 * 1024);
    let download = async move {
        let result = client.download_to(key, &mut tx).await;
        drop(tx);
        result.map(|_| ())
    };
    let decode = async {
        let mut gz = GzipDecoder::new(BufReader::new(rx));
        tokio::io::copy(&mut gz, sink)
            .await
            .map_err(|e| CoreError::External(format!("backup is not valid gzip data: {e}")))?;
        sink.flush()
            .await
            .map_err(|e| CoreError::Io(format!("restore pipe: {e}")))
    };
    let (d, w) = tokio::join!(download, decode);
    // A download error is the root cause; a decode error follows from it.
    d?;
    w
}

async fn read_stderr<R: AsyncRead + Unpin>(mut stderr: R) -> String {
    let mut buf = Vec::new();
    let _ = (&mut stderr).take(8192).read_to_end(&mut buf).await;
    // Keep draining so the child never blocks on a full pipe.
    let _ = tokio::io::copy(&mut stderr, &mut tokio::io::sink()).await;
    String::from_utf8_lossy(&buf).trim().to_string()
}

/// A secret-bearing file that is removed on drop.
struct TempSecretFile(std::path::PathBuf);

impl TempSecretFile {
    async fn create(contents: &str) -> CoreResult<Self> {
        use std::os::unix::fs::OpenOptionsExt;
        let path =
            std::env::temp_dir().join(format!("scontrol-mongo-{}.yml", uuid::Uuid::new_v4()));
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .map_err(|e| CoreError::Io(format!("creating tool config: {e}")))?;
        std::io::Write::write_all(&mut file, contents.as_bytes())
            .map_err(|e| CoreError::Io(format!("writing tool config: {e}")))?;
        Ok(Self(path))
    }
}

impl Drop for TempSecretFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// The tool, its argv, and its environment for one connection.
struct ToolCommand {
    program: &'static str,
    args: Vec<String>,
    env: Vec<(&'static str, String)>,
    _config: Option<TempSecretFile>,
}

impl ToolCommand {
    fn refs(&self) -> (Vec<&str>, Vec<(&str, &str)>) {
        (
            self.args.iter().map(String::as_str).collect(),
            self.env.iter().map(|(k, v)| (*k, v.as_str())).collect(),
        )
    }
}

async fn tool_command(c: &DatabaseConnection, restore: bool) -> CoreResult<ToolCommand> {
    let port = c.port.to_string();
    let s = |v: &str| v.to_string();
    Ok(match (c.engine, restore) {
        (BackupEngine::Postgres, false) => ToolCommand {
            program: "pg_dump",
            args: vec![
                s("--no-password"),
                s("--clean"),
                s("--if-exists"),
                s("--no-owner"),
                s("-h"),
                s(&c.host),
                s("-p"),
                port,
                s("-U"),
                s(&c.username),
                s("-d"),
                s(&c.database),
            ],
            env: vec![("PGPASSWORD", c.password.clone())],
            _config: None,
        },
        (BackupEngine::Postgres, true) => ToolCommand {
            program: "psql",
            args: vec![
                s("--no-password"),
                s("-v"),
                s("ON_ERROR_STOP=1"),
                s("-q"),
                s("-h"),
                s(&c.host),
                s("-p"),
                port,
                s("-U"),
                s(&c.username),
                s("-d"),
                s(&c.database),
            ],
            env: vec![("PGPASSWORD", c.password.clone())],
            _config: None,
        },
        (BackupEngine::Mysql, false) => ToolCommand {
            program: "mysqldump",
            args: vec![
                s("--single-transaction"),
                s("--routines"),
                s("-h"),
                s(&c.host),
                s("-P"),
                port,
                s("-u"),
                s(&c.username),
                s(&c.database),
            ],
            env: vec![("MYSQL_PWD", c.password.clone())],
            _config: None,
        },
        (BackupEngine::Mysql, true) => ToolCommand {
            program: "mysql",
            args: vec![
                s("-h"),
                s(&c.host),
                s("-P"),
                port,
                s("-u"),
                s(&c.username),
                s(&c.database),
            ],
            env: vec![("MYSQL_PWD", c.password.clone())],
            _config: None,
        },
        (BackupEngine::Mongodb, restore) => {
            // The mongo tools take a password only on argv or in a config
            // file; use the file. YAML single-quoted scalar: `'` doubles.
            let config = TempSecretFile::create(&format!(
                "password: '{}'\n",
                c.password.replace('\'', "''")
            ))
            .await?;
            let mut args = vec![
                s("--config"),
                config.0.to_string_lossy().into_owned(),
                s("--archive"),
                s("--host"),
                s(&c.host),
                s("--port"),
                port,
                s("--username"),
                s(&c.username),
                s("--authenticationDatabase"),
                s("admin"),
            ];
            if restore {
                args.extend([s("--drop"), s("--nsInclude"), format!("{}.*", c.database)]);
            } else {
                args.extend([s("--db"), s(&c.database)]);
            }
            ToolCommand {
                program: if restore { "mongorestore" } else { "mongodump" },
                args,
                env: vec![],
                _config: Some(config),
            }
        }
    })
}

/// Spawns the tool. Each program is a literal here so the allow-list scan
/// in `exec.rs` can check it.
fn spawn_tool(tool: &ToolCommand, pipe_stdin: bool) -> CoreResult<Child> {
    let (args, env) = tool.refs();
    match tool.program {
        "pg_dump" => spawn_piped("pg_dump", &args, &env, pipe_stdin),
        "psql" => spawn_piped("psql", &args, &env, pipe_stdin),
        "mysqldump" => spawn_piped("mysqldump", &args, &env, pipe_stdin),
        "mysql" => spawn_piped("mysql", &args, &env, pipe_stdin),
        "mongodump" => spawn_piped("mongodump", &args, &env, pipe_stdin),
        "mongorestore" => spawn_piped("mongorestore", &args, &env, pipe_stdin),
        other => Err(CoreError::Validation(format!("unknown tool {other}"))),
    }
}

fn tool_error(what: &str, stderr: &str, secrets: &[&str]) -> CoreError {
    CoreError::External(redact(&format!("{what} failed: {stderr}"), secrets))
}

async fn finish_backup(
    client: &S3Client,
    key: &str,
    uploaded: CoreResult<u64>,
    status: CoreResult<bool>,
    stderr: String,
    what: &str,
    secrets: &[&str],
) -> CoreResult<u64> {
    let failure = match (&uploaded, &status) {
        (_, Ok(false)) => Some(tool_error(what, &stderr, secrets)),
        (_, Err(e)) => Some(CoreError::Io(format!("waiting for {what}: {e}"))),
        (Err(e), _) => Some(CoreError::External(redact(&e.to_string(), secrets))),
        _ => None,
    };
    match failure {
        None => Ok(uploaded?),
        Some(error) => {
            // A truncated dump must never look like a good backup.
            let _ = client.delete(key).await;
            Err(error)
        }
    }
}

pub async fn test_target(params: TestS3Params) -> CoreResult<()> {
    ensure_target(&params.target)?;
    S3Client::new(&params.target)?.check().await
}

pub async fn list(params: ListBackupsParams) -> CoreResult<Vec<BackupObject>> {
    ensure_target(&params.target)?;
    ensure_path(&params.prefix, "prefix")?;
    S3Client::new(&params.target)?
        .list(&format!("{}/", params.prefix))
        .await
}

pub async fn backup_database(params: BackupDatabaseParams) -> CoreResult<BackupOutcome> {
    ensure_target(&params.target)?;
    ensure_path(&params.prefix, "prefix")?;
    ensure_connection(&params.connection)?;
    let c = &params.connection;
    let client = S3Client::new(&params.target)?;
    let name = format!("{}-{}", c.engine.as_str(), c.database);
    let key = object_key(&params.prefix, &name, c.engine.extension());
    let secrets = [
        c.password.as_str(),
        params.target.secret_key.as_str(),
        params.target.access_key.as_str(),
    ];

    let tool = tool_command(c, false).await?;
    let mut child = spawn_tool(&tool, false)?;
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let (uploaded, stderr_text) = tokio::join!(
        upload_compressed(&client, &key, stdout),
        read_stderr(stderr)
    );
    let status = child
        .wait()
        .await
        .map(|st| st.success())
        .map_err(|e| CoreError::Io(e.to_string()));
    let size = finish_backup(
        &client,
        &key,
        uploaded,
        status,
        stderr_text,
        tool.program,
        &secrets,
    )
    .await?;
    let pruned = prune(&client, &params.prefix, &name, params.retention_count).await?;
    Ok(BackupOutcome {
        key,
        size_bytes: size,
        pruned,
    })
}

pub async fn restore_database(params: RestoreDatabaseParams) -> CoreResult<()> {
    ensure_target(&params.target)?;
    ensure_path(&params.key, "key")?;
    ensure_connection(&params.connection)?;
    let c = &params.connection;
    let client = S3Client::new(&params.target)?;
    let secrets = [
        c.password.as_str(),
        params.target.secret_key.as_str(),
        params.target.access_key.as_str(),
    ];
    let tool = tool_command(c, true).await?;
    let mut child = spawn_tool(&tool, true)?;
    let mut stdin = child.stdin.take().expect("piped stdin");
    let stderr = child.stderr.take().expect("piped stderr");
    let (piped, stderr_text) = tokio::join!(
        async {
            let r = download_decompressed(&client, &params.key, &mut stdin).await;
            drop(stdin);
            r
        },
        read_stderr(stderr)
    );
    let status = child
        .wait()
        .await
        .map_err(|e| CoreError::Io(format!("waiting for {}: {e}", tool.program)))?;
    if !status.success() {
        return Err(tool_error(tool.program, &stderr_text, &secrets));
    }
    piped.map_err(|e| CoreError::External(redact(&e.to_string(), &secrets)))
}

/// Backs up one Docker named volume of a Compose project.
#[cfg(feature = "docker")]
pub async fn backup_volume(
    cfg: &crate::config::AgentConfig,
    params: BackupVolumeParams,
) -> CoreResult<BackupOutcome> {
    ensure_target(&params.target)?;
    ensure_path(&params.prefix, "prefix")?;
    let mount = volume_mountpoint(cfg, &params.project_slug, &params.volume)?;
    let service = ComposeService::to_stop(
        cfg,
        &params.project_slug,
        &params.service,
        params.stop_service,
    )?;
    let client = S3Client::new(&params.target)?;
    let name = format!("volume-{}-{}", params.project_slug, params.volume);
    let key = object_key(&params.prefix, &name, "tar.gz");
    let secrets = [
        params.target.secret_key.as_str(),
        params.target.access_key.as_str(),
    ];

    let result = with_service_stopped(
        service,
        "volume backup",
        preflight_upload(&client, &params.prefix, &secrets),
        async {
            let mount = mount.to_string_lossy().into_owned();
            let mut child = spawn_piped("tar", &["-C", &mount, "-cf", "-", "."], &[], false)?;
            let stdout = child.stdout.take().expect("piped stdout");
            let stderr = child.stderr.take().expect("piped stderr");
            let (uploaded, stderr_text) = tokio::join!(
                upload_compressed(&client, &key, stdout),
                read_stderr(stderr)
            );
            let status = child
                .wait()
                .await
                .map(|st| st.success())
                .map_err(|e| CoreError::Io(e.to_string()));
            finish_backup(
                &client,
                &key,
                uploaded,
                status,
                stderr_text,
                "tar",
                &secrets,
            )
            .await
        },
    )
    .await?;
    let pruned = prune(&client, &params.prefix, &name, params.retention_count).await?;
    Ok(BackupOutcome {
        key,
        size_bytes: result,
        pruned,
    })
}

/// Restores a volume backup over the volume's current contents.
#[cfg(feature = "docker")]
pub async fn restore_volume(
    cfg: &crate::config::AgentConfig,
    params: RestoreVolumeParams,
) -> CoreResult<()> {
    ensure_target(&params.target)?;
    ensure_path(&params.key, "key")?;
    let mount = volume_mountpoint(cfg, &params.project_slug, &params.volume)?;
    let service = ComposeService::to_stop(
        cfg,
        &params.project_slug,
        &params.service,
        params.stop_service,
    )?;
    let client = S3Client::new(&params.target)?;
    let secrets = [
        params.target.secret_key.as_str(),
        params.target.access_key.as_str(),
    ];
    with_service_stopped(
        service,
        "volume restore",
        preflight_download(&client, &params.key, &secrets),
        async {
            let mount = mount.to_string_lossy().into_owned();
            let mut child = spawn_piped("tar", &["-C", &mount, "-xpf", "-"], &[], true)?;
            let mut stdin = child.stdin.take().expect("piped stdin");
            let stderr = child.stderr.take().expect("piped stderr");
            let (piped, stderr_text) = tokio::join!(
                async {
                    let r = download_decompressed(&client, &params.key, &mut stdin).await;
                    drop(stdin);
                    r
                },
                read_stderr(stderr)
            );
            let status = child
                .wait()
                .await
                .map_err(|e| CoreError::Io(format!("waiting for tar: {e}")))?;
            if !status.success() {
                return Err(tool_error("tar", &stderr_text, &secrets));
            }
            piped.map_err(|e| CoreError::External(redact(&e.to_string(), &secrets)))
        },
    )
    .await
}

#[cfg(feature = "docker")]
fn volume_mountpoint(
    cfg: &crate::config::AgentConfig,
    slug: &str,
    volume: &str,
) -> CoreResult<std::path::PathBuf> {
    if !validate::is_valid_project_slug(slug) || !validate::is_valid_volume_name(volume) {
        return Err(CoreError::Validation(
            "invalid project or volume name".into(),
        ));
    }
    // Compose names a project volume `<project>_<volume>`.
    let mount = cfg
        .docker_volumes_root
        .join(format!("{slug}_{volume}"))
        .join("_data");
    if !mount.is_dir() {
        return Err(CoreError::NotFound(format!(
            "volume {volume} of project {slug} does not exist on this host"
        )));
    }
    Ok(mount)
}

/// Proves, before a service is stopped for a backup, that the destination
/// is reachable, accepts the credentials, and takes a write under `prefix`:
/// a zero-byte probe object is written and deleted again.
#[cfg(feature = "docker")]
async fn preflight_upload(client: &S3Client, prefix: &str, secrets: &[&str]) -> CoreResult<()> {
    let probe = format!("{prefix}/.scontrol-preflight-{}", uuid::Uuid::new_v4());
    let checked = async {
        client.check().await?;
        client.put_object(&probe, Vec::new()).await
    };
    checked.await.map_err(|e| preflight_error(&e, secrets))?;
    // Retention and listings never match the probe name, so a probe left
    // behind by a missing delete permission is harmless.
    let _ = client.delete(&probe).await;
    Ok(())
}

/// Proves, before a service is stopped for a restore, that the destination
/// is reachable with these credentials and that the backup `key` exists.
#[cfg(feature = "docker")]
async fn preflight_download(client: &S3Client, key: &str, secrets: &[&str]) -> CoreResult<()> {
    let listing = client
        .list(key)
        .await
        .map_err(|e| preflight_error(&e, secrets))?;
    if listing.iter().any(|object| object.key == key) {
        Ok(())
    } else {
        Err(CoreError::NotFound(format!(
            "backup {key} does not exist; the service was not stopped"
        )))
    }
}

#[cfg(feature = "docker")]
fn preflight_error(error: &CoreError, secrets: &[&str]) -> CoreError {
    CoreError::External(redact(
        &format!(
            "S3 destination check failed, the service was not stopped: {}",
            error.message()
        ),
        secrets,
    ))
}

/// The one service a volume backup or restore stops and starts again.
#[cfg(feature = "docker")]
#[async_trait::async_trait]
trait ServiceControl: Send + Sync + 'static {
    fn name(&self) -> &str;
    async fn stop(&self) -> CoreResult<()>;
    async fn start(&self) -> CoreResult<()>;
}

#[cfg(feature = "docker")]
struct ComposeService {
    cfg: crate::config::AgentConfig,
    slug: String,
    service: String,
}

#[cfg(feature = "docker")]
impl ComposeService {
    /// The service to stop around the operation, if one should be.
    fn to_stop(
        cfg: &crate::config::AgentConfig,
        slug: &str,
        service: &Option<String>,
        stop: bool,
    ) -> CoreResult<Option<Self>> {
        match (stop, service) {
            (false, _) => Ok(None),
            (true, None) => Err(CoreError::Validation(
                "stopping the container needs a service name".into(),
            )),
            (true, Some(service)) if !validate::is_valid_service_name(service) => {
                Err(CoreError::Validation("invalid service name".into()))
            }
            (true, Some(service)) => Ok(Some(Self {
                cfg: cfg.clone(),
                slug: slug.to_string(),
                service: service.clone(),
            })),
        }
    }
}

#[cfg(feature = "docker")]
#[async_trait::async_trait]
impl ServiceControl for ComposeService {
    fn name(&self) -> &str {
        &self.service
    }

    async fn stop(&self) -> CoreResult<()> {
        super::compose::service_action(&self.cfg, &self.slug, "stop", &self.service).await
    }

    async fn start(&self) -> CoreResult<()> {
        super::compose::service_action(&self.cfg, &self.slug, "start", &self.service).await
    }
}

/// Starts a stopped service again when the operation around it ends without
/// doing so itself: its future was dropped (cancelled, timed out) or it
/// panicked. A drop cannot await, so the start runs as a spawned task.
#[cfg(feature = "docker")]
struct RestartGuard<S: ServiceControl> {
    service: Option<std::sync::Arc<S>>,
}

#[cfg(feature = "docker")]
impl<S: ServiceControl> RestartGuard<S> {
    fn disarm(mut self) {
        self.service = None;
    }
}

#[cfg(feature = "docker")]
impl<S: ServiceControl> Drop for RestartGuard<S> {
    fn drop(&mut self) {
        let Some(service) = self.service.take() else {
            return;
        };
        let name = service.name().to_string();
        tracing::warn!(service = %name, "volume backup/restore interrupted; starting the service again");
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                runtime.spawn(async move {
                    if let Err(error) = service.start().await {
                        tracing::error!(service = %service.name(), %error, "starting the service again failed");
                    }
                });
            }
            Err(_) => {
                tracing::error!(service = %name, "no runtime to start the service again; it stays stopped")
            }
        }
    }
}

/// Runs `work`, with `service` stopped around it when there is one.
///
/// Before anything is stopped, `preflight` must pass, so an unreachable or
/// misconfigured destination never costs downtime. Once the service is
/// stopped it is started again on every path: after `work` (or a failed
/// stop) returns, and through [`RestartGuard`] if this future is dropped or
/// panics first. A failed start is never swallowed: with a failed `work`,
/// both errors are reported together.
#[cfg(feature = "docker")]
async fn with_service_stopped<S: ServiceControl, T>(
    service: Option<S>,
    what: &str,
    preflight: impl std::future::Future<Output = CoreResult<()>>,
    work: impl std::future::Future<Output = CoreResult<T>>,
) -> CoreResult<T> {
    let Some(service) = service else {
        return work.await;
    };
    preflight.await?;
    let service = std::sync::Arc::new(service);
    // Armed before `stop`: a stop that fails half-way may still have
    // stopped the container.
    let guard = RestartGuard {
        service: Some(service.clone()),
    };
    let result = match service.stop().await {
        Ok(()) => work.await,
        Err(error) => Err(error),
    };
    let restarted = service.start().await;
    guard.disarm();
    with_restart_error(result, restarted, what, service.name())
}

#[cfg(feature = "docker")]
fn with_restart_error<T>(
    result: CoreResult<T>,
    restarted: CoreResult<()>,
    what: &str,
    service: &str,
) -> CoreResult<T> {
    match (result, restarted) {
        (result, Ok(())) => result,
        (Ok(_), Err(restart)) => Err(CoreError::External(format!(
            "{what} finished, but starting service {service} again failed: {}",
            restart.message()
        ))),
        (Err(failure), Err(restart)) => {
            let message = format!(
                "{what} failed: {}; additionally, starting service {service} again failed: {}",
                failure.message(),
                restart.message()
            );
            Err(match failure {
                CoreError::Validation(_) => CoreError::Validation(message),
                CoreError::NotFound(_) => CoreError::NotFound(message),
                CoreError::AlreadyExists(_) => CoreError::AlreadyExists(message),
                CoreError::PermissionDenied(_) => CoreError::PermissionDenied(message),
                CoreError::Io(_) => CoreError::Io(message),
                CoreError::External(_) => CoreError::External(message),
                CoreError::Cancelled(_) => CoreError::Cancelled(message),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::s3::tests::{start_mock_s3, target};
    use super::*;
    use std::io::Write;

    #[test]
    fn builds_sorted_keys_and_tool_commands_without_secrets_on_argv() {
        let key = object_key("scontrol/db", "postgres-app", "sql.gz");
        assert!(key.starts_with("scontrol/db/postgres-app-20") && key.ends_with("Z.sql.gz"));

        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        for engine in [
            BackupEngine::Postgres,
            BackupEngine::Mysql,
            BackupEngine::Mongodb,
        ] {
            for restore in [false, true] {
                let conn = DatabaseConnection {
                    engine,
                    host: "db.internal".into(),
                    port: 5432,
                    database: "app".into(),
                    username: "appuser".into(),
                    password: "hunter2-secret".into(),
                };
                let tool = rt.block_on(tool_command(&conn, restore)).unwrap();
                assert!(
                    tool.args.iter().all(|a| !a.contains("hunter2-secret")),
                    "{engine:?} puts the password on argv"
                );
                assert!(
                    !tool.args.iter().any(|a| a.starts_with("postgres://")),
                    "no connection URIs"
                );
                match engine {
                    BackupEngine::Mongodb => {
                        let config = tool._config.as_ref().unwrap();
                        let body = std::fs::read_to_string(&config.0).unwrap();
                        assert!(body.contains("hunter2-secret"));
                        use std::os::unix::fs::PermissionsExt;
                        let mode = std::fs::metadata(&config.0).unwrap().permissions().mode();
                        assert_eq!(mode & 0o777, 0o600);
                    }
                    _ => assert!(tool.env.iter().any(|(_, v)| v == "hunter2-secret")),
                }
            }
        }
    }

    #[test]
    fn config_file_is_removed_after_use() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let path = {
            let file = rt
                .block_on(TempSecretFile::create("password: 'x'\n"))
                .unwrap();
            let path = file.0.clone();
            assert!(path.exists());
            path
        };
        assert!(!path.exists());
    }

    #[test]
    fn rejects_unsafe_inputs_before_spawning() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let mut conn = DatabaseConnection {
            engine: BackupEngine::Postgres,
            host: "-oProxyCommand=x".into(),
            port: 5432,
            database: "app".into(),
            username: "u".into(),
            password: "p".into(),
        };
        assert!(ensure_connection(&conn).is_err());
        conn.host = "db".into();
        conn.database = "a;b".into();
        assert!(ensure_connection(&conn).is_err());
        let params = BackupDatabaseParams {
            target: target("http://127.0.0.1:1"),
            prefix: "../escape".into(),
            connection: conn,
            retention_count: 1,
        };
        assert!(matches!(
            rt.block_on(backup_database(params)),
            Err(CoreError::Validation(_))
        ));
    }

    /// The whole streaming path, without any dump tool: gzip, multipart
    /// upload of more than one part, retention, download, gunzip.
    #[tokio::test]
    async fn streams_gzip_to_s3_then_restores_and_prunes() {
        let (endpoint, mock) = start_mock_s3().await;
        let client = S3Client::new(&target(&endpoint)).unwrap();

        // Incompressible-ish data so the compressed stream spans 2+ parts.
        let mut data = Vec::new();
        let mut x: u64 = 0x9E3779B97F4A7C15;
        while data.len() < 20 * 1024 * 1024 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            data.extend_from_slice(&x.to_le_bytes());
        }
        let key = "pfx/postgres-app-20260101T000000Z.sql.gz";
        let size = upload_compressed(&client, key, &data[..]).await.unwrap();
        assert!(size > 16 * 1024 * 1024);
        assert_eq!(*mock.multipart_completed.lock().unwrap(), 1);
        let stored = mock.objects.lock().unwrap()[key].clone();
        assert_eq!(&stored[..2], &[0x1f, 0x8b], "gzip magic");
        assert_eq!(stored.len() as u64, size);

        let mut restored = Vec::new();
        download_decompressed(&client, key, &mut restored)
            .await
            .unwrap();
        assert_eq!(restored, data);

        // Corrupt data is an error, not silent garbage.
        client
            .put_object("pfx/bad.gz", b"not gzip at all".to_vec())
            .await
            .unwrap();
        let mut sink = Vec::new();
        assert!(download_decompressed(&client, "pfx/bad.gz", &mut sink)
            .await
            .is_err());

        // Retention keeps the newest N of the same database only.
        for ts in ["20260102", "20260103", "20260104"] {
            let mut tiny: &[u8] = b"x";
            client
                .upload_stream(&format!("pfx/postgres-app-{ts}T000000Z.sql.gz"), &mut tiny)
                .await
                .unwrap();
        }
        client
            .put_object("pfx/postgres-other-20250101T000000Z.sql.gz", b"o".to_vec())
            .await
            .unwrap();
        assert_eq!(prune(&client, "pfx", "postgres-app", 2).await.unwrap(), 2);
        let left: Vec<String> = client
            .list("pfx/")
            .await
            .unwrap()
            .into_iter()
            .map(|o| o.key)
            .collect();
        assert!(left.contains(&"pfx/postgres-app-20260104T000000Z.sql.gz".to_string()));
        assert!(left.contains(&"pfx/postgres-app-20260103T000000Z.sql.gz".to_string()));
        assert!(!left.contains(&key.to_string()));
        assert!(left.contains(&"pfx/postgres-other-20250101T000000Z.sql.gz".to_string()));
        assert_eq!(prune(&client, "pfx", "postgres-app", 0).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn failed_dump_deletes_the_partial_object_and_redacts_stderr() {
        let (endpoint, mock) = start_mock_s3().await;
        let client = S3Client::new(&target(&endpoint)).unwrap();
        let key = "pfx/partial.sql.gz";
        let uploaded = upload_compressed(&client, key, &b"partial dump"[..]).await;
        assert!(mock.objects.lock().unwrap().contains_key(key));
        let secrets = ["hunter2-secret"];
        let err = finish_backup(
            &client,
            key,
            uploaded,
            Ok(false),
            "FATAL password hunter2-secret rejected".into(),
            "pg_dump",
            &secrets,
        )
        .await
        .unwrap_err();
        assert!(!mock.objects.lock().unwrap().contains_key(key));
        let text = err.to_string();
        assert!(text.contains("pg_dump failed") && text.contains("********"));
        assert!(!text.contains("hunter2-secret"));
        let _ = std::io::stdout().flush();
    }

    #[cfg(feature = "docker")]
    mod service_lifecycle {
        use super::*;
        use std::sync::{Arc, Mutex};
        use std::time::Duration;

        /// Records every stop/start; either can be made to fail.
        #[derive(Clone, Default)]
        struct FakeService {
            calls: Arc<Mutex<Vec<&'static str>>>,
            fail_stop: bool,
            fail_start: bool,
        }

        impl FakeService {
            fn calls(&self) -> Vec<&'static str> {
                self.calls.lock().unwrap().clone()
            }
        }

        #[async_trait::async_trait]
        impl ServiceControl for FakeService {
            fn name(&self) -> &str {
                "web"
            }
            async fn stop(&self) -> CoreResult<()> {
                self.calls.lock().unwrap().push("stop");
                if self.fail_stop {
                    return Err(CoreError::External("docker compose stop web failed".into()));
                }
                Ok(())
            }
            async fn start(&self) -> CoreResult<()> {
                self.calls.lock().unwrap().push("start");
                if self.fail_start {
                    return Err(CoreError::External(
                        "docker compose start web failed (exit code 1)".into(),
                    ));
                }
                Ok(())
            }
        }

        async fn ok() -> CoreResult<()> {
            Ok(())
        }

        /// Waits (bounded) until the spawned restart of a [`RestartGuard`] ran.
        async fn wait_for_start(service: &FakeService) {
            for _ in 0..200 {
                if service.calls().contains(&"start") {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            panic!("service was never started again: {:?}", service.calls());
        }

        #[tokio::test]
        async fn stops_runs_and_starts_in_order() {
            let service = FakeService::default();
            let calls = service.calls.clone();
            let out = with_service_stopped(Some(service.clone()), "volume backup", ok(), async {
                calls.lock().unwrap().push("work");
                Ok(42)
            })
            .await
            .unwrap();
            assert_eq!(out, 42);
            assert_eq!(service.calls(), ["stop", "work", "start"]);
        }

        #[tokio::test]
        async fn without_a_service_nothing_is_stopped_or_preflighted() {
            let out = with_service_stopped(
                None::<FakeService>,
                "volume backup",
                async { panic!("preflight must not run without a stop") },
                async { Ok("done") },
            )
            .await
            .unwrap();
            assert_eq!(out, "done");
        }

        #[tokio::test]
        async fn failed_preflight_never_stops_the_service() {
            let service = FakeService::default();
            let err = with_service_stopped(
                Some(service.clone()),
                "volume backup",
                async { Err(CoreError::External("S3 unreachable".into())) },
                async { panic!("work must not run after a failed preflight") as CoreResult<()> },
            )
            .await
            .unwrap_err();
            assert!(err.message().contains("S3 unreachable"));
            assert!(service.calls().is_empty(), "{:?}", service.calls());
        }

        #[tokio::test]
        async fn tar_failure_still_restarts_and_keeps_the_original_error() {
            let service = FakeService::default();
            let err = with_service_stopped(Some(service.clone()), "volume backup", ok(), async {
                Err::<(), _>(CoreError::External("tar failed: disk error".into()))
            })
            .await
            .unwrap_err();
            assert!(matches!(&err, CoreError::External(m) if m == "tar failed: disk error"));
            assert_eq!(service.calls(), ["stop", "start"]);
        }

        #[tokio::test]
        async fn tar_and_restart_failures_are_reported_together() {
            let service = FakeService {
                fail_start: true,
                ..Default::default()
            };
            let err = with_service_stopped(Some(service.clone()), "volume backup", ok(), async {
                Err::<(), _>(CoreError::Io("tar failed: disk error".into()))
            })
            .await
            .unwrap_err();
            assert!(matches!(err, CoreError::Io(_)), "keeps the work error kind");
            let text = err.message();
            assert!(
                text.contains("volume backup failed: tar failed: disk error"),
                "{text}"
            );
            assert!(
                text.contains("additionally, starting service web again failed: docker compose start web failed"),
                "{text}"
            );
            assert_eq!(service.calls(), ["stop", "start"]);
        }

        #[tokio::test]
        async fn restart_failure_after_successful_work_is_an_error() {
            let service = FakeService {
                fail_start: true,
                ..Default::default()
            };
            let err = with_service_stopped(Some(service.clone()), "volume restore", ok(), ok())
                .await
                .unwrap_err();
            assert!(err
                .message()
                .contains("volume restore finished, but starting service web again failed"));
        }

        #[tokio::test]
        async fn failed_stop_skips_work_and_still_tries_to_start() {
            let service = FakeService {
                fail_stop: true,
                ..Default::default()
            };
            let err = with_service_stopped(Some(service.clone()), "volume backup", ok(), async {
                panic!("work must not run after a failed stop") as CoreResult<()>
            })
            .await
            .unwrap_err();
            assert!(err.message().contains("docker compose stop web failed"));
            assert_eq!(service.calls(), ["stop", "start"]);
        }

        #[tokio::test]
        async fn cancelled_operation_starts_the_service_again() {
            let service = FakeService::default();
            let timed_out = tokio::time::timeout(
                Duration::from_millis(50),
                with_service_stopped(
                    Some(service.clone()),
                    "volume backup",
                    ok(),
                    std::future::pending::<CoreResult<()>>(),
                ),
            )
            .await;
            assert!(timed_out.is_err(), "the work never finishes");
            wait_for_start(&service).await;
            assert_eq!(service.calls(), ["stop", "start"]);
        }

        #[tokio::test]
        async fn panicking_operation_starts_the_service_again() {
            let service = FakeService::default();
            let task = tokio::spawn(with_service_stopped(
                Some(service.clone()),
                "volume restore",
                ok(),
                async { panic!("tar pipeline bug") as CoreResult<()> },
            ));
            assert!(task.await.unwrap_err().is_panic());
            wait_for_start(&service).await;
            assert_eq!(service.calls(), ["stop", "start"]);
        }

        #[tokio::test]
        async fn completed_operation_does_not_start_the_service_twice() {
            let service = FakeService::default();
            with_service_stopped(Some(service.clone()), "volume backup", ok(), ok())
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(20)).await;
            assert_eq!(service.calls(), ["stop", "start"]);
        }

        #[test]
        fn stopping_needs_a_valid_service_name() {
            let cfg = crate::config::AgentConfig::default();
            assert!(ComposeService::to_stop(&cfg, "app", &None, false)
                .unwrap()
                .is_none());
            assert!(matches!(
                ComposeService::to_stop(&cfg, "app", &None, true),
                Err(CoreError::Validation(_))
            ));
            assert!(matches!(
                ComposeService::to_stop(&cfg, "app", &Some("web;rm".into()), true),
                Err(CoreError::Validation(_))
            ));
            let service = ComposeService::to_stop(&cfg, "app", &Some("web".into()), true)
                .unwrap()
                .unwrap();
            assert_eq!(service.name(), "web");
        }

        #[tokio::test]
        async fn upload_preflight_probes_a_write_and_cleans_up() {
            let (endpoint, mock) = start_mock_s3().await;
            let client = S3Client::new(&target(&endpoint)).unwrap();
            preflight_upload(&client, "pfx", &[]).await.unwrap();
            assert!(mock.objects.lock().unwrap().is_empty(), "probe deleted");
            assert!(prune(&client, "pfx", "volume-app-data", 1).await.is_ok());
        }

        #[tokio::test]
        async fn download_preflight_needs_the_exact_key() {
            let (endpoint, _mock) = start_mock_s3().await;
            let client = S3Client::new(&target(&endpoint)).unwrap();
            let key = "pfx/volume-app-data-20260101T000000Z.tar.gz";
            client.put_object(key, b"x".to_vec()).await.unwrap();
            preflight_download(&client, key, &[]).await.unwrap();
            // A prefix match of a longer key is not the backup.
            let err = preflight_download(&client, "pfx/volume-app-data-2026", &[])
                .await
                .unwrap_err();
            assert!(matches!(err, CoreError::NotFound(_)));
            assert!(err.message().contains("service was not stopped"));
        }

        #[tokio::test]
        async fn unreachable_destination_fails_preflight_with_redacted_error() {
            let t = target("http://127.0.0.1:1");
            let client = S3Client::new(&t).unwrap();
            let secrets = [t.secret_key.as_str(), t.access_key.as_str()];
            for err in [
                preflight_upload(&client, "pfx", &secrets)
                    .await
                    .unwrap_err(),
                preflight_download(&client, "pfx/k.tar.gz", &secrets)
                    .await
                    .unwrap_err(),
            ] {
                let text = err.message();
                assert!(text.contains("the service was not stopped"), "{text}");
                assert!(!text.contains(&t.secret_key) && !text.contains(&t.access_key));
            }
        }
    }
}
