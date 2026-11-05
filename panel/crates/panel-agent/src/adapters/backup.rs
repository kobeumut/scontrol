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

/// Deletes the oldest backups of the series `<prefix>/<name>-<timestamp>.<extension>`
/// beyond `keep`. Only exact series members count: the listing prefix also
/// matches longer names (`<name>-staging-…`), which belong to someone else.
pub async fn prune(
    client: &S3Client,
    prefix: &str,
    name: &str,
    extension: &str,
    keep: u32,
) -> CoreResult<u32> {
    if keep == 0 {
        return Ok(0);
    }
    let listing: Vec<_> = client
        .list(&format!("{prefix}/{name}-"))
        .await?
        .into_iter()
        .filter(|o| is_backup_of_series(&o.key, prefix, name, extension))
        .collect();
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
                s("--events"),
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
    let name = params
        .series_name()
        .ok_or_else(|| CoreError::Validation("invalid backup namespace".into()))?;
    let key = object_key(&params.prefix, &name, c.engine.extension());
    ensure_path(&key, "key")?;
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
    let pruned = prune(
        &client,
        &params.prefix,
        &name,
        c.engine.extension(),
        params.retention_count,
    )
    .await?;
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
    let client = S3Client::new(&params.target)?;
    let name = format!("volume-{}-{}", params.project_slug, params.volume);
    let key = object_key(&params.prefix, &name, "tar.gz");
    let secrets = [
        params.target.secret_key.as_str(),
        params.target.access_key.as_str(),
    ];

    let result = with_service_stopped(
        cfg,
        &params.project_slug,
        &params.service,
        params.stop_service,
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
    let pruned = prune(
        &client,
        &params.prefix,
        &name,
        "tar.gz",
        params.retention_count,
    )
    .await?;
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
    let client = S3Client::new(&params.target)?;
    let secrets = [
        params.target.secret_key.as_str(),
        params.target.access_key.as_str(),
    ];
    with_service_stopped(
        cfg,
        &params.project_slug,
        &params.service,
        params.stop_service,
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

/// Runs `work`, with the service stopped around it when asked. The service
/// is started again even when `work` fails; a failed restart is reported
/// only if `work` itself succeeded.
#[cfg(feature = "docker")]
async fn with_service_stopped<T>(
    cfg: &crate::config::AgentConfig,
    slug: &str,
    service: &Option<String>,
    stop: bool,
    work: impl std::future::Future<Output = CoreResult<T>>,
) -> CoreResult<T> {
    use super::compose::service_action;
    let service = match (stop, service) {
        (true, Some(service)) => Some(service.as_str()),
        (true, None) => {
            return Err(CoreError::Validation(
                "stopping the container needs a service name".into(),
            ))
        }
        _ => None,
    };
    if let Some(service) = service {
        service_action(cfg, slug, "stop", service).await?;
    }
    let result = work.await;
    if let Some(service) = service {
        let restarted = service_action(cfg, slug, "start", service).await;
        if result.is_ok() {
            restarted?;
        }
    }
    result
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
                if matches!(engine, BackupEngine::Mysql) && !restore {
                    for flag in ["--single-transaction", "--routines", "--events"] {
                        assert!(tool.args.iter().any(|a| a == flag), "mysqldump {flag}");
                    }
                }
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
        let mut params = BackupDatabaseParams {
            target: target("http://127.0.0.1:1"),
            prefix: "../escape".into(),
            connection: conn,
            retention_count: 1,
            namespace: None,
        };
        assert!(matches!(
            rt.block_on(backup_database(params.clone())),
            Err(CoreError::Validation(_))
        ));
        // A namespace cannot climb out of the destination prefix.
        params.prefix = "pfx".into();
        params.connection.database = "app".into();
        params.namespace = Some("../other".into());
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
        assert_eq!(
            prune(&client, "pfx", "postgres-app", "sql.gz", 2)
                .await
                .unwrap(),
            2
        );
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
        assert_eq!(
            prune(&client, "pfx", "postgres-app", "sql.gz", 0)
                .await
                .unwrap(),
            0
        );
    }

    /// Two services each with a database `app` (and a service whose slug
    /// extends the other's) keep separate series: retention of one never
    /// deletes another's backups.
    #[tokio::test]
    async fn retention_never_crosses_services() {
        let (endpoint, _mock) = start_mock_s3().await;
        let client = S3Client::new(&target(&endpoint)).unwrap();
        let conn = |database: &str| DatabaseConnection {
            engine: BackupEngine::Postgres,
            host: "127.0.0.1".into(),
            port: 5432,
            database: database.into(),
            username: "u".into(),
            password: "p".into(),
        };
        let series = |slug: &str, database: &str| {
            BackupDatabaseParams {
                target: target(&endpoint),
                prefix: "pfx".into(),
                connection: conn(database),
                retention_count: 1,
                namespace: Some(service_backup_namespace(slug)),
            }
            .series_name()
            .unwrap()
        };
        let shop = series("shop", "app");
        let blog = series("blog", "app");
        let shop_app = series("shop-app", "main");
        assert_ne!(shop, blog);

        let mut keys = Vec::new();
        for name in [&shop, &blog, &shop_app] {
            for day in ["01", "02", "03"] {
                keys.push(format!("pfx/{name}-202601{day}T000000Z.sql.gz"));
            }
        }
        // A backup from before namespacing, and volume series that share
        // a listing prefix.
        keys.push("pfx/postgres-app-20250101T000000Z.sql.gz".into());
        for day in ["01", "02"] {
            keys.push(format!("pfx/volume-shop-data-202601{day}T000000Z.tar.gz"));
            keys.push(format!("pfx/volume-shop-data-x-202601{day}T000000Z.tar.gz"));
        }
        for key in &keys {
            client.put_object(key, b"x".to_vec()).await.unwrap();
        }

        // The shop service keeps 1: only its own two oldest go.
        assert_eq!(prune(&client, "pfx", &shop, "sql.gz", 1).await.unwrap(), 2);
        assert_eq!(
            prune(&client, "pfx", "volume-shop-data", "tar.gz", 1)
                .await
                .unwrap(),
            1
        );
        let left: Vec<String> = client
            .list("pfx/")
            .await
            .unwrap()
            .into_iter()
            .map(|o| o.key)
            .collect();
        let gone: Vec<&String> = keys.iter().filter(|k| !left.contains(k)).collect();
        assert_eq!(
            gone,
            vec![
                &format!("pfx/{shop}-20260101T000000Z.sql.gz"),
                &format!("pfx/{shop}-20260102T000000Z.sql.gz"),
                &"pfx/volume-shop-data-20260101T000000Z.tar.gz".to_string(),
            ]
        );
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
}
