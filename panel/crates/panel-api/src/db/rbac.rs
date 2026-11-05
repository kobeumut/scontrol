//! Roles, role bindings, and the permission catalog. Runtime `sqlx::query*`
//! like backups and metrics.

use chrono::{DateTime, Utc};
use panel_core::rbac::{Grant, Permission, Scope, BUILTIN_ROLES};
use serde::Serialize;
use sqlx::PgPool;
use std::collections::BTreeMap;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize)]
pub struct Role {
    pub id: Uuid,
    pub name: String,
    pub description: String,
    pub builtin: bool,
    pub permissions: Vec<Permission>,
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct Binding {
    pub id: Uuid,
    pub user_id: Uuid,
    pub username: String,
    pub role_id: Uuid,
    pub role_name: String,
    pub project_id: Option<Uuid>,
    pub project_name: Option<String>,
    pub environment_id: Option<Uuid>,
    pub environment_name: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// Brings the `permissions` table and the built-in roles in step with
/// `panel_core::rbac`. Runs after migrations at every start; a permission
/// that left the catalog is deleted (and with it every role's hold on it).
pub async fn sync_catalog(pool: &PgPool) -> sqlx::Result<()> {
    let mut tx = pool.begin().await?;
    let keys: Vec<String> = Permission::ALL.iter().map(|p| p.to_string()).collect();
    for permission in Permission::ALL {
        sqlx::query(
            "INSERT INTO permissions (key, description) VALUES ($1, $2)
             ON CONFLICT (key) DO UPDATE SET description = EXCLUDED.description",
        )
        .bind(permission.as_str())
        .bind(permission.description())
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query("DELETE FROM permissions WHERE NOT (key = ANY($1))")
        .bind(&keys)
        .execute(&mut *tx)
        .await?;
    for role in BUILTIN_ROLES {
        let id: Uuid = sqlx::query_scalar(
            "INSERT INTO roles (id, name, description, builtin) VALUES ($1, $2, $3, TRUE)
             ON CONFLICT (name) DO UPDATE
                 SET description = EXCLUDED.description, builtin = TRUE, updated_at = now()
             RETURNING id",
        )
        .bind(Uuid::new_v4())
        .bind(role.name)
        .bind(role.description)
        .fetch_one(&mut *tx)
        .await?;
        let wanted: Vec<String> = role.permissions.iter().map(|p| p.to_string()).collect();
        sqlx::query(
            "DELETE FROM role_permissions WHERE role_id = $1 AND NOT (permission = ANY($2))",
        )
        .bind(id)
        .bind(&wanted)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO role_permissions (role_id, permission)
             SELECT $1, UNNEST($2::text[]) ON CONFLICT DO NOTHING",
        )
        .bind(id)
        .bind(&wanted)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await
}

/// Every grant the user's bindings carry. Unknown permission strings are
/// skipped, so a stale row can only take access away.
pub async fn user_grants(pool: &PgPool, user_id: Uuid) -> sqlx::Result<Vec<Grant>> {
    let rows: Vec<(String, Option<Uuid>, Option<Uuid>)> = sqlx::query_as(
        "SELECT rp.permission, b.project_id, b.environment_id
         FROM role_bindings b
         JOIN role_permissions rp ON rp.role_id = b.role_id
         WHERE b.user_id = $1",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .filter_map(|(permission, project, environment)| {
            let permission = permission.parse().ok()?;
            let scope = match (project, environment) {
                (None, None) => Scope::Global,
                (Some(project_id), None) => Scope::Project { project_id },
                (Some(project_id), Some(environment_id)) => Scope::Environment {
                    project_id,
                    environment_id,
                },
                // The table's CHECK forbids it; refuse rather than guess.
                (None, Some(_)) => return None,
            };
            Some(Grant { permission, scope })
        })
        .collect())
}

pub async fn list_roles(pool: &PgPool) -> sqlx::Result<Vec<Role>> {
    let rows: Vec<(Uuid, String, String, bool, Option<String>)> = sqlx::query_as(
        "SELECT r.id, r.name, r.description, r.builtin, rp.permission
         FROM roles r LEFT JOIN role_permissions rp ON rp.role_id = r.id
         ORDER BY r.builtin DESC, r.name, rp.permission",
    )
    .fetch_all(pool)
    .await?;
    let mut roles: BTreeMap<Uuid, Role> = BTreeMap::new();
    let mut order = Vec::new();
    for (id, name, description, builtin, permission) in rows {
        let role = roles.entry(id).or_insert_with(|| {
            order.push(id);
            Role {
                id,
                name,
                description,
                builtin,
                permissions: Vec::new(),
            }
        });
        if let Some(permission) = permission.and_then(|p| p.parse().ok()) {
            role.permissions.push(permission);
        }
    }
    Ok(order
        .into_iter()
        .filter_map(|id| roles.remove(&id))
        .collect())
}

pub async fn find_role(pool: &PgPool, id: Uuid) -> sqlx::Result<Option<Role>> {
    Ok(list_roles(pool).await?.into_iter().find(|r| r.id == id))
}

/// Creates or replaces a custom role (built-in roles are refused by the
/// caller). Returns `None` when `id` names no custom role.
pub async fn save_role(
    pool: &PgPool,
    id: Option<Uuid>,
    name: &str,
    description: &str,
    permissions: &[Permission],
) -> sqlx::Result<Option<Uuid>> {
    let mut tx = pool.begin().await?;
    let id = match id {
        None => {
            sqlx::query_scalar(
                "INSERT INTO roles (id, name, description) VALUES ($1, $2, $3) RETURNING id",
            )
            .bind(Uuid::new_v4())
            .bind(name)
            .bind(description)
            .fetch_one(&mut *tx)
            .await?
        }
        Some(id) => {
            let updated: Option<Uuid> = sqlx::query_scalar(
                "UPDATE roles SET name = $2, description = $3, updated_at = now()
                 WHERE id = $1 AND NOT builtin RETURNING id",
            )
            .bind(id)
            .bind(name)
            .bind(description)
            .fetch_optional(&mut *tx)
            .await?;
            match updated {
                Some(id) => id,
                None => return Ok(None),
            }
        }
    };
    let keys: Vec<String> = permissions.iter().map(|p| p.to_string()).collect();
    sqlx::query("DELETE FROM role_permissions WHERE role_id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("INSERT INTO role_permissions (role_id, permission) SELECT $1, UNNEST($2::text[])")
        .bind(id)
        .bind(&keys)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Some(id))
}

/// Deletes a custom role and its bindings. False when it does not exist or
/// is built in.
pub async fn delete_role(pool: &PgPool, id: Uuid) -> sqlx::Result<bool> {
    let result = sqlx::query("DELETE FROM roles WHERE id = $1 AND NOT builtin")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() == 1)
}

pub async fn role_id_by_name(pool: &PgPool, name: &str) -> sqlx::Result<Option<Uuid>> {
    sqlx::query_scalar("SELECT id FROM roles WHERE name = $1")
        .bind(name)
        .fetch_optional(pool)
        .await
}

const BINDING_SELECT: &str = "SELECT b.id, b.user_id, u.username, b.role_id, r.name AS role_name,
        b.project_id, p.name AS project_name, b.environment_id, e.name AS environment_name,
        b.created_at
     FROM role_bindings b
     JOIN users u ON u.id = b.user_id
     JOIN roles r ON r.id = b.role_id
     LEFT JOIN projects p ON p.id = b.project_id
     LEFT JOIN environments e ON e.id = b.environment_id";

pub async fn list_bindings(pool: &PgPool, user_id: Option<Uuid>) -> sqlx::Result<Vec<Binding>> {
    sqlx::query_as(&format!(
        "{BINDING_SELECT} WHERE $1::uuid IS NULL OR b.user_id = $1
         ORDER BY u.username, r.name, p.name NULLS FIRST, e.name NULLS FIRST"
    ))
    .bind(user_id)
    .fetch_all(pool)
    .await
}

pub async fn find_binding(pool: &PgPool, id: Uuid) -> sqlx::Result<Option<Binding>> {
    sqlx::query_as(&format!("{BINDING_SELECT} WHERE b.id = $1"))
        .bind(id)
        .fetch_optional(pool)
        .await
}

pub async fn create_binding(
    pool: &PgPool,
    user_id: Uuid,
    role_id: Uuid,
    scope: Scope,
    created_by: Option<Uuid>,
) -> sqlx::Result<Uuid> {
    let (project_id, environment_id) = match scope {
        Scope::Global => (None, None),
        Scope::Project { project_id } => (Some(project_id), None),
        Scope::Environment {
            project_id,
            environment_id,
        } => (Some(project_id), Some(environment_id)),
    };
    sqlx::query_scalar(
        "INSERT INTO role_bindings (id, user_id, role_id, project_id, environment_id, created_by)
         VALUES ($1, $2, $3, $4, $5, $6) RETURNING id",
    )
    .bind(Uuid::new_v4())
    .bind(user_id)
    .bind(role_id)
    .bind(project_id)
    .bind(environment_id)
    .bind(created_by)
    .fetch_one(pool)
    .await
}

pub async fn delete_binding(pool: &PgPool, id: Uuid) -> sqlx::Result<bool> {
    let result = sqlx::query("DELETE FROM role_bindings WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() == 1)
}
