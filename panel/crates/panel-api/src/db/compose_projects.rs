use chrono::{DateTime, Utc};
use panel_core::types::{
    ComposeDomain, ComposeProject, ComposeSource, DeploymentInfo, DeploymentSnapshot,
    DeploymentStatus,
};
use sqlx::PgPool;
use std::str::FromStr;
use uuid::Uuid;

#[derive(sqlx::FromRow)]
struct ProjectRecord {
    id: Uuid,
    name: String,
    slug: String,
    owner_user_id: Uuid,
    environment_id: Uuid,
    env_layered: bool,
    server_id: Uuid,
    source: serde_json::Value,
    status: String,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl ProjectRecord {
    fn into_project(self) -> anyhow::Result<ComposeProject> {
        Ok(ComposeProject {
            id: self.id,
            name: self.name,
            slug: self.slug,
            owner_user_id: self.owner_user_id,
            environment_id: self.environment_id,
            env_layered: self.env_layered,
            server_id: self.server_id,
            source: serde_json::from_value(self.source)?,
            status: DeploymentStatus::from_str(&self.status).map_err(anyhow::Error::msg)?,
            created_at: self.created_at,
            updated_at: self.updated_at,
        })
    }
}

#[derive(sqlx::FromRow)]
struct DeploymentRecord {
    id: Uuid,
    project_id: Uuid,
    status: String,
    log_path: String,
    trigger: String,
    commit_sha: Option<String>,
    queued_at: DateTime<Utc>,
    started_at: Option<DateTime<Utc>>,
    finished_at: Option<DateTime<Utc>>,
    error_message: Option<String>,
    rollback_of: Option<Uuid>,
    rollback_available: bool,
}

impl DeploymentRecord {
    fn into_info(self) -> anyhow::Result<DeploymentInfo> {
        Ok(DeploymentInfo {
            id: self.id,
            project_id: self.project_id,
            status: DeploymentStatus::from_str(&self.status).map_err(anyhow::Error::msg)?,
            log_path: self.log_path,
            trigger: self.trigger,
            commit_sha: self.commit_sha,
            queued_at: self.queued_at,
            started_at: self.started_at,
            finished_at: self.finished_at,
            error_message: self.error_message,
            rollback_of: self.rollback_of,
            rollback_available: self.rollback_available,
        })
    }
}

#[derive(sqlx::FromRow)]
struct DomainRecord {
    id: Uuid,
    project_id: Uuid,
    domain: String,
    service_name: String,
    container_port: i32,
    host_port: i32,
    https: bool,
    created_at: DateTime<Utc>,
}

impl DomainRecord {
    fn into_domain(self) -> anyhow::Result<ComposeDomain> {
        Ok(ComposeDomain {
            id: self.id,
            project_id: self.project_id,
            domain: self.domain,
            service_name: self.service_name,
            container_port: u16::try_from(self.container_port)?,
            host_port: u16::try_from(self.host_port)?,
            https: self.https,
            created_at: self.created_at,
        })
    }
}

/// A queued deployment claimed by a worker (now `running`).
pub struct ClaimedDeployment {
    pub id: Uuid,
    pub project_id: Uuid,
    pub pull_images: bool,
    /// Set for a rollback: the deployment whose release it starts again.
    pub rollback_of: Option<Uuid>,
}

pub async fn create(
    pool: &PgPool,
    id: Uuid,
    name: &str,
    slug: &str,
    owner_user_id: Uuid,
    environment_id: Option<Uuid>,
    source: &ComposeSource,
) -> anyhow::Result<ComposeProject> {
    let environment_id = match environment_id {
        Some(id) => id,
        None => super::projects::default_environment(pool, owner_user_id).await?,
    };
    let source = serde_json::to_value(source)?;
    // A service created by the panel starts layered: the panel holds its
    // own variables from the first write on.
    let record = sqlx::query_as!(
        ProjectRecord,
        r#"INSERT INTO compose_projects
               (id, name, slug, owner_user_id, environment_id, env_layered, source)
           VALUES ($1, $2, $3, $4, $5, TRUE, $6)
           RETURNING id, name, slug, owner_user_id, environment_id, env_layered, server_id, source, status,
                     created_at, updated_at"#,
        id,
        name,
        slug,
        owner_user_id,
        environment_id,
        source,
    )
    .fetch_one(pool)
    .await?;
    record.into_project()
}

pub async fn find_by_id(pool: &PgPool, id: Uuid) -> anyhow::Result<Option<ComposeProject>> {
    let record = sqlx::query_as!(
        ProjectRecord,
        "SELECT id, name, slug, owner_user_id, environment_id, env_layered, server_id, source, status, created_at, updated_at FROM compose_projects WHERE id = $1",
        id,
    )
    .fetch_optional(pool)
    .await?;
    record.map(ProjectRecord::into_project).transpose()
}

pub async fn list_all(pool: &PgPool) -> anyhow::Result<Vec<ComposeProject>> {
    let records = sqlx::query_as!(
        ProjectRecord,
        "SELECT id, name, slug, owner_user_id, environment_id, env_layered, server_id, source, status, created_at, updated_at FROM compose_projects ORDER BY created_at DESC",
    )
    .fetch_all(pool)
    .await?;
    records
        .into_iter()
        .map(ProjectRecord::into_project)
        .collect()
}

pub async fn set_status(pool: &PgPool, id: Uuid, status: DeploymentStatus) -> sqlx::Result<()> {
    sqlx::query!(
        "UPDATE compose_projects SET status = $1, updated_at = now() WHERE id = $2",
        status.to_string(),
        id
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn delete(pool: &PgPool, id: Uuid) -> sqlx::Result<()> {
    sqlx::query!("DELETE FROM compose_projects WHERE id = $1", id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Queues a deployment job. The row is the job: it survives restarts.
pub async fn enqueue_deployment(
    pool: &PgPool,
    id: Uuid,
    project_id: Uuid,
    log_path: &str,
    pull_images: bool,
    trigger: &str,
    rollback_of: Option<Uuid>,
) -> anyhow::Result<DeploymentInfo> {
    let mut tx = pool.begin().await?;
    let record = sqlx::query_as!(
        DeploymentRecord,
        r#"INSERT INTO compose_deployments
               (id, project_id, status, log_path, pull_images, trigger, rollback_of)
           VALUES ($1, $2, 'queued', $3, $4, $5, $6)
           RETURNING id, project_id, status, log_path, trigger, commit_sha, queued_at,
                     started_at, finished_at, error_message, rollback_of,
                     (status = 'succeeded' AND snapshot IS NOT NULL AND release_retained) AS "rollback_available!""#,
        id,
        project_id,
        log_path,
        pull_images,
        trigger,
        rollback_of,
    )
    .fetch_one(&mut *tx)
    .await?;
    // Only an idle project shows "queued"; a running one keeps its status.
    sqlx::query!(
        "UPDATE compose_projects SET status = 'queued', updated_at = now() WHERE id = $1 AND status <> 'running'",
        project_id
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    record.into_info()
}

/// Claims the oldest queued deployment whose project has nothing running,
/// so deployments of one project run strictly in order while different
/// projects run in parallel. Claims are serialised with a transaction-scoped
/// advisory lock, which keeps the "nothing running" check race-free even
/// with several workers (or several panel-api processes).
pub async fn claim_next_deployment(pool: &PgPool) -> sqlx::Result<Option<ClaimedDeployment>> {
    let mut tx = pool.begin().await?;
    sqlx::query!("SELECT pg_advisory_xact_lock(hashtext('scontrol.compose_deploy_claim'))")
        .execute(&mut *tx)
        .await?;
    let claimed = sqlx::query!(
        r#"UPDATE compose_deployments
           SET status = 'running', started_at = now()
           WHERE id = (
               SELECT d.id FROM compose_deployments d
               WHERE d.status = 'queued'
                 AND NOT EXISTS (
                     SELECT 1 FROM compose_deployments r
                     WHERE r.project_id = d.project_id AND r.status = 'running'
                 )
               ORDER BY d.queued_at
               LIMIT 1
               FOR UPDATE SKIP LOCKED
           )
           RETURNING id, project_id, pull_images, rollback_of"#
    )
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(row) = &claimed {
        sqlx::query!(
            "UPDATE compose_projects SET status = 'running', updated_at = now() WHERE id = $1",
            row.project_id
        )
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(claimed.map(|row| ClaimedDeployment {
        id: row.id,
        project_id: row.project_id,
        pull_images: row.pull_images,
        rollback_of: row.rollback_of,
    }))
}

/// Stores the snapshot of a deployment's release (if it recorded one) and
/// mirrors which releases the agent still keeps, in one transaction.
pub async fn record_releases(
    pool: &PgPool,
    project_id: Uuid,
    deployment_id: Uuid,
    snapshot: Option<&DeploymentSnapshot>,
    retained: &[Uuid],
) -> anyhow::Result<()> {
    let mut tx = pool.begin().await?;
    if let Some(snapshot) = snapshot {
        sqlx::query!(
            "UPDATE compose_deployments SET snapshot = $1 WHERE id = $2 AND project_id = $3",
            serde_json::to_value(snapshot)?,
            deployment_id,
            project_id,
        )
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query!(
        r#"UPDATE compose_deployments SET release_retained = (id = ANY($2))
           WHERE project_id = $1 AND (release_retained OR id = ANY($2))"#,
        project_id,
        retained,
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// What a rollback needs from its target deployment.
pub struct ReleaseRecord {
    pub status: DeploymentStatus,
    pub commit_sha: Option<String>,
    pub snapshot: Option<DeploymentSnapshot>,
    pub retained: bool,
}

pub async fn find_release(
    pool: &PgPool,
    project_id: Uuid,
    deployment_id: Uuid,
) -> anyhow::Result<Option<ReleaseRecord>> {
    let row = sqlx::query!(
        r#"SELECT status, commit_sha, snapshot, release_retained
           FROM compose_deployments WHERE id = $1 AND project_id = $2"#,
        deployment_id,
        project_id,
    )
    .fetch_optional(pool)
    .await?;
    row.map(|row| {
        Ok(ReleaseRecord {
            status: DeploymentStatus::from_str(&row.status).map_err(anyhow::Error::msg)?,
            commit_sha: row.commit_sha,
            snapshot: row.snapshot.map(serde_json::from_value).transpose()?,
            retained: row.release_retained,
        })
    })
    .transpose()
}

/// Records a deployment's result and mirrors it onto the project, unless
/// more deployments of the project are already queued behind it.
pub async fn finish_deployment(
    pool: &PgPool,
    id: Uuid,
    status: DeploymentStatus,
    error_message: Option<&str>,
    commit_sha: Option<&str>,
) -> sqlx::Result<()> {
    let mut tx = pool.begin().await?;
    let project_id = sqlx::query_scalar!(
        r#"UPDATE compose_deployments
           SET status = $1, finished_at = now(), error_message = $2, commit_sha = $3
           WHERE id = $4
           RETURNING project_id"#,
        status.to_string(),
        error_message,
        commit_sha,
        id,
    )
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query!(
        r#"UPDATE compose_projects
           SET status = CASE WHEN EXISTS (
                   SELECT 1 FROM compose_deployments WHERE project_id = $2 AND status = 'queued'
               ) THEN 'queued' ELSE $1 END,
               updated_at = now()
           WHERE id = $2"#,
        status.to_string(),
        project_id,
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await
}

/// Cancels a deployment that has not started. Returns false if it is no
/// longer queued (already running or finished).
pub async fn cancel_queued_deployment(
    pool: &PgPool,
    project_id: Uuid,
    id: Uuid,
) -> sqlx::Result<bool> {
    let updated = sqlx::query!(
        r#"UPDATE compose_deployments
           SET status = 'cancelled', finished_at = now()
           WHERE id = $1 AND project_id = $2 AND status = 'queued'"#,
        id,
        project_id,
    )
    .execute(pool)
    .await?
    .rows_affected();
    if updated == 1 {
        sqlx::query!(
            r#"UPDATE compose_projects SET status = 'cancelled', updated_at = now()
               WHERE id = $1 AND status = 'queued' AND NOT EXISTS (
                   SELECT 1 FROM compose_deployments WHERE project_id = $1 AND status = 'queued'
               )"#,
            project_id,
        )
        .execute(pool)
        .await?;
    }
    Ok(updated == 1)
}

/// At startup, a `running` row means the previous panel-api process died
/// mid-deployment and nobody will record its result: mark it failed. Queued
/// rows stay queued and are picked up again.
pub async fn fail_interrupted_deployments(pool: &PgPool) -> sqlx::Result<u64> {
    let mut tx = pool.begin().await?;
    let failed = sqlx::query!(
        r#"UPDATE compose_deployments
           SET status = 'failed', finished_at = now(),
               error_message = 'interrupted: panel-api restarted during the deployment'
           WHERE status = 'running'"#
    )
    .execute(&mut *tx)
    .await?
    .rows_affected();
    sqlx::query!(
        r#"UPDATE compose_projects p
           SET status = CASE WHEN EXISTS (
                   SELECT 1 FROM compose_deployments d WHERE d.project_id = p.id AND d.status = 'queued'
               ) THEN 'queued' ELSE 'failed' END,
               updated_at = now()
           WHERE p.status = 'running'"#
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(failed)
}

pub async fn find_deployment(
    pool: &PgPool,
    project_id: Uuid,
    id: Uuid,
) -> anyhow::Result<Option<DeploymentInfo>> {
    let record = sqlx::query_as!(
        DeploymentRecord,
        r#"SELECT id, project_id, status, log_path, trigger, commit_sha, queued_at, started_at,
                  finished_at, error_message, rollback_of,
                  (status = 'succeeded' AND snapshot IS NOT NULL AND release_retained) AS "rollback_available!"
           FROM compose_deployments WHERE id = $1 AND project_id = $2"#,
        id,
        project_id,
    )
    .fetch_optional(pool)
    .await?;
    record.map(DeploymentRecord::into_info).transpose()
}

pub async fn list_deployments(
    pool: &PgPool,
    project_id: Uuid,
) -> anyhow::Result<Vec<DeploymentInfo>> {
    let records = sqlx::query_as!(
        DeploymentRecord,
        r#"SELECT id, project_id, status, log_path, trigger, commit_sha, queued_at, started_at,
                  finished_at, error_message, rollback_of,
                  (status = 'succeeded' AND snapshot IS NOT NULL AND release_retained) AS "rollback_available!"
           FROM compose_deployments WHERE project_id = $1
           ORDER BY queued_at DESC LIMIT 100"#,
        project_id,
    )
    .fetch_all(pool)
    .await?;
    records
        .into_iter()
        .map(DeploymentRecord::into_info)
        .collect()
}

pub async fn list_domains(pool: &PgPool, project_id: Uuid) -> anyhow::Result<Vec<ComposeDomain>> {
    let records = sqlx::query_as!(
        DomainRecord,
        r#"SELECT id, project_id, domain, service_name, container_port, host_port, https, created_at
           FROM compose_project_domains WHERE project_id = $1 ORDER BY created_at"#,
        project_id,
    )
    .fetch_all(pool)
    .await?;
    records.into_iter().map(DomainRecord::into_domain).collect()
}

/// True if a site or any Compose project already serves `domain`.
pub async fn domain_in_use(pool: &PgPool, domain: &str) -> sqlx::Result<bool> {
    let in_use = sqlx::query_scalar!(
        r#"SELECT (EXISTS (SELECT 1 FROM sites WHERE domain = $1)
               OR EXISTS (SELECT 1 FROM compose_project_domains WHERE domain = $1)) AS "in_use!""#,
        domain,
    )
    .fetch_one(pool)
    .await?;
    Ok(in_use)
}

/// Adds a binding with the lowest free loopback host port in 40000..=49999.
/// Returns `None` when the port range is exhausted.
pub async fn create_domain(
    pool: &PgPool,
    project_id: Uuid,
    domain: &str,
    service_name: &str,
    container_port: u16,
    https: bool,
) -> anyhow::Result<Option<ComposeDomain>> {
    let record = sqlx::query_as!(
        DomainRecord,
        r#"INSERT INTO compose_project_domains
               (id, project_id, domain, service_name, container_port, https, host_port)
           SELECT $1, $2, $3, $4, $5, $6, port
           FROM generate_series(40000, 49999) AS port
           WHERE NOT EXISTS (SELECT 1 FROM compose_project_domains WHERE host_port = port)
           ORDER BY port
           LIMIT 1
           RETURNING id, project_id, domain, service_name, container_port, host_port, https,
                     created_at"#,
        Uuid::new_v4(),
        project_id,
        domain,
        service_name,
        i32::from(container_port),
        https,
    )
    .fetch_optional(pool)
    .await?;
    record.map(DomainRecord::into_domain).transpose()
}

pub async fn delete_domain(pool: &PgPool, project_id: Uuid, id: Uuid) -> sqlx::Result<bool> {
    let deleted = sqlx::query!(
        "DELETE FROM compose_project_domains WHERE id = $1 AND project_id = $2",
        id,
        project_id,
    )
    .execute(pool)
    .await?
    .rows_affected();
    Ok(deleted == 1)
}

/// The project's webhook nonce, or `None` when its webhook is disabled.
pub async fn webhook_nonce(pool: &PgPool, project_id: Uuid) -> sqlx::Result<Option<String>> {
    let nonce = sqlx::query_scalar!(
        "SELECT webhook_nonce FROM compose_projects WHERE id = $1",
        project_id
    )
    .fetch_optional(pool)
    .await?;
    Ok(nonce.flatten())
}

/// Sets (enable/rotate) or clears (disable) the webhook nonce.
pub async fn set_webhook_nonce(
    pool: &PgPool,
    project_id: Uuid,
    nonce: Option<&str>,
) -> sqlx::Result<()> {
    sqlx::query!(
        "UPDATE compose_projects SET webhook_nonce = $1, updated_at = now() WHERE id = $2",
        nonce,
        project_id
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn webhook_watch_paths(pool: &PgPool, project_id: Uuid) -> sqlx::Result<Vec<String>> {
    let paths = sqlx::query_scalar!(
        "SELECT webhook_watch_paths FROM compose_projects WHERE id = $1",
        project_id
    )
    .fetch_optional(pool)
    .await?;
    Ok(paths.unwrap_or_default())
}

pub async fn set_webhook_watch_paths(
    pool: &PgPool,
    project_id: Uuid,
    paths: &[String],
) -> sqlx::Result<()> {
    sqlx::query!(
        "UPDATE compose_projects SET webhook_watch_paths = $1, updated_at = now() WHERE id = $2",
        paths,
        project_id
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// The oldest deployment of the project that has not started yet.
pub async fn find_queued_deployment(
    pool: &PgPool,
    project_id: Uuid,
) -> anyhow::Result<Option<DeploymentInfo>> {
    let record = sqlx::query_as!(
        DeploymentRecord,
        r#"SELECT id, project_id, status, log_path, trigger, commit_sha, queued_at, started_at,
                  finished_at, error_message, rollback_of,
                  (status = 'succeeded' AND snapshot IS NOT NULL AND release_retained) AS "rollback_available!"
           FROM compose_deployments
           WHERE project_id = $1 AND status = 'queued'
           ORDER BY queued_at LIMIT 1"#,
        project_id,
    )
    .fetch_optional(pool)
    .await?;
    record.map(DeploymentRecord::into_info).transpose()
}

/// Non-secret Git access state: the deploy key's public half, the host a
/// write-only HTTPS token was saved for, and the linked GitHub App
/// installation.
#[derive(Default)]
pub struct GitAccessRecord {
    pub deploy_public_key: Option<String>,
    pub git_token_host: Option<String>,
    pub github_app_id: Option<Uuid>,
    pub github_installation_id: Option<i64>,
}

pub async fn git_access(pool: &PgPool, project_id: Uuid) -> sqlx::Result<GitAccessRecord> {
    let record = sqlx::query_as!(
        GitAccessRecord,
        "SELECT deploy_public_key, git_token_host, github_app_id, github_installation_id
         FROM compose_projects WHERE id = $1",
        project_id
    )
    .fetch_optional(pool)
    .await?;
    Ok(record.unwrap_or_default())
}

/// Links (`Some((app, installation))`) or unlinks a GitHub App installation.
pub async fn set_github_app(
    pool: &PgPool,
    project_id: Uuid,
    link: Option<(Uuid, i64)>,
) -> sqlx::Result<()> {
    let (app, installation) = link.unzip();
    sqlx::query!(
        "UPDATE compose_projects SET github_app_id = $1, github_installation_id = $2, updated_at = now()
         WHERE id = $3",
        app,
        installation,
        project_id
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn set_deploy_public_key(
    pool: &PgPool,
    project_id: Uuid,
    public_key: Option<&str>,
) -> sqlx::Result<()> {
    sqlx::query!(
        "UPDATE compose_projects SET deploy_public_key = $1, updated_at = now() WHERE id = $2",
        public_key,
        project_id
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn set_git_token_host(
    pool: &PgPool,
    project_id: Uuid,
    host: Option<&str>,
) -> sqlx::Result<()> {
    sqlx::query!(
        "UPDATE compose_projects SET git_token_host = $1, updated_at = now() WHERE id = $2",
        host,
        project_id
    )
    .execute(pool)
    .await?;
    Ok(())
}
