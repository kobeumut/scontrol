//! Pull request preview settings and preview records (runtime-checked SQL,
//! like backups and metrics).

use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

/// A project's preview settings. The sealed comment token never leaves
/// this module except through [`comment_token`].
#[derive(Debug, Clone, sqlx::FromRow, Serialize)]
pub struct SettingsRow {
    pub enabled: bool,
    pub base_domain: String,
    pub service_name: String,
    pub container_port: i32,
    pub https: bool,
    pub wildcard_certificate: Option<String>,
    pub max_active: i32,
    pub ttl_hours: i32,
    pub memory_mb: i32,
    pub cpu_millis: i32,
    /// Whether a write-only comment token is stored (never the token).
    pub has_comment_token: bool,
    pub updated_at: DateTime<Utc>,
}

const SETTINGS_COLUMNS: &str = "enabled, base_domain, service_name, container_port, \
     https, wildcard_certificate, max_active, ttl_hours, memory_mb, cpu_millis, \
     (comment_token_sealed IS NOT NULL) AS has_comment_token, updated_at";

/// Validated settings to store.
pub struct SettingsInput<'a> {
    pub enabled: bool,
    pub base_domain: &'a str,
    pub service_name: &'a str,
    pub container_port: u16,
    pub https: bool,
    pub wildcard_certificate: Option<&'a str>,
    pub max_active: u32,
    pub ttl_hours: u32,
    pub memory_mb: u32,
    pub cpu_millis: u32,
}

pub async fn settings(pool: &PgPool, project_id: Uuid) -> sqlx::Result<Option<SettingsRow>> {
    sqlx::query_as::<_, SettingsRow>(&format!(
        "SELECT {SETTINGS_COLUMNS} FROM preview_settings WHERE project_id = $1"
    ))
    .bind(project_id)
    .fetch_optional(pool)
    .await
}

pub async fn upsert_settings(
    pool: &PgPool,
    project_id: Uuid,
    input: &SettingsInput<'_>,
) -> sqlx::Result<SettingsRow> {
    sqlx::query_as::<_, SettingsRow>(&format!(
        "INSERT INTO preview_settings (project_id, enabled, base_domain, service_name, \
             container_port, https, wildcard_certificate, max_active, ttl_hours, memory_mb, \
             cpu_millis) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11) \
         ON CONFLICT (project_id) DO UPDATE SET enabled = EXCLUDED.enabled, \
             base_domain = EXCLUDED.base_domain, service_name = EXCLUDED.service_name, \
             container_port = EXCLUDED.container_port, https = EXCLUDED.https, \
             wildcard_certificate = EXCLUDED.wildcard_certificate, \
             max_active = EXCLUDED.max_active, ttl_hours = EXCLUDED.ttl_hours, \
             memory_mb = EXCLUDED.memory_mb, cpu_millis = EXCLUDED.cpu_millis, \
             updated_at = now() \
         RETURNING {SETTINGS_COLUMNS}"
    ))
    .bind(project_id)
    .bind(input.enabled)
    .bind(input.base_domain)
    .bind(input.service_name)
    .bind(i32::from(input.container_port))
    .bind(input.https)
    .bind(input.wildcard_certificate)
    .bind(input.max_active as i32)
    .bind(input.ttl_hours as i32)
    .bind(input.memory_mb as i32)
    .bind(input.cpu_millis as i32)
    .fetch_one(pool)
    .await
}

/// The sealed comment token, if one is stored.
pub async fn comment_token(pool: &PgPool, project_id: Uuid) -> sqlx::Result<Option<Vec<u8>>> {
    let token: Option<Option<Vec<u8>>> = sqlx::query_scalar(
        "SELECT comment_token_sealed FROM preview_settings WHERE project_id = $1",
    )
    .bind(project_id)
    .fetch_optional(pool)
    .await?;
    Ok(token.flatten())
}

/// Stores or clears the sealed comment token. False when the project has
/// no preview settings yet.
pub async fn set_comment_token(
    pool: &PgPool,
    project_id: Uuid,
    sealed: Option<&[u8]>,
) -> sqlx::Result<bool> {
    let updated = sqlx::query(
        "UPDATE preview_settings SET comment_token_sealed = $1, updated_at = now() \
         WHERE project_id = $2",
    )
    .bind(sealed)
    .bind(project_id)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(updated == 1)
}

/// A preview with its Compose project's slug, status and domain.
#[derive(Debug, Clone, sqlx::FromRow, Serialize)]
pub struct PreviewRow {
    pub id: Uuid,
    pub parent_project_id: Uuid,
    pub preview_project_id: Uuid,
    pub slug: String,
    pub provider: String,
    pub pr_number: i64,
    pub head_branch: String,
    pub head_sha: String,
    pub status: String,
    pub close_reason: Option<String>,
    #[serde(skip)]
    pub comment_id: Option<i64>,
    /// Deployment status of the preview's Compose project.
    pub project_status: String,
    pub domain: Option<String>,
    pub last_activity_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
}

const PREVIEW_SELECT: &str = "SELECT p.id, p.parent_project_id, p.preview_project_id, c.slug, \
     p.provider, p.pr_number, p.head_branch, p.head_sha, p.status, p.close_reason, p.comment_id, \
     c.status AS project_status, \
     (SELECT d.domain FROM compose_project_domains d WHERE d.project_id = c.id \
      ORDER BY d.created_at LIMIT 1) AS domain, \
     p.last_activity_at, p.created_at \
     FROM preview_deployments p JOIN compose_projects c ON c.id = p.preview_project_id";

pub async fn list_for_parent(pool: &PgPool, parent: Uuid) -> sqlx::Result<Vec<PreviewRow>> {
    sqlx::query_as::<_, PreviewRow>(&format!(
        "{PREVIEW_SELECT} WHERE p.parent_project_id = $1 ORDER BY p.pr_number DESC"
    ))
    .bind(parent)
    .fetch_all(pool)
    .await
}

pub async fn find_by_pr(
    pool: &PgPool,
    parent: Uuid,
    number: i64,
) -> sqlx::Result<Option<PreviewRow>> {
    sqlx::query_as::<_, PreviewRow>(&format!(
        "{PREVIEW_SELECT} WHERE p.parent_project_id = $1 AND p.pr_number = $2"
    ))
    .bind(parent)
    .bind(number)
    .fetch_optional(pool)
    .await
}

/// The preview record of a preview Compose project, if it is one.
pub async fn find_by_preview_project(
    pool: &PgPool,
    preview_project: Uuid,
) -> sqlx::Result<Option<PreviewRow>> {
    sqlx::query_as::<_, PreviewRow>(&format!("{PREVIEW_SELECT} WHERE p.preview_project_id = $1"))
        .bind(preview_project)
        .fetch_optional(pool)
        .await
}

/// Previews waiting for teardown.
pub async fn list_closing(pool: &PgPool) -> sqlx::Result<Vec<PreviewRow>> {
    sqlx::query_as::<_, PreviewRow>(&format!(
        "{PREVIEW_SELECT} WHERE p.status = 'closing' ORDER BY p.last_activity_at"
    ))
    .fetch_all(pool)
    .await
}

/// Active previews with their project's TTL, for the expiry sweep.
pub async fn list_active_with_ttl(pool: &PgPool) -> sqlx::Result<Vec<(PreviewRow, i32)>> {
    #[derive(sqlx::FromRow)]
    struct Row {
        #[sqlx(flatten)]
        preview: PreviewRow,
        ttl_hours: i32,
    }
    let rows = sqlx::query_as::<_, Row>(&format!(
        "SELECT q.*, s.ttl_hours FROM ({PREVIEW_SELECT} WHERE p.status = 'active') q \
         JOIN preview_settings s ON s.project_id = q.parent_project_id"
    ))
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|r| (r.preview, r.ttl_hours)).collect())
}

/// Marks one active preview closing. False if it was not active.
pub async fn mark_closing(pool: &PgPool, id: Uuid, reason: &str) -> sqlx::Result<bool> {
    let updated = sqlx::query(
        "UPDATE preview_deployments SET status = 'closing', close_reason = $2 \
         WHERE id = $1 AND status = 'active'",
    )
    .bind(id)
    .bind(reason)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(updated == 1)
}

/// Marks every active preview of a project closing; returns how many.
pub async fn mark_all_closing(pool: &PgPool, parent: Uuid, reason: &str) -> sqlx::Result<u64> {
    Ok(sqlx::query(
        "UPDATE preview_deployments SET status = 'closing', close_reason = $2 \
         WHERE parent_project_id = $1 AND status = 'active'",
    )
    .bind(parent)
    .bind(reason)
    .execute(pool)
    .await?
    .rows_affected())
}

pub async fn set_comment_id(pool: &PgPool, id: Uuid, comment_id: i64) -> sqlx::Result<()> {
    sqlx::query("UPDATE preview_deployments SET comment_id = $2 WHERE id = $1")
        .bind(id)
        .bind(comment_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Serialises preview changes of one parent project (webhook deliveries
/// for the same request can arrive concurrently).
pub async fn lock_parent(tx: &mut Transaction<'_, Postgres>, parent: Uuid) -> sqlx::Result<()> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext('scontrol.previews'), hashtext($1))")
        .bind(parent.to_string())
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// `(preview id, preview project id, status)` of a request's preview.
pub async fn status_in(
    tx: &mut Transaction<'_, Postgres>,
    parent: Uuid,
    number: i64,
) -> sqlx::Result<Option<(Uuid, Uuid, String)>> {
    sqlx::query_as(
        "SELECT id, preview_project_id, status FROM preview_deployments \
         WHERE parent_project_id = $1 AND pr_number = $2",
    )
    .bind(parent)
    .bind(number)
    .fetch_optional(&mut **tx)
    .await
}

pub async fn count_active(tx: &mut Transaction<'_, Postgres>, parent: Uuid) -> sqlx::Result<i64> {
    sqlx::query_scalar(
        "SELECT count(*) FROM preview_deployments WHERE parent_project_id = $1 AND status = 'active'",
    )
    .bind(parent)
    .fetch_one(&mut **tx)
    .await
}

/// Everything needed to create the preview's Compose project row, its
/// domain binding, and the preview record.
pub struct NewPreview<'a> {
    pub parent_project_id: Uuid,
    pub preview_project_id: Uuid,
    pub name: &'a str,
    pub slug: &'a str,
    pub source: &'a serde_json::Value,
    pub provider: &'a str,
    pub number: i64,
    pub head_branch: &'a str,
    pub head_sha: &'a str,
    pub domain: &'a str,
    pub service_name: &'a str,
    pub container_port: i32,
    pub https: bool,
}

/// Inserts the preview project (inheriting the parent's owner,
/// environment, layering mode, GitHub App link and registry), its domain on
/// the lowest free loopback port, and the preview record. Returns false
/// when no loopback port is free.
pub async fn insert_preview(
    tx: &mut Transaction<'_, Postgres>,
    new: &NewPreview<'_>,
) -> sqlx::Result<bool> {
    sqlx::query(
        "INSERT INTO compose_projects (id, name, slug, owner_user_id, environment_id, \
             env_layered, source, github_app_id, github_installation_id, registry_id) \
         SELECT $1, $2, $3, owner_user_id, environment_id, env_layered, $4, github_app_id, \
             github_installation_id, registry_id \
         FROM compose_projects WHERE id = $5",
    )
    .bind(new.preview_project_id)
    .bind(new.name)
    .bind(new.slug)
    .bind(new.source)
    .bind(new.parent_project_id)
    .execute(&mut **tx)
    .await?;
    let domain = sqlx::query(
        "INSERT INTO compose_project_domains \
             (id, project_id, domain, service_name, container_port, https, host_port) \
         SELECT $1, $2, $3, $4, $5, $6, port FROM generate_series(40000, 49999) AS port \
         WHERE NOT EXISTS (SELECT 1 FROM compose_project_domains WHERE host_port = port) \
         ORDER BY port LIMIT 1",
    )
    .bind(Uuid::new_v4())
    .bind(new.preview_project_id)
    .bind(new.domain)
    .bind(new.service_name)
    .bind(new.container_port)
    .bind(new.https)
    .execute(&mut **tx)
    .await?
    .rows_affected();
    if domain == 0 {
        return Ok(false);
    }
    sqlx::query(
        "INSERT INTO preview_deployments (id, parent_project_id, preview_project_id, provider, \
             pr_number, head_branch, head_sha) VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(Uuid::new_v4())
    .bind(new.parent_project_id)
    .bind(new.preview_project_id)
    .bind(new.provider)
    .bind(new.number)
    .bind(new.head_branch)
    .bind(new.head_sha)
    .execute(&mut **tx)
    .await?;
    Ok(true)
}

/// Records a new head commit (and branch) and resets the TTL clock.
pub async fn touch(
    tx: &mut Transaction<'_, Postgres>,
    id: Uuid,
    preview_project: Uuid,
    head_branch: &str,
    head_sha: &str,
    source: &serde_json::Value,
) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE preview_deployments SET head_branch = $2, head_sha = $3, last_activity_at = now() \
         WHERE id = $1",
    )
    .bind(id)
    .bind(head_branch)
    .bind(head_sha)
    .execute(&mut **tx)
    .await?;
    sqlx::query("UPDATE compose_projects SET source = $2, updated_at = now() WHERE id = $1")
        .bind(preview_project)
        .bind(source)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Moves a preview's last activity (tests drive the TTL with it).
#[cfg(test)]
pub async fn set_last_activity(pool: &PgPool, id: Uuid, at: DateTime<Utc>) -> sqlx::Result<()> {
    sqlx::query("UPDATE preview_deployments SET last_activity_at = $2 WHERE id = $1")
        .bind(id)
        .bind(at)
        .execute(pool)
        .await?;
    Ok(())
}
