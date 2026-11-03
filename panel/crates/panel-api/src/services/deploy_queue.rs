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
//! Job rows never contain environment values: those are written to the
//! agent before the job is queued.

use crate::db;
use crate::db::compose_projects::ClaimedDeployment;
use crate::error::ApiResult;
use crate::services::notifications::Notifier;
use panel_core::adapters::{AdapterPort, DeployComposeProjectParams};
use panel_core::error::{CoreError, CoreResult};
use panel_core::types::{
    ComposeDeployOutcome, ComposeDomainBinding, DeploymentInfo, DeploymentStatus,
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
        workers: usize,
    ) {
        for worker in 0..workers.max(1) {
            let (db, agent, wake) = (db.clone(), agent.clone(), self.wake.clone());
            let notifier = notifier.clone();
            tokio::spawn(async move { worker_loop(worker, db, agent, notifier, wake).await });
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
        let id = Uuid::new_v4();
        let log_path = format!("agent://compose/{project_slug}/logs/{id}.log");
        let deployment = db::compose_projects::enqueue_deployment(
            db,
            id,
            project_id,
            &log_path,
            pull_images,
            trigger,
        )
        .await?;
        // Wake idle workers now; the stored permit covers a worker that is
        // between "found nothing" and "waiting".
        self.wake.notify_waiters();
        self.wake.notify_one();
        Ok(deployment)
    }
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
    wake: Arc<Notify>,
) {
    loop {
        match db::compose_projects::claim_next_deployment(&db).await {
            Ok(Some(job)) => {
                tracing::info!(worker, deployment_id = %job.id, project_id = %job.project_id, "deployment started");
                let (project_id, deployment_id) = (job.project_id, job.id);
                run_job(&db, agent.as_ref(), job).await;
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

pub(crate) async fn run_job(db: &PgPool, agent: &dyn AdapterPort, job: ClaimedDeployment) {
    let result = execute(db, agent, &job).await;
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
    job: &ClaimedDeployment,
) -> CoreResult<ComposeDeployOutcome> {
    let project = db::compose_projects::find_by_id(db, job.project_id)
        .await
        .map_err(|e| CoreError::External(format!("loading project: {e}")))?
        .ok_or_else(|| CoreError::NotFound("project no longer exists".into()))?;
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
    agent
        .deploy_compose_project(DeployComposeProjectParams {
            project_id: project.id,
            deployment_id: job.id,
            project_slug: project.slug,
            source: project.source,
            pull_images: job.pull_images,
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
        })
        .await
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

    use crate::test_support::QUEUE_TESTS;

    use crate::test_support::test_pool;

    async fn project(pool: &PgPool) -> panel_core::types::ComposeProject {
        let owner = crate::test_support::user(pool, true).await;
        let slug = format!("p{}", &Uuid::new_v4().simple().to_string()[..12]);
        compose_projects::create(
            pool,
            Uuid::new_v4(),
            "Test",
            &slug,
            owner.id,
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

        run_job(&pool, &MockAdapter::new(), claimed).await;
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
        run_job(&pool, &adapter, job).await;

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
        run_job(&pool, &adapter, job).await;
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
