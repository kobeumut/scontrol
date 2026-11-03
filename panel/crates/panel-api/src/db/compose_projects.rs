use chrono::{DateTime, Utc};
use panel_core::types::{ComposeProject, ComposeSource, DeploymentInfo, DeploymentStatus};
use sqlx::PgPool;
use std::str::FromStr;
use uuid::Uuid;

#[derive(sqlx::FromRow)]
struct ProjectRecord {
    id: Uuid,
    name: String,
    slug: String,
    owner_user_id: Uuid,
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
    started_at: DateTime<Utc>,
    finished_at: Option<DateTime<Utc>>,
    error_message: Option<String>,
}

impl DeploymentRecord {
    fn into_info(self) -> anyhow::Result<DeploymentInfo> {
        Ok(DeploymentInfo {
            id: self.id,
            project_id: self.project_id,
            status: DeploymentStatus::from_str(&self.status).map_err(anyhow::Error::msg)?,
            log_path: self.log_path,
            started_at: self.started_at,
            finished_at: self.finished_at,
            error_message: self.error_message,
        })
    }
}

pub async fn create(
    pool: &PgPool,
    id: Uuid,
    name: &str,
    slug: &str,
    owner_user_id: Uuid,
    source: &ComposeSource,
) -> anyhow::Result<ComposeProject> {
    let source = serde_json::to_value(source)?;
    let record = sqlx::query_as!(
        ProjectRecord,
        r#"INSERT INTO compose_projects (id, name, slug, owner_user_id, source)
           VALUES ($1, $2, $3, $4, $5)
           RETURNING id, name, slug, owner_user_id, source, status, created_at, updated_at"#,
        id,
        name,
        slug,
        owner_user_id,
        source,
    )
    .fetch_one(pool)
    .await?;
    record.into_project()
}

pub async fn find_by_id(pool: &PgPool, id: Uuid) -> anyhow::Result<Option<ComposeProject>> {
    let record = sqlx::query_as!(
        ProjectRecord,
        "SELECT id, name, slug, owner_user_id, source, status, created_at, updated_at FROM compose_projects WHERE id = $1",
        id,
    )
    .fetch_optional(pool)
    .await?;
    record.map(ProjectRecord::into_project).transpose()
}

pub async fn list_for_owner(
    pool: &PgPool,
    owner_user_id: Uuid,
) -> anyhow::Result<Vec<ComposeProject>> {
    let records = sqlx::query_as!(
        ProjectRecord,
        "SELECT id, name, slug, owner_user_id, source, status, created_at, updated_at FROM compose_projects WHERE owner_user_id = $1 ORDER BY created_at DESC",
        owner_user_id,
    )
    .fetch_all(pool)
    .await?;
    records
        .into_iter()
        .map(ProjectRecord::into_project)
        .collect()
}

pub async fn list_all(pool: &PgPool) -> anyhow::Result<Vec<ComposeProject>> {
    let records = sqlx::query_as!(
        ProjectRecord,
        "SELECT id, name, slug, owner_user_id, source, status, created_at, updated_at FROM compose_projects ORDER BY created_at DESC",
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

pub async fn create_deployment(
    pool: &PgPool,
    id: Uuid,
    project_id: Uuid,
    log_path: &str,
) -> anyhow::Result<DeploymentInfo> {
    let record = sqlx::query_as!(
        DeploymentRecord,
        r#"INSERT INTO compose_deployments (id, project_id, status, log_path)
           VALUES ($1, $2, 'running', $3)
           RETURNING id, project_id, status, log_path, started_at, finished_at, error_message"#,
        id,
        project_id,
        log_path,
    )
    .fetch_one(pool)
    .await?;
    record.into_info()
}

pub async fn finish_deployment(
    pool: &PgPool,
    id: Uuid,
    status: DeploymentStatus,
    error_message: Option<&str>,
) -> sqlx::Result<()> {
    sqlx::query!(
        "UPDATE compose_deployments SET status = $1, finished_at = now(), error_message = $2 WHERE id = $3",
        status.to_string(),
        error_message,
        id,
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn list_deployments(
    pool: &PgPool,
    project_id: Uuid,
) -> anyhow::Result<Vec<DeploymentInfo>> {
    let records = sqlx::query_as!(
        DeploymentRecord,
        "SELECT id, project_id, status, log_path, started_at, finished_at, error_message FROM compose_deployments WHERE project_id = $1 ORDER BY started_at DESC",
        project_id,
    )
    .fetch_all(pool)
    .await?;
    records
        .into_iter()
        .map(DeploymentRecord::into_info)
        .collect()
}
