#[cfg(feature = "antivirus")]
pub mod antivirus;
#[cfg(feature = "builders")]
pub mod builders;
#[cfg(feature = "docker")]
pub mod compose;
pub mod cron;
pub mod database;
#[cfg(feature = "docker")]
pub mod deploy_log;
#[cfg(feature = "docker")]
pub mod docker;
#[cfg(feature = "docker")]
pub mod docker_config;
pub mod exec;
pub mod firewall;
#[cfg(feature = "docker")]
pub mod git_auth;
#[cfg(feature = "mail")]
pub mod mail;
pub mod nginx;
pub mod php_fpm;
#[cfg(feature = "docker")]
pub mod scheduled_jobs;
pub mod systemd_app;
pub mod tls;
pub mod users;

use crate::config::AgentConfig;
use async_trait::async_trait;
use panel_core::adapters::*;
use panel_core::error::{CoreError, CoreResult};
use panel_core::types::*;

/// The real, host-executing [`AdapterPort`] implementation used by the
/// agent's RPC dispatcher.
pub struct LocalAdapter {
    pub config: AgentConfig,
    /// Cancellation senders of deployments currently running in this agent.
    running_deployments:
        std::sync::Mutex<std::collections::HashMap<uuid::Uuid, tokio::sync::watch::Sender<bool>>>,
}

impl LocalAdapter {
    pub fn new(config: AgentConfig) -> Self {
        Self {
            config,
            running_deployments: Default::default(),
        }
    }

    /// Builders compiled in and present on `PATH` (no process is spawned).
    fn build_tools(&self) -> Vec<BuildTool> {
        #[cfg(feature = "builders")]
        if self.config.docker_enabled() {
            return builders::available();
        }
        Vec::new()
    }

    fn feature_disabled(name: &str) -> CoreError {
        CoreError::PermissionDenied(format!(
            "the '{name}' optional feature was not selected at install time"
        ))
    }
}

#[async_trait]
impl AdapterPort for LocalAdapter {
    async fn capabilities(&self) -> CoreResult<AgentCapabilities> {
        Ok(AgentCapabilities {
            docker: self.config.docker_enabled(),
            mail: self.config.mail_enabled(),
            antivirus: self.config.antivirus_enabled(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            build_tools: self.build_tools(),
        })
    }

    async fn create_site(&self, params: CreateSiteParams) -> CoreResult<()> {
        if !validate::is_valid_domain(&params.domain) {
            return Err(CoreError::Validation(format!(
                "invalid domain: {}",
                params.domain
            )));
        }
        if !validate::is_valid_username(&params.owner_username) {
            return Err(CoreError::Validation(format!(
                "invalid username: {}",
                params.owner_username
            )));
        }

        let home_dir = self.config.sites_root.join(&params.owner_username);
        let document_root = home_dir.join(&params.domain).join("public");

        users::create_user(&params.owner_username, &home_dir.to_string_lossy()).await?;
        tokio::fs::create_dir_all(&document_root)
            .await
            .map_err(|e| CoreError::Io(format!("creating document root: {e}")))?;

        match &params.runtime {
            Runtime::Php { version } => {
                php_fpm::write_pool(
                    version,
                    &params.domain,
                    &params.owner_username,
                    &document_root.to_string_lossy(),
                )
                .await?;
            }
            Runtime::NodeJs { .. } | Runtime::Python { .. } => {
                systemd_app::write_unit(
                    &self.config,
                    &params.domain,
                    &params.owner_username,
                    &document_root.to_string_lossy(),
                    &params.runtime,
                )
                .await?;
            }
            Runtime::Static | Runtime::ReverseProxy { .. } => {}
        }

        nginx::write_vhost(
            &self.config,
            &params.domain,
            &document_root.to_string_lossy(),
            &params.runtime,
        )
        .await?;

        Ok(())
    }

    async fn delete_site(&self, params: DeleteSiteParams) -> CoreResult<()> {
        nginx::remove_vhost(&self.config, &params.domain).await?;
        systemd_app::remove_unit(&self.config, &params.domain)
            .await
            .ok();
        if let Some(version) = &params.php_version {
            php_fpm::remove_pool(version, &params.domain).await.ok();
        }
        Ok(())
    }

    async fn reload_web_server(&self) -> CoreResult<()> {
        nginx::test_and_reload().await
    }

    async fn issue_certificate(&self, params: IssueCertificateParams) -> CoreResult<()> {
        tls::issue_certificate(&params.domain, &params.admin_email).await
    }

    async fn create_database(&self, params: CreateDatabaseParams) -> CoreResult<()> {
        database::create_database(
            params.engine,
            &params.name,
            &params.username,
            &params.password,
        )
        .await
    }

    async fn delete_database(&self, params: DeleteDatabaseParams) -> CoreResult<()> {
        database::delete_database(params.engine, &params.name, &params.username).await
    }

    async fn create_system_user(&self, params: CreateSystemUserParams) -> CoreResult<()> {
        users::create_user(&params.username, &params.home_dir).await
    }

    async fn delete_system_user(&self, params: DeleteSystemUserParams) -> CoreResult<()> {
        users::delete_user(&params.username).await
    }

    async fn apply_firewall_rule(&self, params: ApplyFirewallRuleParams) -> CoreResult<()> {
        firewall::apply_rule(params.port, params.protocol, params.allow).await
    }

    async fn upsert_cron_job(&self, params: UpsertCronJobParams) -> CoreResult<()> {
        cron::upsert_job(
            &params.owner_username,
            params.job_id,
            &params.schedule,
            &params.command,
            params.enabled,
        )
        .await
    }

    async fn remove_cron_job(&self, params: RemoveCronJobParams) -> CoreResult<()> {
        cron::remove_job(&params.owner_username, params.job_id).await
    }

    async fn write_compose_environment(
        &self,
        params: WriteComposeEnvironmentParams,
    ) -> CoreResult<()> {
        #[cfg(feature = "docker")]
        {
            if !self.config.docker_enabled() {
                return Err(Self::feature_disabled("docker"));
            }
            return compose::write_project_environment(&self.config, params).await;
        }
        #[cfg(not(feature = "docker"))]
        {
            let _ = params;
            Err(Self::feature_disabled("docker"))
        }
    }

    async fn deploy_compose_project(
        &self,
        params: DeployComposeProjectParams,
    ) -> CoreResult<ComposeDeployOutcome> {
        #[cfg(feature = "docker")]
        {
            if !self.config.docker_enabled() {
                return Err(Self::feature_disabled("docker"));
            }
            let id = params.deployment_id;
            let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
            self.running_deployments
                .lock()
                .expect("deployment registry poisoned")
                .insert(id, cancel_tx);
            let result = compose::deploy(&self.config, params, cancel_rx).await;
            self.running_deployments
                .lock()
                .expect("deployment registry poisoned")
                .remove(&id);
            return result;
        }
        #[cfg(not(feature = "docker"))]
        {
            let _ = params;
            Err(Self::feature_disabled("docker"))
        }
    }

    async fn generate_deploy_key(&self, params: ComposeProjectActionParams) -> CoreResult<String> {
        #[cfg(feature = "docker")]
        {
            if !self.config.docker_enabled() {
                return Err(Self::feature_disabled("docker"));
            }
            return git_auth::generate_deploy_key(&self.config, params).await;
        }
        #[cfg(not(feature = "docker"))]
        {
            let _ = params;
            Err(Self::feature_disabled("docker"))
        }
    }

    async fn remove_deploy_key(&self, params: ComposeProjectActionParams) -> CoreResult<()> {
        #[cfg(feature = "docker")]
        {
            if !self.config.docker_enabled() {
                return Err(Self::feature_disabled("docker"));
            }
            return git_auth::remove_deploy_key(&self.config, params).await;
        }
        #[cfg(not(feature = "docker"))]
        {
            let _ = params;
            Err(Self::feature_disabled("docker"))
        }
    }

    async fn write_git_credentials(&self, params: WriteGitCredentialsParams) -> CoreResult<()> {
        #[cfg(feature = "docker")]
        {
            if !self.config.docker_enabled() {
                return Err(Self::feature_disabled("docker"));
            }
            return git_auth::write_credentials(&self.config, params).await;
        }
        #[cfg(not(feature = "docker"))]
        {
            let _ = params;
            Err(Self::feature_disabled("docker"))
        }
    }

    async fn write_docker_config(&self, params: WriteDockerConfigParams) -> CoreResult<()> {
        #[cfg(feature = "docker")]
        {
            if !self.config.docker_enabled() {
                return Err(Self::feature_disabled("docker"));
            }
            return docker_config::write_config(&self.config, params).await;
        }
        #[cfg(not(feature = "docker"))]
        {
            let _ = params;
            Err(Self::feature_disabled("docker"))
        }
    }

    async fn test_docker_registry(&self, params: TestDockerRegistryParams) -> CoreResult<()> {
        #[cfg(feature = "docker")]
        {
            if !self.config.docker_enabled() {
                return Err(Self::feature_disabled("docker"));
            }
            return docker_config::test_registry(&self.config, params).await;
        }
        #[cfg(not(feature = "docker"))]
        {
            let _ = params;
            Err(Self::feature_disabled("docker"))
        }
    }

    async fn stop_compose_project(&self, params: ComposeProjectActionParams) -> CoreResult<()> {
        #[cfg(feature = "docker")]
        {
            if !self.config.docker_enabled() {
                return Err(Self::feature_disabled("docker"));
            }
            return compose::stop(&self.config, params).await;
        }
        #[cfg(not(feature = "docker"))]
        {
            let _ = params;
            Err(Self::feature_disabled("docker"))
        }
    }

    async fn remove_compose_project(&self, params: ComposeProjectActionParams) -> CoreResult<()> {
        #[cfg(feature = "docker")]
        {
            if !self.config.docker_enabled() {
                return Err(Self::feature_disabled("docker"));
            }
            return compose::remove(&self.config, params).await;
        }
        #[cfg(not(feature = "docker"))]
        {
            let _ = params;
            Err(Self::feature_disabled("docker"))
        }
    }

    async fn list_compose_project_services(
        &self,
        params: ComposeProjectActionParams,
    ) -> CoreResult<Vec<ComposeServiceInfo>> {
        #[cfg(feature = "docker")]
        {
            if !self.config.docker_enabled() {
                return Err(Self::feature_disabled("docker"));
            }
            return compose::list_services_for(&self.config, &params.project_slug).await;
        }
        #[cfg(not(feature = "docker"))]
        {
            let _ = params;
            Err(Self::feature_disabled("docker"))
        }
    }

    async fn follow_compose_logs(&self, params: FollowComposeLogsParams) -> CoreResult<LogLines> {
        #[cfg(feature = "docker")]
        {
            if !self.config.docker_enabled() {
                return Err(Self::feature_disabled("docker"));
            }
            return compose::follow_logs(&self.config, params).await;
        }
        #[cfg(not(feature = "docker"))]
        {
            let _ = params;
            Err(Self::feature_disabled("docker"))
        }
    }

    async fn compose_project_logs(&self, params: ComposeProjectLogsParams) -> CoreResult<String> {
        #[cfg(feature = "docker")]
        {
            if !self.config.docker_enabled() {
                return Err(Self::feature_disabled("docker"));
            }
            return compose::logs(&self.config, params).await;
        }
        #[cfg(not(feature = "docker"))]
        {
            let _ = params;
            Err(Self::feature_disabled("docker"))
        }
    }

    async fn read_deployment_log(
        &self,
        params: DeploymentLogParams,
    ) -> CoreResult<DeploymentLogChunk> {
        #[cfg(feature = "docker")]
        {
            if !self.config.docker_enabled() {
                return Err(Self::feature_disabled("docker"));
            }
            return compose::read_deployment_log(&self.config, params).await;
        }
        #[cfg(not(feature = "docker"))]
        {
            let _ = params;
            Err(Self::feature_disabled("docker"))
        }
    }

    async fn cancel_deployment(&self, params: CancelDeploymentParams) -> CoreResult<bool> {
        let sender = self
            .running_deployments
            .lock()
            .expect("deployment registry poisoned")
            .get(&params.deployment_id)
            .cloned();
        Ok(sender.is_some_and(|tx| tx.send(true).is_ok()))
    }

    async fn upsert_scheduled_job(&self, params: UpsertScheduledJobParams) -> CoreResult<()> {
        #[cfg(feature = "docker")]
        {
            if !self.config.docker_enabled() {
                return Err(Self::feature_disabled("docker"));
            }
            return scheduled_jobs::upsert(&self.config, params).await;
        }
        #[cfg(not(feature = "docker"))]
        {
            let _ = params;
            Err(Self::feature_disabled("docker"))
        }
    }

    async fn remove_scheduled_job(&self, params: ScheduledJobRef) -> CoreResult<()> {
        #[cfg(feature = "docker")]
        {
            if !self.config.docker_enabled() {
                return Err(Self::feature_disabled("docker"));
            }
            return scheduled_jobs::remove(&self.config, params).await;
        }
        #[cfg(not(feature = "docker"))]
        {
            let _ = params;
            Err(Self::feature_disabled("docker"))
        }
    }

    async fn run_scheduled_job(&self, params: ScheduledJobRef) -> CoreResult<()> {
        #[cfg(feature = "docker")]
        {
            if !self.config.docker_enabled() {
                return Err(Self::feature_disabled("docker"));
            }
            return scheduled_jobs::run_now(&self.config, params).await;
        }
        #[cfg(not(feature = "docker"))]
        {
            let _ = params;
            Err(Self::feature_disabled("docker"))
        }
    }

    async fn list_scheduled_job_runs(
        &self,
        params: ListScheduledJobRunsParams,
    ) -> CoreResult<Vec<panel_core::schedule::JobRunInfo>> {
        #[cfg(feature = "docker")]
        {
            if !self.config.docker_enabled() {
                return Err(Self::feature_disabled("docker"));
            }
            return scheduled_jobs::list_runs(&self.config, params).await;
        }
        #[cfg(not(feature = "docker"))]
        {
            let _ = params;
            Err(Self::feature_disabled("docker"))
        }
    }

    async fn read_scheduled_job_run_log(
        &self,
        params: ScheduledJobRunLogParams,
    ) -> CoreResult<DeploymentLogChunk> {
        #[cfg(feature = "docker")]
        {
            if !self.config.docker_enabled() {
                return Err(Self::feature_disabled("docker"));
            }
            return scheduled_jobs::read_run_log(&self.config, params).await;
        }
        #[cfg(not(feature = "docker"))]
        {
            let _ = params;
            Err(Self::feature_disabled("docker"))
        }
    }

    async fn create_container(&self, params: CreateContainerParams) -> CoreResult<ContainerInfo> {
        #[cfg(feature = "docker")]
        {
            if !self.config.docker_enabled() {
                return Err(Self::feature_disabled("docker"));
            }
            return docker::create_container(
                &params.name,
                &params.image,
                &params.ports,
                &params.env,
            )
            .await;
        }
        #[cfg(not(feature = "docker"))]
        {
            let _ = params;
            Err(Self::feature_disabled("docker"))
        }
    }

    async fn start_container(&self, params: ContainerActionParams) -> CoreResult<()> {
        #[cfg(feature = "docker")]
        {
            if !self.config.docker_enabled() {
                return Err(Self::feature_disabled("docker"));
            }
            return docker::start_container(&params.name).await;
        }
        #[cfg(not(feature = "docker"))]
        {
            let _ = params;
            Err(Self::feature_disabled("docker"))
        }
    }

    async fn stop_container(&self, params: ContainerActionParams) -> CoreResult<()> {
        #[cfg(feature = "docker")]
        {
            if !self.config.docker_enabled() {
                return Err(Self::feature_disabled("docker"));
            }
            return docker::stop_container(&params.name).await;
        }
        #[cfg(not(feature = "docker"))]
        {
            let _ = params;
            Err(Self::feature_disabled("docker"))
        }
    }

    async fn remove_container(&self, params: ContainerActionParams) -> CoreResult<()> {
        #[cfg(feature = "docker")]
        {
            if !self.config.docker_enabled() {
                return Err(Self::feature_disabled("docker"));
            }
            return docker::remove_container(&params.name).await;
        }
        #[cfg(not(feature = "docker"))]
        {
            let _ = params;
            Err(Self::feature_disabled("docker"))
        }
    }

    async fn list_containers(&self) -> CoreResult<Vec<ContainerInfo>> {
        #[cfg(feature = "docker")]
        {
            if !self.config.docker_enabled() {
                return Err(Self::feature_disabled("docker"));
            }
            return docker::list_containers().await;
        }
        #[cfg(not(feature = "docker"))]
        {
            Err(Self::feature_disabled("docker"))
        }
    }

    async fn create_mailbox(&self, params: CreateMailboxParams) -> CoreResult<MailboxInfo> {
        #[cfg(feature = "mail")]
        {
            if !self.config.mail_enabled() {
                return Err(Self::feature_disabled("mail"));
            }
            return mail::create_mailbox(&params.address, &params.password, params.quota_mb).await;
        }
        #[cfg(not(feature = "mail"))]
        {
            let _ = params;
            Err(Self::feature_disabled("mail"))
        }
    }

    async fn delete_mailbox(&self, params: DeleteMailboxParams) -> CoreResult<()> {
        #[cfg(feature = "mail")]
        {
            if !self.config.mail_enabled() {
                return Err(Self::feature_disabled("mail"));
            }
            return mail::delete_mailbox(&params.address).await;
        }
        #[cfg(not(feature = "mail"))]
        {
            let _ = params;
            Err(Self::feature_disabled("mail"))
        }
    }

    async fn scan_path(&self, params: ScanPathParams) -> CoreResult<ScanResult> {
        #[cfg(feature = "antivirus")]
        {
            if !self.config.antivirus_enabled() {
                return Err(Self::feature_disabled("antivirus"));
            }
            return antivirus::scan_path(&params.path).await;
        }
        #[cfg(not(feature = "antivirus"))]
        {
            let _ = params;
            Err(Self::feature_disabled("antivirus"))
        }
    }
}
