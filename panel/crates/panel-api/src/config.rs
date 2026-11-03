use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct ApiConfig {
    pub database_url: String,
    pub bind_addr: String,
    pub agent_socket_path: PathBuf,
    /// 32 raw bytes (base64-encoded in the environment) used for AES-GCM
    /// session cookie encryption.
    pub cookie_key: [u8; 32],
    /// Key for secrets stored encrypted at rest (`services::secret_box`).
    pub secret_key: [u8; 32],
    pub session_ttl_hours: i64,
    /// Parallel Compose deployment workers (distinct projects only; one
    /// project's deployments always run one at a time).
    pub deploy_workers: usize,
}

impl ApiConfig {
    pub fn from_env() -> anyhow::Result<Self> {
        let _ = dotenvy::dotenv();

        let database_url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgresql:///scontrol?user=scontrol".to_string());
        let bind_addr =
            std::env::var("PANEL_API_BIND").unwrap_or_else(|_| "127.0.0.1:8080".to_string());
        let agent_socket_path = std::env::var("PANEL_AGENT_SOCKET")
            .unwrap_or_else(|_| "/run/scontrol/agent.sock".to_string())
            .into();

        let cookie_key = match std::env::var("PANEL_COOKIE_KEY") {
            Ok(b64) => decode_key("PANEL_COOKIE_KEY", &b64)?,
            Err(_) => {
                tracing::warn!(
                    "PANEL_COOKIE_KEY not set; generating an ephemeral key (sessions will not \
                     survive a restart). Set PANEL_COOKIE_KEY in production."
                );
                let mut key = [0u8; 32];
                rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut key);
                key
            }
        };

        // Installs from before PANEL_SECRET_KEY existed keep working with a
        // key derived from the cookie key (SecretBox applies its own label),
        // but then rotating the cookie key also invalidates stored secrets.
        let secret_key = match std::env::var("PANEL_SECRET_KEY") {
            Ok(b64) => decode_key("PANEL_SECRET_KEY", &b64)?,
            Err(_) => {
                tracing::warn!(
                    "PANEL_SECRET_KEY not set; stored secrets are keyed from PANEL_COOKIE_KEY, \
                     so rotating that key makes them unreadable"
                );
                cookie_key
            }
        };

        let session_ttl_hours = std::env::var("PANEL_SESSION_TTL_HOURS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(24 * 7);

        let deploy_workers = std::env::var("PANEL_DEPLOY_WORKERS")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|n| (1..=16).contains(n))
            .unwrap_or_else(crate::services::deploy_queue::default_workers);

        Ok(Self {
            database_url,
            bind_addr,
            agent_socket_path,
            cookie_key,
            secret_key,
            session_ttl_hours,
            deploy_workers,
        })
    }
}

/// Decodes a base64 environment value that must hold exactly 32 bytes.
fn decode_key(name: &str, b64: &str) -> anyhow::Result<[u8; 32]> {
    let bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b64.trim())
        .map_err(|e| anyhow::anyhow!("{name} must be base64: {e}"))?;
    bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("{name} must decode to exactly 32 bytes"))
}
