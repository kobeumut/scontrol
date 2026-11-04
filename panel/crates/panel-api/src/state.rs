use crate::auth::api_keys::VerifiedKeys;
use crate::auth::session::CookieCipher;
use crate::config::ApiConfig;
use crate::services::agent_pool::AgentPool;
use crate::services::deploy_queue::DeployQueue;
use crate::services::github_app::GitHubApps;
use crate::services::metrics::MetricsCollector;
use crate::services::notifications::Notifier;
use crate::services::scheduled_jobs::JobRunCollector;
use crate::services::secret_box::SecretBox;
use crate::services::webhook::WebhookKey;
use panel_core::adapters::AdapterPort;
use panel_core::agent_client::AgentClient;
use sqlx::PgPool;
use std::sync::Arc;

#[derive(Clone)]
pub struct AppState {
    pub db: PgPool,
    /// The agent on this host (the `local` server).
    pub agent: Arc<dyn AdapterPort>,
    /// Agents per server; services go to the agent of their server.
    pub agents: AgentPool,
    /// Recently verified API key secrets (see `auth::api_keys`).
    pub api_keys: VerifiedKeys,
    pub cookie_cipher: CookieCipher,
    pub session_ttl_hours: i64,
    pub deploy_queue: DeployQueue,
    pub webhook_key: WebhookKey,
    pub notifier: Notifier,
    /// Seals stored secrets (layered environment variables).
    pub secret_box: SecretBox,
    pub github_apps: GitHubApps,
    /// Copies scheduled-job runs from the agent (see `services::scheduled_jobs`).
    pub job_runs: JobRunCollector,
    /// Stores agent metrics and evaluates alerts (see `services::metrics`).
    pub metrics: MetricsCollector,
}

impl AppState {
    pub fn new(db: PgPool, config: &ApiConfig) -> Self {
        let agent: Arc<dyn AdapterPort> =
            Arc::new(AgentClient::new(config.agent_socket_path.clone()));
        Self {
            db,
            agents: AgentPool::new(agent.clone(), SecretBox::derive(&config.secret_key)),
            agent,
            api_keys: VerifiedKeys::default(),
            cookie_cipher: CookieCipher::new(&config.cookie_key),
            session_ttl_hours: config.session_ttl_hours,
            deploy_queue: DeployQueue::new(),
            webhook_key: WebhookKey::derive(&config.cookie_key),
            notifier: Notifier::new(SecretBox::derive(&config.secret_key)),
            secret_box: SecretBox::derive(&config.secret_key),
            github_apps: GitHubApps::new(SecretBox::derive(&config.secret_key)),
            job_runs: JobRunCollector::new(),
            metrics: MetricsCollector::new(),
        }
    }
}
