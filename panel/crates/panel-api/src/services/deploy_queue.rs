//! Durable Compose deployment queue.
//!
//! Dokploy keeps its self-hosted build queue in memory, so a restart drops
//! queued work. Here the `compose_deployments` table is the queue: enqueueing
//! inserts a `queued` row, and workers claim rows with `FOR UPDATE SKIP
//! LOCKED` (see `db::compose_projects::claim_next_deployment`). Deployments
//! of one project run strictly in order; different projects run in parallel
//! up to the worker count. Workers wake immediately on enqueue and otherwise
//! poll at a slow interval, so an idle panel costs one indexed query per
//! worker every [`IDLE_POLL`].
//!
//! Job rows never contain environment values. Right before a deployment
//! the worker merges the service's sealed variable layers (project →
//! environment → service, see `services::env_layers`) and writes the result
//! to the agent with `write_compose_environment`. A GitHub App installation token is
//! minted when the job starts and sent to the agent with that deployment
//! only, so it never reaches the queue either.
//!
//! A rollback is a queued deployment with `rollback_of` set. Its worker
//! skips everything that prepares a source (registry login, GitHub App
//! token, Git, build, pull), still writes the current merged environment,
//! and asks the agent to start the target's pinned release again. A
//! successful deployment stores the agent's sanitized release snapshot;
//! before it is stored, every environment value the worker sent is masked
//! in it once more.

use crate::db;
use crate::db::compose_projects::ClaimedDeployment;
use crate::error::{ApiError, ApiResult};
use crate::services::database_services;
use crate::services::env_layers;
use crate::services::github_app::{self, GitHubApps};
use crate::services::notifications::Notifier;
use crate::services::registries;
use crate::services::secret_box::SecretBox;
use panel_core::adapters::{
    AdapterPort, DeployComposeProjectParams, DockerRegistryAuth, GitHttpCredentials,
    WriteComposeEnvironmentParams, WriteDockerConfigParams,
};
use panel_core::error::{CoreError, CoreResult};
use panel_core::types::{
    ComposeDeployOutcome, ComposeDomainBinding, ComposeProject, ComposeSource, DeploymentInfo,
    DeploymentSnapshot, DeploymentStatus, RollbackTarget,
};
use sqlx::PgPool;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;
use uuid::Uuid;

const IDLE_POLL: Duration = Duration::from_secs(15);
const MAX_ERROR_LEN: usize = 1_024;

#[derive(Clone, Default)]
pub struct DeployQueue {
    wake: Arc<Notify>,
}

impl DeployQueue {
    pub fn new() -> Self {
        Self::default()
    }

    /// Spawns `workers` claim loops. Call once, from the process that serves
    /// HTTP (MCP stdio processes enqueue but leave execution to it).
    pub fn start_workers(
        &self,
        db: PgPool,
        agent: Arc<dyn AdapterPort>,
        notifier: Notifier,
        secrets: SecretBox,
        github: GitHubApps,
        workers: usize,
    ) {
        for worker in 0..workers.max(1) {
            let (db, agent, wake) = (db.clone(), agent.clone(), self.wake.clone());
            let (notifier, secrets, github) = (notifier.clone(), secrets.clone(), github.clone());
            tokio::spawn(async move {
                worker_loop(worker, db, agent, notifier, secrets, github, wake).await
            });
        }
    }

    pub async fn enqueue(
        &self,
        db: &PgPool,
        project_id: Uuid,
        project_slug: &str,
        pull_images: bool,
        trigger: &str,
    ) -> ApiResult<DeploymentInfo> {
        self.enqueue_job(db, project_id, project_slug, pull_images, trigger, None)
            .await
    }

    /// Queues a rollback to `target`, a succeeded deployment of `project`
    /// whose release the agent still keeps. Errors name the reason: unknown
    /// target (`NotFound`), a source kind that has no releases
    /// (`BadRequest`), or a target without a retained release (`Conflict`).
    pub async fn enqueue_rollback(
        &self,
        db: &PgPool,
        project: &ComposeProject,
        target: Uuid,
    ) -> ApiResult<DeploymentInfo> {
        if !supports_rollback(&project.source) {
            return Err(ApiError::BadRequest(
                "static sites and database services cannot be rolled back".into(),
            ));
        }
        let release = db::compose_projects::find_release(db, project.id, target)
            .await?
            .ok_or_else(|| ApiError::NotFound("deployment not found".into()))?;
        rollback_target(&release, target).map_err(ApiError::Conflict)?;
        self.enqueue_job(
            db,
            project.id,
            &project.slug,
            false,
            "rollback",
            Some(target),
        )
        .await
    }

    async fn enqueue_job(
        &self,
        db: &PgPool,
        project_id: Uuid,
        project_slug: &str,
        pull_images: bool,
        trigger: &str,
        rollback_of: Option<Uuid>,
    ) -> ApiResult<DeploymentInfo> {
        let id = Uuid::new_v4();
        let log_path = format!("agent://compose/{project_slug}/logs/{id}.log");
        let deployment = db::compose_projects::enqueue_deployment(
            db,
            id,
            project_id,
            &log_path,
            pull_images,
            trigger,
            rollback_of,
        )
        .await?;
        // Wake idle workers now; the stored permit covers a worker that is
        // between "found nothing" and "waiting".
        self.wake.notify_waiters();
        self.wake.notify_one();
        Ok(deployment)
    }
}

/// Static sites run no containers and database services must not start an
/// older engine on newer data, so neither records releases.
pub fn supports_rollback(source: &ComposeSource) -> bool {
    !matches!(
        source,
        ComposeSource::Static(_) | ComposeSource::Database(_)
    )
}

/// The agent request for rolling back to `release`, or why it cannot be.
fn rollback_target(
    release: &db::compose_projects::ReleaseRecord,
    deployment_id: Uuid,
) -> Result<RollbackTarget, String> {
    if release.status != DeploymentStatus::Succeeded {
        return Err("only a succeeded deployment can be rolled back to".into());
    }
    let snapshot = release
        .snapshot
        .as_ref()
        .ok_or("this deployment recorded no release to roll back to")?;
    if !release.retained {
        return Err("the release of this deployment is no longer kept on the server".into());
    }
    Ok(RollbackTarget {
        deployment_id,
        compose_sha256: snapshot.compose_sha256.clone(),
        images: snapshot.images.clone(),
    })
}

/// Default worker count: half the CPUs, between 1 and 4. Builds are CPU and
/// I/O heavy; small VPSes should not build several images at once.
pub fn default_workers() -> usize {
    std::thread::available_parallelism()
        .map(|n| (n.get() / 2).clamp(1, 4))
        .unwrap_or(1)
}

async fn worker_loop(
    worker: usize,
    db: PgPool,
    agent: Arc<dyn AdapterPort>,
    notifier: Notifier,
    secrets: SecretBox,
    github: GitHubApps,
    wake: Arc<Notify>,
) {
    loop {
        match db::compose_projects::claim_next_deployment(&db).await {
            Ok(Some(job)) => {
                tracing::info!(worker, deployment_id = %job.id, project_id = %job.project_id, "deployment started");
                let (project_id, deployment_id) = (job.project_id, job.id);
                run_job(&db, agent.as_ref(), &secrets, &github, job).await;
                notifier.deployment_finished(db.clone(), project_id, deployment_id);
                continue;
            }
            Ok(None) => {}
            Err(error) => tracing::error!(worker, %error, "claiming deployment failed"),
        }
        tokio::select! {
            _ = wake.notified() => {}
            _ = tokio::time::sleep(IDLE_POLL) => {}
        }
    }
}

pub(crate) async fn run_job(
    db: &PgPool,
    agent: &dyn AdapterPort,
    secrets: &SecretBox,
    github: &GitHubApps,
    job: ClaimedDeployment,
) {
    let result = execute(db, agent, secrets, github, &job).await;
    if let Ok(outcome) = &result {
        if let Err(error) = db::compose_projects::record_releases(
            db,
            job.project_id,
            job.id,
            outcome.snapshot.as_ref(),
            &outcome.retained_releases,
        )
        .await
        {
            tracing::error!(deployment_id = %job.id, %error, "recording the release failed");
        }
    }
    let message = result.as_ref().err().map(ToString::to_string);
    let (status, commit) = match &result {
        Ok(outcome) => (DeploymentStatus::Succeeded, outcome.commit_sha.as_deref()),
        Err(CoreError::Cancelled(_)) => (DeploymentStatus::Cancelled, None),
        Err(_) => (DeploymentStatus::Failed, None),
    };
    let error = message.as_deref().map(|m| truncate(m, MAX_ERROR_LEN));
    tracing::info!(deployment_id = %job.id, %status, "deployment finished");
    if let Err(error) =
        db::compose_projects::finish_deployment(db, job.id, status, error, commit).await
    {
        tracing::error!(deployment_id = %job.id, %error, "recording deployment result failed");
    }
}

async fn execute(
    db: &PgPool,
    agent: &dyn AdapterPort,
    secrets: &SecretBox,
    github: &GitHubApps,
    job: &ClaimedDeployment,
) -> CoreResult<ComposeDeployOutcome> {
    let project = db::compose_projects::find_by_id(db, job.project_id)
        .await
        .map_err(|e| CoreError::External(format!("loading project: {e}")))?
        .ok_or_else(|| CoreError::NotFound("project no longer exists".into()))?;
    let rollback = match job.rollback_of {
        Some(target) => {
            if !supports_rollback(&project.source) {
                return Err(CoreError::Validation(
                    "static sites and database services cannot be rolled back".into(),
                ));
            }
            let release = db::compose_projects::find_release(db, project.id, target)
                .await
                .map_err(|e| CoreError::External(format!("loading the rollback target: {e}")))?
                .ok_or_else(|| {
                    CoreError::NotFound("the rollback target no longer exists".into())
                })?;
            let request = rollback_target(&release, target).map_err(CoreError::Validation)?;
            Some((request, release.commit_sha))
        }
        None => None,
    };
    // Values this job sends the agent, masked again in the stored snapshot.
    let mut sent_secrets: Vec<String> = Vec::new();
    // A service that predates layering keeps the env file the agent has;
    // the panel does not know its own variables, so overwriting it would
    // lose them.
    if project.env_layered {
        let (environment, masked_values) = env_layers::merged_for_service(db, secrets, project.id)
            .await
            // Errors name keys at most (decryption failures name none).
            .map_err(|e| CoreError::External(format!("loading environment variables: {e}")))?;
        sent_secrets.extend(environment.iter().map(|(_, value)| value.clone()));
        sent_secrets.extend(masked_values.iter().cloned());
        agent
            .write_compose_environment(WriteComposeEnvironmentParams {
                project_slug: project.slug.clone(),
                environment,
                masked_values,
            })
            .await?;
    }
    let domains = db::compose_projects::list_domains(db, project.id)
        .await
        .map_err(|e| CoreError::External(format!("loading domains: {e}")))?;
    let acme_email = if domains.iter().any(|d| d.https) {
        db::users::find_by_id(db, project.owner_user_id)
            .await
            .map_err(|e| CoreError::External(format!("loading project owner: {e}")))?
            .map(|owner| owner.email)
    } else {
        None
    };
    let slug = project.slug.clone();
    // A rollback starts pinned local images: nothing to pull or fetch, so
    // no registry login and no GitHub App token are created for it.
    let registry = match rollback {
        Some(_) => None,
        None => registry_login(db, secrets, project.id).await?,
    };
    let has_registry = registry.is_some();
    if let Some(auth) = registry {
        sent_secrets.push(auth.password.clone());
        // Registry credentials reach the agent right before the pull and are
        // removed right after, win or lose; the job row never carries them.
        agent
            .write_docker_config(WriteDockerConfigParams {
                project_slug: slug.clone(),
                registries: vec![auth],
            })
            .await?;
    }
    let git_credentials = match rollback {
        Some(_) => None,
        None => github_app_credentials(db, github, &project).await?,
    };
    sent_secrets.extend(git_credentials.iter().map(|c| c.token.clone()));
    // Database credentials are unsealed for this request only.
    let database = if matches!(project.source, ComposeSource::Database(_)) {
        Some(
            database_services::deployment_for(db, secrets, project.id)
                .await
                .map_err(|e| CoreError::External(format!("loading database settings: {e}")))?,
        )
    } else {
        None
    };
    let (rollback, rollback_commit) = rollback.unzip();
    let result = agent
        .deploy_compose_project(DeployComposeProjectParams {
            project_id: project.id,
            deployment_id: job.id,
            project_slug: project.slug,
            source: project.source,
            pull_images: job.pull_images && rollback.is_none(),
            domains: domains
                .into_iter()
                .map(|d| ComposeDomainBinding {
                    domain: d.domain,
                    service_name: d.service_name,
                    container_port: d.container_port,
                    host_port: d.host_port,
                    https: d.https,
                })
                .collect(),
            acme_email,
            git_credentials,
            database,
            rollback,
        })
        .await
        .map(|mut outcome| {
            if let Some(snapshot) = &mut outcome.snapshot {
                scrub_snapshot(snapshot, &sent_secrets);
            }
            // A rollback runs the target's code again.
            if let Some(commit) = rollback_commit.flatten() {
                outcome.commit_sha = Some(commit);
            }
            outcome
        });
    if has_registry {
        if let Err(error) = agent
            .write_docker_config(WriteDockerConfigParams {
                project_slug: slug,
                registries: Vec::new(),
            })
            .await
        {
            tracing::warn!(deployment_id = %job.id, %error, "removing registry credentials failed");
        }
    }
    result
}

/// The decrypted login of the project's private registry, if it has one.
async fn registry_login(
    db: &PgPool,
    secrets: &SecretBox,
    project_id: Uuid,
) -> CoreResult<Option<DockerRegistryAuth>> {
    let Some(registry_id) = db::registries::project_registry(db, project_id)
        .await
        .map_err(|e| CoreError::External(format!("loading project registry: {e}")))?
    else {
        return Ok(None);
    };
    let row = db::registries::find(db, registry_id)
        .await
        .map_err(|e| CoreError::External(format!("loading registry: {e}")))?
        .ok_or_else(|| CoreError::NotFound("the project's registry no longer exists".into()))?;
    registries::open(secrets, &row)
        .map(Some)
        // The error names no secret: it comes from the AEAD check.
        .map_err(|e| CoreError::External(format!("unsealing registry credential: {e}")))
}

/// Mints a GitHub App installation token for a linked project. The token
/// lives for one deployment: it goes to the agent with the request and is
/// never stored here.
async fn github_app_credentials(
    db: &PgPool,
    github: &GitHubApps,
    project: &panel_core::types::ComposeProject,
) -> CoreResult<Option<GitHttpCredentials>> {
    let access = db::compose_projects::git_access(db, project.id)
        .await
        .map_err(|e| CoreError::External(format!("loading Git access: {e}")))?;
    let (Some(app_id), Some(installation_id)) =
        (access.github_app_id, access.github_installation_id)
    else {
        return Ok(None);
    };
    let repository = project
        .source
        .git_checkout()
        .and_then(|(url, _)| github_app::github_repository(url))
        .ok_or_else(|| {
            CoreError::Validation(
                "GitHub App tokens need an https://github.com/<owner>/<repo> remote".into(),
            )
        })?;
    let app = db::github_apps::find_sealed(db, app_id)
        .await
        .map_err(|e| CoreError::External(format!("loading GitHub App: {e}")))?
        .ok_or_else(|| CoreError::NotFound("the project's GitHub App no longer exists".into()))?;
    let token = github
        .installation_token(&app, installation_id, &repository)
        .await
        .map_err(|e| CoreError::External(format!("GitHub App installation token: {e}")))?;
    Ok(Some(GitHttpCredentials {
        username: github_app::TOKEN_USERNAME.into(),
        token,
    }))
}

/// Masks every value this job sent the agent in the snapshot's text
/// fields. The agent already sanitized it; this keeps the guarantee even
/// if an older or faulty agent did not.
fn scrub_snapshot(snapshot: &mut DeploymentSnapshot, values: &[String]) {
    const MASK: &str = "********";
    let mut values: Vec<&str> = values
        .iter()
        .map(String::as_str)
        .filter(|v| v.len() >= 4)
        .collect();
    values.sort_by_key(|v| std::cmp::Reverse(v.len()));
    let scrub = |text: &mut String| {
        for value in &values {
            if text.contains(value) {
                *text = text.replace(value, MASK);
            }
        }
    };
    scrub(&mut snapshot.compose_file);
    if let Some(body) = &mut snapshot.override_file {
        scrub(body);
    }
}

/// Truncates on a character boundary (slicing bytes could panic).
fn truncate(message: &str, max: usize) -> &str {
    match message.char_indices().nth(max) {
        Some((index, _)) => &message[..index],
        None => message,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::compose_projects;
    use panel_core::adapters::mock::MockAdapter;
    use panel_core::types::ComposeSource;

    use crate::test_support::{self, QUEUE_TESTS};

    use crate::test_support::test_pool;

    fn test_secrets() -> SecretBox {
        SecretBox::derive(&[42u8; 32])
    }

    async fn project(pool: &PgPool) -> panel_core::types::ComposeProject {
        let owner = crate::test_support::user(pool, true).await;
        let slug = format!("p{}", &Uuid::new_v4().simple().to_string()[..12]);
        compose_projects::create(
            pool,
            Uuid::new_v4(),
            "Test",
            &slug,
            owner.id,
            None,
            &ComposeSource::Raw {
                compose_yaml: "services:\n  web:\n    image: nginx".into(),
            },
        )
        .await
        .unwrap()
    }

    /// Claims until this project's queued job comes up; leftovers from
    /// earlier runs in the shared test database are completed on the way.
    async fn claim_for(pool: &PgPool, project_id: Uuid) -> Option<ClaimedDeployment> {
        while let Some(job) = compose_projects::claim_next_deployment(pool).await.unwrap() {
            if job.project_id == project_id {
                return Some(job);
            }
            compose_projects::finish_deployment(pool, job.id, DeploymentStatus::Failed, None, None)
                .await
                .unwrap();
        }
        None
    }

    #[test]
    fn truncate_respects_char_boundaries() {
        assert_eq!(truncate("çğüş", 2), "çğ");
        assert_eq!(truncate("abc", 10), "abc");
    }

    #[tokio::test]
    async fn project_deployments_run_in_order_one_at_a_time() {
        let _serial = QUEUE_TESTS.lock().await;
        let pool = test_pool().await;
        let project = project(&pool).await;
        let queue = DeployQueue::new();
        let first = queue
            .enqueue(&pool, project.id, &project.slug, false, "manual")
            .await
            .unwrap();
        let second = queue
            .enqueue(&pool, project.id, &project.slug, false, "manual")
            .await
            .unwrap();
        assert_eq!(first.status, DeploymentStatus::Queued);
        assert!(first.started_at.is_none());

        let claimed = claim_for(&pool, project.id).await.expect("first job");
        assert_eq!(claimed.id, first.id);
        // The second job must wait while the first one runs.
        assert!(claim_for(&pool, project.id).await.is_none());

        run_job(
            &pool,
            &MockAdapter::new(),
            &test_secrets(),
            &test_support::github_apps(),
            claimed,
        )
        .await;
        let done = compose_projects::find_deployment(&pool, project.id, first.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(done.status, DeploymentStatus::Succeeded);
        // Still queued behind it, so the project reports "queued".
        let reloaded = compose_projects::find_by_id(&pool, project.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(reloaded.status, DeploymentStatus::Queued);

        let next = claim_for(&pool, project.id).await.expect("second job");
        assert_eq!(next.id, second.id);
    }

    #[tokio::test]
    async fn github_app_tokens_are_minted_per_deployment_and_never_stored() {
        use crate::services::github_app::{tests::mock_github, TEST_KEY_PKCS1};
        use crate::services::secret_box::SecretBox;

        let _serial = QUEUE_TESTS.lock().await;
        let pool = test_pool().await;
        let owner = test_support::user(&pool, true).await;
        let slug = format!("gh{}", &Uuid::new_v4().simple().to_string()[..12]);
        let project = compose_projects::create(
            &pool,
            Uuid::new_v4(),
            "Private",
            &slug,
            owner.id,
            None,
            &ComposeSource::Git {
                repository_url: "https://github.com/acme/private-shop.git".into(),
                branch: "main".into(),
                compose_path: "docker-compose.yml".into(),
            },
        )
        .await
        .unwrap();
        let github =
            GitHubApps::with_api_base(SecretBox::derive(&[42u8; 32]), &mock_github(99).await);
        let app = Uuid::new_v4();
        let sealed = github.seal_private_key(app, TEST_KEY_PKCS1);
        db::github_apps::create(&pool, app, &format!("app-{slug}"), 1234, &sealed, owner.id)
            .await
            .unwrap();
        compose_projects::set_github_app(&pool, project.id, Some((app, 99)))
            .await
            .unwrap();

        let queue = DeployQueue::new();
        let queued = queue
            .enqueue(&pool, project.id, &project.slug, false, "manual")
            .await
            .unwrap();
        let job = claim_for(&pool, project.id).await.unwrap();
        let adapter = MockAdapter::new();
        run_job(&pool, &adapter, &test_secrets(), &github, job).await;
        let sent = adapter
            .deploys
            .lock()
            .unwrap()
            .pop()
            .expect("agent was called");
        let credentials = sent
            .git_credentials
            .clone()
            .expect("token sent with the job");
        assert_eq!(credentials.username, "x-access-token");
        assert_eq!(credentials.token, "ghs_minted_for_private-shop");
        assert!(
            !format!("{sent:?}").contains("ghs_"),
            "Debug must mask the token"
        );
        for table in ["compose_deployments", "compose_projects", "audit_log"] {
            let rows: Vec<String> =
                sqlx::query_scalar(&format!("SELECT row_to_json(t)::text FROM {table} t"))
                    .fetch_all(&pool)
                    .await
                    .unwrap();
            assert!(rows.iter().all(|r| !r.contains("ghs_minted")), "{table}");
        }
        let done = compose_projects::find_deployment(&pool, project.id, queued.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(done.status, DeploymentStatus::Succeeded);

        // An installation GitHub refuses fails the job before the agent runs.
        compose_projects::set_github_app(&pool, project.id, Some((app, 98)))
            .await
            .unwrap();
        let queued = queue
            .enqueue(&pool, project.id, &project.slug, false, "manual")
            .await
            .unwrap();
        let job = claim_for(&pool, project.id).await.unwrap();
        let adapter = MockAdapter::new();
        run_job(&pool, &adapter, &test_secrets(), &github, job).await;
        assert!(adapter.deploys.lock().unwrap().is_empty());
        let failed = compose_projects::find_deployment(&pool, project.id, queued.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(failed.status, DeploymentStatus::Failed);
        let message = failed.error_message.unwrap();
        assert!(
            message.contains("GitHub App installation token") && message.contains("HTTP 404"),
            "{message}"
        );

        compose_projects::set_github_app(&pool, project.id, None)
            .await
            .unwrap();
        db::github_apps::delete(&pool, app).await.unwrap();
    }

    #[tokio::test]
    async fn failed_agent_call_marks_deployment_and_project_failed() {
        let _serial = QUEUE_TESTS.lock().await;
        let pool = test_pool().await;
        let project = project(&pool).await;
        let queue = DeployQueue::new();
        let queued = queue
            .enqueue(&pool, project.id, &project.slug, false, "manual")
            .await
            .unwrap();
        let job = claim_for(&pool, project.id).await.unwrap();
        let adapter = MockAdapter::new();
        adapter.fail_next_call("deploy_compose_project");
        run_job(
            &pool,
            &adapter,
            &test_secrets(),
            &test_support::github_apps(),
            job,
        )
        .await;

        let failed = compose_projects::find_deployment(&pool, project.id, queued.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(failed.status, DeploymentStatus::Failed);
        assert!(failed.error_message.unwrap().contains("mock failure"));
        assert!(failed.finished_at.is_some());
        let reloaded = compose_projects::find_by_id(&pool, project.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(reloaded.status, DeploymentStatus::Failed);
    }

    #[tokio::test]
    async fn registry_credentials_bracket_the_deployment_even_when_it_fails() {
        let _serial = QUEUE_TESTS.lock().await;
        let pool = test_pool().await;
        let secrets = test_secrets();
        for fail in [false, true] {
            let project = project(&pool).await;
            let registry_id = Uuid::new_v4();
            crate::db::registries::create(
                &pool,
                registry_id,
                &format!("reg-{registry_id}"),
                "ghcr.io",
                "octocat",
                &crate::services::registries::seal(&secrets, registry_id, "pw"),
            )
            .await
            .unwrap();
            crate::db::registries::set_project_registry(&pool, project.id, Some(registry_id))
                .await
                .unwrap();
            DeployQueue::new()
                .enqueue(&pool, project.id, &project.slug, true, "manual")
                .await
                .unwrap();
            let job = claim_for(&pool, project.id).await.unwrap();
            let adapter = MockAdapter::new();
            if fail {
                adapter.fail_next_call("deploy_compose_project");
            }
            run_job(&pool, &adapter, &secrets, &test_support::github_apps(), job).await;
            assert_eq!(
                *adapter.calls.lock().unwrap(),
                [
                    "write_compose_environment",
                    "write_docker_config",
                    "deploy_compose_project",
                    "write_docker_config"
                ]
            );
        }
    }

    #[tokio::test]
    async fn projects_without_a_registry_never_touch_docker_config() {
        let _serial = QUEUE_TESTS.lock().await;
        let pool = test_pool().await;
        let project = project(&pool).await;
        DeployQueue::new()
            .enqueue(&pool, project.id, &project.slug, false, "manual")
            .await
            .unwrap();
        let job = claim_for(&pool, project.id).await.unwrap();
        let adapter = MockAdapter::new();
        run_job(
            &pool,
            &adapter,
            &test_secrets(),
            &test_support::github_apps(),
            job,
        )
        .await;
        assert_eq!(
            *adapter.calls.lock().unwrap(),
            ["write_compose_environment", "deploy_compose_project"]
        );
    }

    #[tokio::test]
    async fn agent_cancellation_records_cancelled_status() {
        let _serial = QUEUE_TESTS.lock().await;
        let pool = test_pool().await;
        let project = project(&pool).await;
        let queued = DeployQueue::new()
            .enqueue(&pool, project.id, &project.slug, false, "manual")
            .await
            .unwrap();
        let job = claim_for(&pool, project.id).await.unwrap();
        let adapter = MockAdapter::new();
        adapter.fail_next_call_with(
            "deploy_compose_project",
            CoreError::Cancelled("deployment cancelled".into()),
        );
        run_job(
            &pool,
            &adapter,
            &test_secrets(),
            &test_support::github_apps(),
            job,
        )
        .await;
        let cancelled = compose_projects::find_deployment(&pool, project.id, queued.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(cancelled.status, DeploymentStatus::Cancelled);
        let reloaded = compose_projects::find_by_id(&pool, project.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(reloaded.status, DeploymentStatus::Cancelled);
    }

    #[tokio::test]
    async fn queued_deployment_can_be_cancelled_but_running_cannot() {
        let _serial = QUEUE_TESTS.lock().await;
        let pool = test_pool().await;
        let project = project(&pool).await;
        let queue = DeployQueue::new();
        let running = queue
            .enqueue(&pool, project.id, &project.slug, false, "manual")
            .await
            .unwrap();
        let queued = queue
            .enqueue(&pool, project.id, &project.slug, false, "manual")
            .await
            .unwrap();
        claim_for(&pool, project.id).await.unwrap();
        assert!(
            !compose_projects::cancel_queued_deployment(&pool, project.id, running.id)
                .await
                .unwrap()
        );
        assert!(
            compose_projects::cancel_queued_deployment(&pool, project.id, queued.id)
                .await
                .unwrap()
        );
        let cancelled = compose_projects::find_deployment(&pool, project.id, queued.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(cancelled.status, DeploymentStatus::Cancelled);
    }

    #[tokio::test]
    async fn deployment_writes_the_merged_layers_to_the_agent_first() {
        use crate::db::projects::EnvScope;
        let _serial = QUEUE_TESTS.lock().await;
        let pool = test_pool().await;
        let secrets = test_secrets();
        let project = project(&pool).await;
        let (environment_id, project_scope_id) =
            crate::db::projects::service_scopes(&pool, project.id)
                .await
                .unwrap()
                .unwrap();
        let set = |scope, items: &[(&str, &str)]| {
            let pairs: Vec<(String, String)> = items
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
            let (pool, secrets) = (pool.clone(), secrets.clone());
            async move { env_layers::replace(&pool, &secrets, scope, &pairs).await }
        };
        set(
            EnvScope::Project(project_scope_id),
            &[
                ("SHARED", "from-project-xx"),
                ("ONLY_PROJECT", "p-value-xx"),
            ],
        )
        .await
        .unwrap();
        set(
            EnvScope::Environment(environment_id),
            &[("SHARED", "from-env-xx"), ("ONLY_ENV", "e-value-xx")],
        )
        .await
        .unwrap();
        set(
            EnvScope::Service(project.id),
            &[("SHARED", "from-service-xx")],
        )
        .await
        .unwrap();

        let queued = DeployQueue::new()
            .enqueue(&pool, project.id, &project.slug, false, "manual")
            .await
            .unwrap();
        let job = claim_for(&pool, project.id).await.unwrap();
        let adapter = MockAdapter::new();
        run_job(&pool, &adapter, &secrets, &test_support::github_apps(), job).await;

        let calls = adapter.calls.lock().unwrap().clone();
        assert_eq!(
            calls,
            vec!["write_compose_environment", "deploy_compose_project"]
        );
        let writes = adapter.environment_writes.lock().unwrap().clone();
        let mut env = writes[0].environment.clone();
        env.sort();
        assert_eq!(
            env,
            vec![
                ("ONLY_ENV".to_string(), "e-value-xx".to_string()),
                ("ONLY_PROJECT".to_string(), "p-value-xx".to_string()),
                ("SHARED".to_string(), "from-service-xx".to_string()),
            ]
        );
        let mut masked = writes[0].masked_values.clone();
        masked.sort();
        assert_eq!(masked, vec!["from-env-xx", "from-project-xx"]);
        // Debug output (what a stray log line would print) has no values.
        let debug = format!("{:?}", writes[0]);
        assert!(!debug.contains("-xx"), "{debug}");

        let row: String =
            sqlx::query_scalar("SELECT to_jsonb(d)::text FROM compose_deployments d WHERE id = $1")
                .bind(queued.id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(!row.contains("-xx"), "{row}");
    }

    #[tokio::test]
    async fn legacy_services_keep_their_agent_env_file() {
        let _serial = QUEUE_TESTS.lock().await;
        let pool = test_pool().await;
        let project = project(&pool).await;
        sqlx::query("UPDATE compose_projects SET env_layered = FALSE WHERE id = $1")
            .bind(project.id)
            .execute(&pool)
            .await
            .unwrap();
        DeployQueue::new()
            .enqueue(&pool, project.id, &project.slug, false, "manual")
            .await
            .unwrap();
        let job = claim_for(&pool, project.id).await.unwrap();
        let adapter = MockAdapter::new();
        run_job(
            &pool,
            &adapter,
            &test_secrets(),
            &test_support::github_apps(),
            job,
        )
        .await;
        assert_eq!(
            adapter.calls.lock().unwrap().clone(),
            vec!["deploy_compose_project"]
        );
    }

    /// Deploys once through the worker and returns the finished record.
    async fn deploy_once(
        pool: &PgPool,
        adapter: &MockAdapter,
        secrets: &SecretBox,
        project: &panel_core::types::ComposeProject,
    ) -> DeploymentInfo {
        let queued = DeployQueue::new()
            .enqueue(pool, project.id, &project.slug, true, "manual")
            .await
            .unwrap();
        let job = claim_for(pool, project.id).await.unwrap();
        run_job(pool, adapter, secrets, &test_support::github_apps(), job).await;
        compose_projects::find_deployment(pool, project.id, queued.id)
            .await
            .unwrap()
            .unwrap()
    }

    #[tokio::test]
    async fn rollbacks_restart_a_pinned_release_without_fetching_or_storing_secrets() {
        use crate::db::projects::EnvScope;
        let _serial = QUEUE_TESTS.lock().await;
        let pool = test_pool().await;
        let secrets = test_secrets();
        let project = project(&pool).await;
        env_layers::replace(
            &pool,
            &secrets,
            EnvScope::Service(project.id),
            &[("DB_PASSWORD".into(), "rollback-secret-xx".into())],
        )
        .await
        .unwrap();
        let registry_id = Uuid::new_v4();
        db::registries::create(
            &pool,
            registry_id,
            &format!("reg-{registry_id}"),
            "ghcr.io",
            "octocat",
            &registries::seal(&secrets, registry_id, "registry-pass-xx"),
        )
        .await
        .unwrap();
        db::registries::set_project_registry(&pool, project.id, Some(registry_id))
            .await
            .unwrap();

        let adapter = MockAdapter::new();
        // An agent that failed to sanitize: the worker masks what it sent.
        *adapter.next_snapshot.lock().unwrap() = Some(DeploymentSnapshot {
            compose_path: "docker-compose.yml".into(),
            compose_sha256: "c".repeat(64),
            compose_file: "services:\n  web:\n    environment:\n      DB_PASSWORD: rollback-secret-xx\n      REG: registry-pass-xx\n".into(),
            override_file: Some("# rollback-secret-xx\n".into()),
            env_keys: vec!["DB_PASSWORD".into()],
            images: vec![],
        });
        let first = deploy_once(&pool, &adapter, &secrets, &project).await;
        assert_eq!(first.status, DeploymentStatus::Succeeded);
        assert!(first.rollback_available);
        let stored = compose_projects::find_release(&pool, project.id, first.id)
            .await
            .unwrap()
            .unwrap()
            .snapshot
            .unwrap();
        assert_eq!(stored.env_keys, ["DB_PASSWORD"]);
        assert!(stored.compose_file.contains("DB_PASSWORD: ********"));
        let second = deploy_once(&pool, &adapter, &secrets, &project).await;
        assert!(second.rollback_available);
        for table in ["compose_deployments", "audit_log"] {
            let rows: Vec<String> =
                sqlx::query_scalar(&format!("SELECT row_to_json(t)::text FROM {table} t"))
                    .fetch_all(&pool)
                    .await
                    .unwrap();
            assert!(
                rows.iter()
                    .all(|r| !r.contains("rollback-secret-xx") && !r.contains("registry-pass-xx")),
                "{table}"
            );
        }

        let queue = DeployQueue::new();
        let queued = queue
            .enqueue_rollback(&pool, &project, first.id)
            .await
            .unwrap();
        assert_eq!(queued.rollback_of, Some(first.id));
        assert_eq!(queued.trigger, "rollback");
        let job = claim_for(&pool, project.id).await.unwrap();
        assert_eq!(job.rollback_of, Some(first.id));
        adapter.calls.lock().unwrap().clear();
        run_job(&pool, &adapter, &secrets, &test_support::github_apps(), job).await;
        // The current environment is written; no registry login is made.
        assert_eq!(
            *adapter.calls.lock().unwrap(),
            ["write_compose_environment", "deploy_compose_project"]
        );
        let sent = adapter.deploys.lock().unwrap().pop().unwrap();
        let target = sent.rollback.expect("rollback target sent");
        assert_eq!(target.deployment_id, first.id);
        assert_eq!(target.compose_sha256, "c".repeat(64));
        assert!(!sent.pull_images && sent.git_credentials.is_none());
        let done = compose_projects::find_deployment(&pool, project.id, queued.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(done.status, DeploymentStatus::Succeeded);
        // A rollback records no release of its own; both targets stay.
        assert!(!done.rollback_available);
        for id in [first.id, second.id] {
            let info = compose_projects::find_deployment(&pool, project.id, id)
                .await
                .unwrap()
                .unwrap();
            assert!(info.rollback_available);
        }

        // Targets without a kept release, failed ones, and foreign ones.
        compose_projects::record_releases(&pool, project.id, done.id, None, &[first.id])
            .await
            .unwrap();
        assert!(matches!(
            queue.enqueue_rollback(&pool, &project, second.id).await,
            Err(ApiError::Conflict(_))
        ));
        assert!(matches!(
            queue.enqueue_rollback(&pool, &project, done.id).await,
            Err(ApiError::Conflict(_))
        ));
        adapter.fail_next_call("deploy_compose_project");
        let failed = deploy_once(&pool, &adapter, &secrets, &project).await;
        assert_eq!(failed.status, DeploymentStatus::Failed);
        assert!(matches!(
            queue.enqueue_rollback(&pool, &project, failed.id).await,
            Err(ApiError::Conflict(_))
        ));
        let other = super::tests::project(&pool).await;
        assert!(matches!(
            queue.enqueue_rollback(&pool, &other, first.id).await,
            Err(ApiError::NotFound(_))
        ));
        db::registries::set_project_registry(&pool, project.id, None)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn rollbacks_refuse_sources_without_releases() {
        let pool = test_pool().await;
        let mut project = project(&pool).await;
        project.source = ComposeSource::Static(Box::new(panel_core::types::StaticSiteSpec {
            repository_url: "https://github.com/acme/site.git".into(),
            branch: "main".into(),
            publish_path: "dist".into(),
            build: None,
            spa: false,
        }));
        assert!(matches!(
            DeployQueue::new()
                .enqueue_rollback(&pool, &project, Uuid::new_v4())
                .await,
            Err(ApiError::BadRequest(_))
        ));
    }

    #[tokio::test]
    async fn domains_get_distinct_loopback_ports() {
        let pool = test_pool().await;
        let project = project(&pool).await;
        let a = format!("a{}.example.com", Uuid::new_v4().simple());
        let b = format!("b{}.example.com", Uuid::new_v4().simple());
        let first = compose_projects::create_domain(&pool, project.id, &a, "web", 3000, false)
            .await
            .unwrap()
            .unwrap();
        let second = compose_projects::create_domain(&pool, project.id, &b, "web", 3000, true)
            .await
            .unwrap()
            .unwrap();
        assert_ne!(first.host_port, second.host_port);
        assert!((40000..=49999).contains(&first.host_port));
        assert!(compose_projects::domain_in_use(&pool, &a).await.unwrap());
        assert!(compose_projects::delete_domain(&pool, project.id, first.id)
            .await
            .unwrap());
        assert!(!compose_projects::domain_in_use(&pool, &a).await.unwrap());
    }
}
