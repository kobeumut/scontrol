use crate::auth::session::CookieCipher;
use crate::config::ApiConfig;
use crate::services::deploy_queue::DeployQueue;
use crate::services::notifications::Notifier;
use crate::services::secret_box::SecretBox;
use crate::services::webhook::WebhookKey;
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
    pub deploy_queue: DeployQueue,
    pub webhook_key: WebhookKey,
    pub notifier: Notifier,
    /// Seals stored secrets (layered environment variables).
    pub secret_box: SecretBox,
}

impl AppState {
    pub fn new(db: PgPool, config: &ApiConfig) -> Self {
        Self {
            db,
            agent: Arc::new(AgentClient::new(config.agent_socket_path.clone())),
            cookie_cipher: CookieCipher::new(&config.cookie_key),
            session_ttl_hours: config.session_ttl_hours,
            deploy_queue: DeployQueue::new(),
            webhook_key: WebhookKey::derive(&config.cookie_key),
            notifier: Notifier::new(SecretBox::derive(&config.secret_key)),
            secret_box: SecretBox::derive(&config.secret_key),
        }
    }
}
