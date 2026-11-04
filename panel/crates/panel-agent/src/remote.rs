//! The remote listener: the agent RPC over mutual TLS 1.3 (see
//! `panel_core::pki`). Only a client certificate from the panel CA with the
//! `clientAuth` usage gets through the handshake; everything after it is the
//! same line protocol the Unix socket serves.

use crate::config::RemoteConfig;
use panel_core::pki::{self, KeyedCert};
use rustls::ServerConfig;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

/// Connections served at once; more wait for a slot.
const MAX_CONNECTIONS: usize = 64;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// A remote connection idle this long is closed; the panel reuses idle
/// connections for at most a minute.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(300);

/// Reloads the TLS identity when the certificate or CA file changes, so a
/// rotated certificate takes effect without a restart.
pub struct TlsIdentity {
    config: RemoteConfig,
    cached: Mutex<Option<Loaded>>,
}

/// A loaded configuration and the file modification times it came from.
type Loaded = (Vec<Option<SystemTime>>, Arc<ServerConfig>);

impl TlsIdentity {
    pub fn new(config: RemoteConfig) -> Self {
        Self {
            config,
            cached: Mutex::new(None),
        }
    }

    fn stamps(&self) -> Vec<Option<SystemTime>> {
        [&self.config.ca_cert, &self.config.cert, &self.config.key]
            .iter()
            .map(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok())
            .collect()
    }

    pub fn current(&self) -> anyhow::Result<Arc<ServerConfig>> {
        let stamps = self.stamps();
        let mut cached = self.cached.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((seen, config)) = cached.as_ref() {
            if *seen == stamps {
                return Ok(config.clone());
            }
        }
        check_key_file(&self.config.key)?;
        let ca_pem = std::fs::read_to_string(&self.config.ca_cert)?;
        let identity = KeyedCert {
            cert_pem: std::fs::read_to_string(&self.config.cert)?,
            key_pem: std::fs::read_to_string(&self.config.key)?,
        };
        let config = pki::server_config(&ca_pem, &identity)?;
        *cached = Some((stamps, config.clone()));
        Ok(config)
    }
}

/// The key must be a regular file (not a symlink) that neither group nor
/// others can read.
fn check_key_file(path: &Path) -> anyhow::Result<()> {
    let meta = std::fs::symlink_metadata(path)
        .map_err(|e| anyhow::anyhow!("reading {}: {e}", path.display()))?;
    if !meta.file_type().is_file() {
        anyhow::bail!("{} must be a regular file", path.display());
    }
    if meta.mode() & 0o077 != 0 {
        anyhow::bail!(
            "{} is readable by group or others; chmod 600 it",
            path.display()
        );
    }
    Ok(())
}

pub async fn serve<F, Fut>(config: RemoteConfig, handle: F) -> anyhow::Result<()>
where
    F: Fn(tokio_rustls::server::TlsStream<tokio::net::TcpStream>) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = anyhow::Result<()>> + Send + 'static,
{
    let identity = Arc::new(TlsIdentity::new(config.clone()));
    // Right after `remote-init` the certificate is not installed yet: wait
    // for it (checking every 30 s) instead of failing, so installing
    // `agent.pem` and `ca.pem` is enough, without a restart.
    let mut reported = false;
    while let Err(error) = identity.current() {
        if !reported {
            tracing::warn!(%error, "remote listener waiting for its certificate files");
            reported = true;
        }
        tokio::time::sleep(Duration::from_secs(30)).await;
    }
    let listener = tokio::net::TcpListener::bind(&config.listen).await?;
    tracing::info!(listen = %config.listen, "remote mTLS listener started");
    let slots = Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS));
    let handle = Arc::new(handle);
    loop {
        let (tcp, peer) = listener.accept().await?;
        let permit = slots.clone().acquire_owned().await?;
        let (identity, handle) = (identity.clone(), handle.clone());
        tokio::spawn(async move {
            let _permit = permit;
            let tls = match identity.current() {
                Ok(tls) => tls,
                Err(error) => {
                    tracing::error!(%error, "remote TLS identity unusable");
                    return;
                }
            };
            tcp.set_nodelay(true).ok();
            let accepted = tokio::time::timeout(
                HANDSHAKE_TIMEOUT,
                tokio_rustls::TlsAcceptor::from(tls).accept(tcp),
            )
            .await;
            let stream = match accepted {
                Ok(Ok(stream)) => stream,
                Ok(Err(error)) => {
                    tracing::warn!(%peer, %error, "remote TLS handshake refused");
                    return;
                }
                Err(_) => {
                    tracing::warn!(%peer, "remote TLS handshake timed out");
                    return;
                }
            };
            if let Err(error) = handle(stream).await {
                tracing::warn!(%peer, %error, "remote connection error");
            }
        });
    }
}

/// `panel-agent remote-init`: writes a new private key (0600) and a CSR for
/// `host` into `dir`, and returns the CSR to paste into the panel. Refuses
/// to replace an existing key unless `force` (a rotation).
pub fn init(host: &str, dir: &Path, force: bool) -> anyhow::Result<String> {
    let request = pki::generate_agent_csr(host)?;
    std::fs::create_dir_all(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    let key_path = dir.join("agent.key");
    let new_key_path: PathBuf = dir.join("agent.key.new");
    let _ = std::fs::remove_file(&new_key_path);
    {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&new_key_path)?;
        file.write_all(request.key_pem.as_bytes())?;
        file.sync_all()?;
    }
    if key_path.exists() && !force {
        std::fs::remove_file(&new_key_path).ok();
        anyhow::bail!(
            "{} exists; pass --force to rotate it (the current certificate stops matching)",
            key_path.display()
        );
    }
    std::fs::rename(&new_key_path, &key_path)?;
    std::fs::write(dir.join("agent.csr"), &request.cert_pem)?;
    Ok(request.cert_pem)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("scontrol-remote-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn init_writes_a_private_key_and_refuses_to_overwrite_it() {
        let dir = temp_dir();
        let csr = init("agent.example.com", &dir, false).unwrap();
        assert!(csr.contains("BEGIN CERTIFICATE REQUEST"));
        let mode = std::fs::metadata(dir.join("agent.key")).unwrap().mode();
        assert_eq!(mode & 0o777, 0o600);
        let first = std::fs::read_to_string(dir.join("agent.key")).unwrap();
        assert!(init("agent.example.com", &dir, false).is_err());
        assert_eq!(
            std::fs::read_to_string(dir.join("agent.key")).unwrap(),
            first
        );
        init("agent.example.com", &dir, true).unwrap();
        assert_ne!(
            std::fs::read_to_string(dir.join("agent.key")).unwrap(),
            first
        );
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn identity_requires_a_private_key_and_reloads_on_rotation() {
        let dir = temp_dir();
        let ca = pki::generate_ca().unwrap();
        let csr = init("127.0.0.1", &dir, false).unwrap();
        let (cert, _) = pki::sign_agent_csr(&ca, &csr, "127.0.0.1", uuid::Uuid::nil(), 1).unwrap();
        std::fs::write(dir.join("ca.pem"), &ca.cert_pem).unwrap();
        std::fs::write(dir.join("agent.pem"), &cert).unwrap();
        let identity = TlsIdentity::new(RemoteConfig {
            listen: "127.0.0.1:0".into(),
            ca_cert: dir.join("ca.pem"),
            cert: dir.join("agent.pem"),
            key: dir.join("agent.key"),
        });
        let first = identity.current().unwrap();
        assert!(Arc::ptr_eq(&first, &identity.current().unwrap()));

        std::fs::set_permissions(
            dir.join("agent.key"),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        // Forces a reload (mode changes do not touch mtime).
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(dir.join("agent.pem"), &cert).unwrap();
        assert!(identity.current().is_err());
        std::fs::remove_dir_all(dir).ok();
    }
}
