//! Projects → environments → services, and the sealed variables of each
//! layer. Values are only ever stored sealed (see `services::env_layers`);
//! nothing here returns a plaintext value.

use chrono::{DateTime, Utc};
use panel_core::types::{EnvLayer, Environment, Project};
use sqlx::PgPool;
use uuid::Uuid;

/// Slug of the project every owner gets for services created without an
/// explicit environment (and for services that predate projects).
pub const DEFAULT_PROJECT_SLUG: &str = "default";
pub const DEFAULT_ENVIRONMENT_SLUG: &str = "production";

/// One layer's owner: which table row the variables belong to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvScope {
    Project(Uuid),
    Environment(Uuid),
    Service(Uuid),
}

impl EnvScope {
    pub fn layer(self) -> EnvLayer {
        match self {
            Self::Project(_) => EnvLayer::Project,
            Self::Environment(_) => EnvLayer::Environment,
            Self::Service(_) => EnvLayer::Service,
        }
    }

    pub fn id(self) -> Uuid {
        match self {
            Self::Project(id) | Self::Environment(id) | Self::Service(id) => id,
        }
    }

    fn columns(self) -> (Option<Uuid>, Option<Uuid>, Option<Uuid>) {
        match self {
            Self::Project(id) => (Some(id), None, None),
            Self::Environment(id) => (None, Some(id), None),
            Self::Service(id) => (None, None, Some(id)),
        }
    }
}

/// A stored variable: its key and sealed value.
pub struct SealedVariable {
    pub key: String,
    pub value_sealed: Vec<u8>,
}

#[derive(sqlx::FromRow)]
struct ProjectRecord {
    id: Uuid,
    name: String,
    slug: String,
    owner_user_id: Uuid,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl From<ProjectRecord> for Project {
    fn from(r: ProjectRecord) -> Self {
        Project {
            id: r.id,
            name: r.name,
            slug: r.slug,
            owner_user_id: r.owner_user_id,
            created_at: r.created_at,
            updated_at: r.updated_at,
        }
    }
}

#[derive(sqlx::FromRow)]
struct EnvironmentRecord {
    id: Uuid,
    project_id: Uuid,
    name: String,
    slug: String,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl From<EnvironmentRecord> for Environment {
    fn from(r: EnvironmentRecord) -> Self {
        Environment {
            id: r.id,
            project_id: r.project_id,
            name: r.name,
            slug: r.slug,
            created_at: r.created_at,
            updated_at: r.updated_at,
        }
    }
}

/// Creates a project together with its first environment.
pub async fn create(
    pool: &PgPool,
    name: &str,
    slug: &str,
    owner_user_id: Uuid,
    environment_name: &str,
    environment_slug: &str,
) -> anyhow::Result<(Project, Environment)> {
    let mut tx = pool.begin().await?;
    let project = sqlx::query_as!(
        ProjectRecord,
        r#"INSERT INTO projects (id, name, slug, owner_user_id) VALUES ($1, $2, $3, $4)
           RETURNING id, name, slug, owner_user_id, created_at, updated_at"#,
        Uuid::new_v4(),
        name,
        slug,
        owner_user_id,
    )
    .fetch_one(&mut *tx)
    .await?;
    let environment = sqlx::query_as!(
        EnvironmentRecord,
        r#"INSERT INTO environments (id, project_id, name, slug) VALUES ($1, $2, $3, $4)
           RETURNING id, project_id, name, slug, created_at, updated_at"#,
        Uuid::new_v4(),
        project.id,
        environment_name,
        environment_slug,
    )
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok((project.into(), environment.into()))
}

pub async fn find_by_id(pool: &PgPool, id: Uuid) -> sqlx::Result<Option<Project>> {
    let record = sqlx::query_as!(
        ProjectRecord,
        "SELECT id, name, slug, owner_user_id, created_at, updated_at FROM projects WHERE id = $1",
        id
    )
    .fetch_optional(pool)
    .await?;
    Ok(record.map(Into::into))
}

pub async fn list_all(pool: &PgPool) -> sqlx::Result<Vec<Project>> {
    let records = sqlx::query_as!(
        ProjectRecord,
        "SELECT id, name, slug, owner_user_id, created_at, updated_at FROM projects ORDER BY name, created_at"
    )
    .fetch_all(pool)
    .await?;
    Ok(records.into_iter().map(Into::into).collect())
}

pub async fn rename(pool: &PgPool, id: Uuid, name: &str) -> sqlx::Result<bool> {
    let updated = sqlx::query!(
        "UPDATE projects SET name = $1, updated_at = now() WHERE id = $2",
        name,
        id
    )
    .execute(pool)
    .await?
    .rows_affected();
    Ok(updated == 1)
}

/// Deletes a project and its environments and variables. Fails with a
/// foreign-key violation while any environment still holds services.
pub async fn delete(pool: &PgPool, id: Uuid) -> sqlx::Result<bool> {
    let deleted = sqlx::query!("DELETE FROM projects WHERE id = $1", id)
        .execute(pool)
        .await?
        .rows_affected();
    Ok(deleted == 1)
}

/// The owner's default project's production environment, created on first
/// use. Concurrent callers converge on the same rows.
pub async fn default_environment(pool: &PgPool, owner_user_id: Uuid) -> sqlx::Result<Uuid> {
    let mut tx = pool.begin().await?;
    sqlx::query!(
        r#"INSERT INTO projects (id, name, slug, owner_user_id) VALUES ($1, 'Default', $2, $3)
           ON CONFLICT (owner_user_id, slug) DO NOTHING"#,
        Uuid::new_v4(),
        DEFAULT_PROJECT_SLUG,
        owner_user_id,
    )
    .execute(&mut *tx)
    .await?;
    let project_id = sqlx::query_scalar!(
        "SELECT id FROM projects WHERE owner_user_id = $1 AND slug = $2",
        owner_user_id,
        DEFAULT_PROJECT_SLUG,
    )
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query!(
        r#"INSERT INTO environments (id, project_id, name, slug) VALUES ($1, $2, $3, $3)
           ON CONFLICT (project_id, slug) DO NOTHING"#,
        Uuid::new_v4(),
        project_id,
        DEFAULT_ENVIRONMENT_SLUG,
    )
    .execute(&mut *tx)
    .await?;
    let environment_id = sqlx::query_scalar!(
        "SELECT id FROM environments WHERE project_id = $1 AND slug = $2",
        project_id,
        DEFAULT_ENVIRONMENT_SLUG,
    )
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(environment_id)
}

pub async fn create_environment(
    pool: &PgPool,
    project_id: Uuid,
    name: &str,
    slug: &str,
) -> anyhow::Result<Environment> {
    let record = sqlx::query_as!(
        EnvironmentRecord,
        r#"INSERT INTO environments (id, project_id, name, slug) VALUES ($1, $2, $3, $4)
           RETURNING id, project_id, name, slug, created_at, updated_at"#,
        Uuid::new_v4(),
        project_id,
        name,
        slug,
    )
    .fetch_one(pool)
    .await?;
    Ok(record.into())
}

pub async fn find_environment(pool: &PgPool, id: Uuid) -> sqlx::Result<Option<Environment>> {
    let record = sqlx::query_as!(
        EnvironmentRecord,
        "SELECT id, project_id, name, slug, created_at, updated_at FROM environments WHERE id = $1",
        id
    )
    .fetch_optional(pool)
    .await?;
    Ok(record.map(Into::into))
}

pub async fn list_environments(pool: &PgPool, project_id: Uuid) -> sqlx::Result<Vec<Environment>> {
    let records = sqlx::query_as!(
        EnvironmentRecord,
        r#"SELECT id, project_id, name, slug, created_at, updated_at FROM environments
           WHERE project_id = $1 ORDER BY created_at"#,
        project_id
    )
    .fetch_all(pool)
    .await?;
    Ok(records.into_iter().map(Into::into).collect())
}

/// Number of services (Compose projects) per environment of a project.
pub async fn service_counts(pool: &PgPool, project_id: Uuid) -> sqlx::Result<Vec<(Uuid, i64)>> {
    let rows = sqlx::query!(
        r#"SELECT e.id, COUNT(c.id) AS "count!" FROM environments e
           LEFT JOIN compose_projects c ON c.environment_id = e.id
           WHERE e.project_id = $1 GROUP BY e.id"#,
        project_id
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|r| (r.id, r.count)).collect())
}

/// Deletes an environment and its variables. Fails with a foreign-key
/// violation while it still holds services.
pub async fn delete_environment(pool: &PgPool, project_id: Uuid, id: Uuid) -> sqlx::Result<bool> {
    let deleted = sqlx::query!(
        "DELETE FROM environments WHERE id = $1 AND project_id = $2",
        id,
        project_id
    )
    .execute(pool)
    .await?
    .rows_affected();
    Ok(deleted == 1)
}

/// The environment and project a service inherits from.
pub async fn service_scopes(pool: &PgPool, service_id: Uuid) -> sqlx::Result<Option<(Uuid, Uuid)>> {
    let row = sqlx::query!(
        r#"SELECT e.id AS environment_id, e.project_id FROM compose_projects c
           JOIN environments e ON e.id = c.environment_id WHERE c.id = $1"#,
        service_id
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| (r.environment_id, r.project_id)))
}

/// The sealed variables of one layer, ordered by key.
pub async fn layer_variables(pool: &PgPool, scope: EnvScope) -> sqlx::Result<Vec<SealedVariable>> {
    let (project, environment, service) = scope.columns();
    sqlx::query_as!(
        SealedVariable,
        r#"SELECT key, value_sealed FROM env_variables
           WHERE ($1::uuid IS NOT NULL AND project_id = $1)
              OR ($2::uuid IS NOT NULL AND environment_id = $2)
              OR ($3::uuid IS NOT NULL AND service_id = $3)
           ORDER BY key"#,
        project,
        environment,
        service,
    )
    .fetch_all(pool)
    .await
}

/// Replaces every variable of one layer in one transaction. Replacing the
/// service layer also marks the service as layered.
pub async fn replace_layer(
    pool: &PgPool,
    scope: EnvScope,
    variables: &[SealedVariable],
) -> sqlx::Result<()> {
    let (project, environment, service) = scope.columns();
    let mut tx = pool.begin().await?;
    sqlx::query!(
        r#"DELETE FROM env_variables
           WHERE ($1::uuid IS NOT NULL AND project_id = $1)
              OR ($2::uuid IS NOT NULL AND environment_id = $2)
              OR ($3::uuid IS NOT NULL AND service_id = $3)"#,
        project,
        environment,
        service,
    )
    .execute(&mut *tx)
    .await?;
    for variable in variables {
        sqlx::query!(
            r#"INSERT INTO env_variables
                   (id, project_id, environment_id, service_id, key, value_sealed)
               VALUES ($1, $2, $3, $4, $5, $6)"#,
            Uuid::new_v4(),
            project,
            environment,
            service,
            variable.key,
            variable.value_sealed,
        )
        .execute(&mut *tx)
        .await?;
    }
    if let Some(service) = service {
        sqlx::query!(
            "UPDATE compose_projects SET env_layered = TRUE, updated_at = now() WHERE id = $1",
            service
        )
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await
}
