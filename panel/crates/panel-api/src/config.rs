use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct ApiConfig {
    pub database_url: String,
    pub bind_addr: String,
    pub agent_socket_path: PathBuf,
    /// 32 raw bytes (base64-encoded in the environment) used for AES-GCM
    /// session cookie encryption.
    pub cookie_key: [u8; 32],
    pub session_ttl_hours: i64,
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
            Ok(b64) => {
                let bytes =
                    base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b64.trim())
                        .map_err(|e| anyhow::anyhow!("PANEL_COOKIE_KEY must be base64: {e}"))?;
                let mut key = [0u8; 32];
                if bytes.len() != 32 {
                    anyhow::bail!("PANEL_COOKIE_KEY must decode to exactly 32 bytes");
                }
                key.copy_from_slice(&bytes);
                key
            }
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

        let session_ttl_hours = std::env::var("PANEL_SESSION_TTL_HOURS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(24 * 7);

        Ok(Self {
            database_url,
            bind_addr,
            agent_socket_path,
            cookie_key,
            session_ttl_hours,
        })
    }
}
