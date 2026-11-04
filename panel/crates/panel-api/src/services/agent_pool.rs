//! Which agent serves which server, and the panel's side of the PKI.
//!
//! The local server is the agent on the panel host's Unix socket. A remote
//! server is reached over mutual TLS with the panel client certificate, or
//! over a Unix socket that SSH forwards from the remote host. Every service
//! (Compose project) records its server; operations on it go to that agent.

use crate::db;
use crate::db::servers::{Server, Transport, LOCAL_SERVER_ID};
use crate::error::{ApiError, ApiResult};
use crate::services::secret_box::SecretBox;
use chrono::{DateTime, Duration, Utc};
use panel_core::adapters::AdapterPort;
use panel_core::agent_client::{AgentClient, TlsTarget};
use panel_core::pki::{self, KeyedCert};
use sqlx::PgPool;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use tokio::sync::Mutex;
use uuid::Uuid;

/// Forwarded sockets must live here (created by the operator's SSH unit).
pub const SSH_SOCKET_DIR: &str = "/run/scontrol/remote/";

const CA: &str = "ca";
const PANEL_CLIENT: &str = "panel_client";

/// A client TLS configuration and when to renew its certificate.
type CachedClient = (Arc<rustls::ClientConfig>, DateTime<Utc>);

/// The panel CA and client certificate, kept sealed in `pki_material`.
#[derive(Clone)]
pub struct PanelPki {
    secrets: SecretBox,
    /// The client TLS configuration and when its certificate should renew.
    client: Arc<Mutex<Option<CachedClient>>>,
}

impl PanelPki {
    pub fn new(secrets: SecretBox) -> Self {
        Self {
            secrets,
            client: Arc::new(Mutex::new(None)),
        }
    }

    fn unseal(&self, name: &str, row: db::servers::PkiRow) -> ApiResult<KeyedCert> {
        let key = self
            .secrets
            .open(&row.key_sealed, name.as_bytes())
            .map_err(|_| anyhow::anyhow!("the panel {name} key does not unseal"))?;
        Ok(KeyedCert {
            cert_pem: row.cert_pem,
            key_pem: String::from_utf8(key).map_err(|e| anyhow::anyhow!(e))?,
        })
    }

    fn seal(&self, name: &str, cert: &KeyedCert, expires_at: DateTime<Utc>) -> db::servers::PkiRow {
        db::servers::PkiRow {
            cert_pem: cert.cert_pem.clone(),
            key_sealed: self.secrets.seal(cert.key_pem.as_bytes(), name.as_bytes()),
            expires_at,
        }
    }

    /// The CA, created on first use.
    pub async fn ca(&self, pool: &PgPool) -> ApiResult<KeyedCert> {
        if let Some(row) = db::servers::pki_material(pool, CA).await? {
            return self.unseal(CA, row);
        }
        let ca = pki::generate_ca()?;
        let expires_at = Utc::now() + Duration::days(3650);
        // Two processes racing here keep whichever CA was stored first.
        let row = db::servers::store_pki_material(pool, CA, &self.seal(CA, &ca, expires_at), false)
            .await?;
        self.unseal(CA, row)
    }

    /// The panel's client TLS configuration, renewing the client certificate
    /// when it is about to expire.
    pub async fn client_config(&self, pool: &PgPool) -> ApiResult<Arc<rustls::ClientConfig>> {
        let mut cached = self.client.lock().await;
        if let Some((config, renew_at)) = cached.as_ref() {
            if Utc::now() < *renew_at {
                return Ok(config.clone());
            }
        }
        let ca = self.ca(pool).await?;
        let renew_before = Duration::days(pki::CLIENT_RENEW_BEFORE_DAYS);
        let identity = match db::servers::pki_material(pool, PANEL_CLIENT).await? {
            Some(row) if row.expires_at - renew_before > Utc::now() => {
                let renew_at = row.expires_at - renew_before;
                (self.unseal(PANEL_CLIENT, row)?, renew_at)
            }
            _ => {
                let (identity, info) = pki::issue_client_cert(&ca, pki::CLIENT_CERT_DAYS)?;
                db::servers::store_pki_material(
                    pool,
                    PANEL_CLIENT,
                    &self.seal(PANEL_CLIENT, &identity, info.not_after),
                    true,
                )
                .await?;
                tracing::info!(expires_at = %info.not_after, "issued panel client certificate");
                (identity, info.not_after - renew_before)
            }
        };
        let config = pki::client_config(&ca.cert_pem, &identity.0)?;
        *cached = Some((config.clone(), identity.1));
        Ok(config)
    }
}

/// Resolves servers to agent clients.
#[derive(Clone)]
pub struct AgentPool {
    local: Arc<dyn AdapterPort>,
    pki: PanelPki,
    /// Adapters registered in place of a server's real transport (tests).
    overrides: Arc<RwLock<HashMap<Uuid, Arc<dyn AdapterPort>>>>,
    /// One client per server, kept so its open connections are reused;
    /// replaced when the server's address, pins, or the panel identity
    /// change.
    clients: Arc<RwLock<HashMap<Uuid, CachedAgent>>>,
    /// How long a health check waits for an agent's answer. Deployments
    /// and log streams are long on purpose and are not bounded by it.
    health_timeout: std::time::Duration,
}

/// A cached client and what it was built from.
type CachedAgent = (ClientKey, Arc<dyn AdapterPort>);

/// What a cached client was built from.
#[derive(Clone, PartialEq, Eq)]
struct ClientKey {
    transport: Transport,
    address: String,
    pins: Vec<String>,
    /// Identity of the panel's TLS configuration (it changes on renewal).
    config: usize,
}

/// Default bound on one health check (connect, handshake, and answer).
pub const HEALTH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

impl AgentPool {
    pub fn new(local: Arc<dyn AdapterPort>, secrets: SecretBox) -> Self {
        Self {
            local,
            pki: PanelPki::new(secrets),
            overrides: Arc::new(RwLock::new(HashMap::new())),
            clients: Arc::new(RwLock::new(HashMap::new())),
            health_timeout: HEALTH_TIMEOUT,
        }
    }

    #[cfg(test)]
    pub fn set_health_timeout(&mut self, timeout: std::time::Duration) {
        self.health_timeout = timeout;
    }

    /// `capabilities()` bounded by the health timeout, so an agent that
    /// accepts a connection but never answers counts as offline.
    async fn probe(
        &self,
        agent: &dyn AdapterPort,
    ) -> ApiResult<panel_core::adapters::AgentCapabilities> {
        match tokio::time::timeout(self.health_timeout, agent.capabilities()).await {
            Ok(result) => result.map_err(ApiError::from),
            Err(_) => Err(ApiError::Core(panel_core::error::CoreError::External(
                format!(
                    "the agent did not answer within {} s",
                    self.health_timeout.as_secs()
                ),
            ))),
        }
    }

    pub fn pki(&self) -> &PanelPki {
        &self.pki
    }

    #[cfg(test)]
    pub fn set_override(&self, server_id: Uuid, adapter: Arc<dyn AdapterPort>) {
        self.overrides
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(server_id, adapter);
    }

    /// The agent of `server`.
    pub async fn for_server(
        &self,
        pool: &PgPool,
        server: &Server,
    ) -> ApiResult<Arc<dyn AdapterPort>> {
        if server.id == LOCAL_SERVER_ID {
            return Ok(self.local.clone());
        }
        if let Some(adapter) = self
            .overrides
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(&server.id)
        {
            return Ok(adapter.clone());
        }
        let pins = server.accepted_fingerprints(Utc::now());
        let config = match server.transport {
            Transport::Local => return Ok(self.local.clone()),
            Transport::SshSocket => None,
            Transport::Mtls => {
                if pins.is_empty() {
                    return Err(ApiError::Conflict(format!(
                        "server {} has no certificate yet",
                        server.name
                    )));
                }
                Some(self.pki.client_config(pool).await?)
            }
        };
        let key = ClientKey {
            transport: server.transport,
            address: server.address.clone(),
            pins: pins.clone(),
            config: config.as_ref().map_or(0, |c| Arc::as_ptr(c) as usize),
        };
        if let Some((cached, client)) = self
            .clients
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(&server.id)
        {
            if *cached == key {
                return Ok(client.clone());
            }
        }
        let client: Arc<dyn AdapterPort> = match config {
            None => Arc::new(AgentClient::new(&server.address)),
            Some(config) => Arc::new(AgentClient::tls(TlsTarget {
                address: server.address.clone(),
                server_name: server.host().to_string(),
                config,
                pinned_fingerprints: pins,
            })),
        };
        self.clients
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(server.id, (key, client.clone()));
        Ok(client)
    }

    pub async fn for_server_id(&self, pool: &PgPool, id: Uuid) -> ApiResult<Arc<dyn AdapterPort>> {
        if id == LOCAL_SERVER_ID {
            return Ok(self.local.clone());
        }
        let server = db::servers::find(pool, id)
            .await?
            .ok_or_else(|| ApiError::NotFound("server not found".into()))?;
        self.for_server(pool, &server).await
    }

    /// The agent of the server a service is placed on.
    pub async fn for_service(
        &self,
        pool: &PgPool,
        service_id: Uuid,
    ) -> ApiResult<Arc<dyn AdapterPort>> {
        let server_id = db::servers::server_of_service(pool, service_id)
            .await?
            .ok_or_else(|| ApiError::NotFound("compose project not found".into()))?;
        self.for_server_id(pool, server_id).await
    }

    /// Asks the server's agent for its capabilities and records the result.
    /// A successful mTLS check with the new certificate ends a rotation.
    pub async fn check(
        &self,
        pool: &PgPool,
        server: &Server,
    ) -> ApiResult<panel_core::adapters::AgentCapabilities> {
        let result = match self.for_server(pool, server).await {
            Ok(agent) => self.probe(agent.as_ref()).await,
            Err(error) => Err(error),
        };
        match &result {
            Ok(capabilities) => {
                db::servers::record_check(
                    pool,
                    server.id,
                    Ok((capabilities.docker, &capabilities.version)),
                )
                .await?;
                if server.transport == Transport::Mtls && server.previous_fingerprint.is_some() {
                    // Pinned to the new certificate only: if it answers, the
                    // operator installed it, and the old one can go.
                    let mut current_only = server.clone();
                    current_only.previous_fingerprint = None;
                    if let Ok(agent) = self.for_server(pool, &current_only).await {
                        if self.probe(agent.as_ref()).await.is_ok() {
                            db::servers::finish_rotation(pool, server.id).await?;
                        }
                    }
                }
            }
            Err(error) => {
                let message = match error {
                    ApiError::Core(core) => core.message().to_string(),
                    other => other.to_string(),
                };
                db::servers::record_check(pool, server.id, Err(&message)).await?;
            }
        }
        result
    }

    /// Checks every remote server at once; each check is bounded, so one
    /// silent server delays neither the others nor the next round.
    pub async fn check_all(&self, pool: &PgPool) -> sqlx::Result<()> {
        let servers = db::servers::list(pool).await?;
        let checks = servers
            .iter()
            .filter(|s| s.id != LOCAL_SERVER_ID)
            .map(|server| async move {
                if let Err(error) = self.check(pool, server).await {
                    tracing::debug!(server = %server.name, %error, "server check failed");
                }
            });
        futures::future::join_all(checks).await;
        Ok(())
    }

    /// Checks every remote server once a minute so placement sees which are
    /// online. The local agent is reported by `/health/agent`.
    pub fn start_health_checks(&self, pool: PgPool) {
        let agents = self.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                if let Err(error) = agents.check_all(&pool).await {
                    tracing::warn!(%error, "listing servers for health checks");
                }
            }
        });
    }
}

/// Socket paths an `ssh_socket` server may use:
/// `/run/scontrol/remote/<name>.sock` with a plain file name.
pub fn is_valid_ssh_socket_path(path: &str) -> bool {
    let Some(file) = path.strip_prefix(SSH_SOCKET_DIR) else {
        return false;
    };
    let Some(stem) = file.strip_suffix(".sock") else {
        return false;
    };
    (1..=64).contains(&stem.len())
        && stem
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_socket_paths_are_allow_listed() {
        assert!(is_valid_ssh_socket_path("/run/scontrol/remote/web-2.sock"));
        assert!(!is_valid_ssh_socket_path("/run/scontrol/agent.sock"));
        assert!(!is_valid_ssh_socket_path(
            "/run/scontrol/remote/../agent.sock"
        ));
        assert!(!is_valid_ssh_socket_path("/run/scontrol/remote/a/b.sock"));
        assert!(!is_valid_ssh_socket_path("/run/scontrol/remote/.sock"));
        assert!(!is_valid_ssh_socket_path("/tmp/x.sock"));
    }

    #[tokio::test]
    async fn the_ca_is_created_once_and_the_client_certificate_is_reused() {
        let pool = crate::test_support::test_pool().await;
        let _guard = crate::test_support::PKI_TESTS.lock().await;
        sqlx::query("DELETE FROM pki_material")
            .execute(&pool)
            .await
            .unwrap();
        let pki = PanelPki::new(SecretBox::derive(&[42u8; 32]));
        let first = pki.ca(&pool).await.unwrap();
        let again = PanelPki::new(SecretBox::derive(&[42u8; 32]))
            .ca(&pool)
            .await
            .unwrap();
        assert_eq!(first.cert_pem, again.cert_pem);
        // The key is sealed at rest.
        let row = db::servers::pki_material(&pool, CA).await.unwrap().unwrap();
        assert!(!String::from_utf8_lossy(&row.key_sealed).contains("PRIVATE KEY"));

        pki.client_config(&pool).await.unwrap();
        let client = db::servers::pki_material(&pool, PANEL_CLIENT)
            .await
            .unwrap()
            .unwrap();
        // A fresh process reuses the stored certificate.
        PanelPki::new(SecretBox::derive(&[42u8; 32]))
            .client_config(&pool)
            .await
            .unwrap();
        let reused = db::servers::pki_material(&pool, PANEL_CLIENT)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(client.cert_pem, reused.cert_pem);

        // One close to expiry is rotated.
        sqlx::query("UPDATE pki_material SET expires_at = now() + interval '1 day' WHERE name = 'panel_client'")
            .execute(&pool)
            .await
            .unwrap();
        PanelPki::new(SecretBox::derive(&[42u8; 32]))
            .client_config(&pool)
            .await
            .unwrap();
        let rotated = db::servers::pki_material(&pool, PANEL_CLIENT)
            .await
            .unwrap()
            .unwrap();
        assert_ne!(client.cert_pem, rotated.cert_pem);
        assert!(rotated.expires_at > Utc::now() + Duration::days(pki::CLIENT_CERT_DAYS - 1));
    }

    #[tokio::test]
    async fn a_silent_agent_does_not_hold_up_the_other_checks() {
        use panel_core::adapters::mock::MockAdapter;
        let pool = crate::test_support::test_pool().await;
        let dir = std::env::temp_dir().join(format!("scontrol-silent-{}", Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("silent.sock");
        // Accepts connections and reads, but never answers.
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((stream, _)) = listener.accept().await {
                held.push(stream);
            }
        });
        let suffix = &Uuid::new_v4().simple().to_string()[..8];
        let (silent, healthy) = (Uuid::new_v4(), Uuid::new_v4());
        for (id, name, address) in [
            (
                silent,
                format!("silent-{suffix}"),
                socket.display().to_string(),
            ),
            (
                healthy,
                format!("healthy-{suffix}"),
                "/run/scontrol/remote/x.sock".to_string(),
            ),
        ] {
            db::servers::insert(
                &pool,
                db::servers::NewServer {
                    id,
                    name: &name,
                    transport: Transport::SshSocket,
                    address: &address,
                    max_services: None,
                },
            )
            .await
            .unwrap();
        }
        let mut agents =
            AgentPool::new(Arc::new(MockAdapter::new()), SecretBox::derive(&[42u8; 32]));
        agents.set_health_timeout(std::time::Duration::from_millis(300));
        agents.set_override(healthy, Arc::new(MockAdapter::new()));

        let started = std::time::Instant::now();
        tokio::time::timeout(std::time::Duration::from_secs(5), agents.check_all(&pool))
            .await
            .expect("check_all finishes despite the silent agent")
            .unwrap();
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
        let silent_row = db::servers::find(&pool, silent).await.unwrap().unwrap();
        assert_eq!(silent_row.status, "offline");
        assert!(silent_row.last_error.unwrap().contains("did not answer"));
        assert_eq!(
            db::servers::find(&pool, healthy)
                .await
                .unwrap()
                .unwrap()
                .status,
            "online"
        );
        for id in [silent, healthy] {
            db::servers::delete(&pool, id).await.unwrap();
        }
        std::fs::remove_dir_all(dir).ok();
    }
}
