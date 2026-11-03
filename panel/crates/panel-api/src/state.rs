use crate::auth::session::CookieCipher;
use crate::config::ApiConfig;
use panel_core::adapters::AdapterPort;
use panel_core::agent_client::AgentClient;
use sqlx::PgPool;
use std::sync::Arc;

#[derive(Clone)]
pub struct AppState {
    pub db: PgPool,
    pub agent: Arc<dyn AdapterPort>,
    pub cookie_cipher: CookieCipher,
    pub session_ttl_hours: i64,
}

impl AppState {
    pub fn new(db: PgPool, config: &ApiConfig) -> Self {
        Self {
            db,
            agent: Arc::new(AgentClient::new(config.agent_socket_path.clone())),
            cookie_cipher: CookieCipher::new(&config.cookie_key),
            session_ttl_hours: config.session_ttl_hours,
        }
    }
}
