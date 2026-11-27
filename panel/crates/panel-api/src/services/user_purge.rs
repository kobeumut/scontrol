//! Removes everything a panel user owns, before the user is deleted.
//!
//! `sites`, `compose_projects` and `projects` reference `users` with
//! `ON DELETE CASCADE`, so deleting a user alone would drop the panel rows
//! while their vhosts, containers, timers, crontabs and host databases kept
//! running with nothing left to manage them. This tears each one down on its
//! host first, through the same steps as removing it by hand, and only then
//! lets the user row go.
//!
//! Every step is idempotent: when one fails (an unreachable server, a running
//! deployment), what was removed stays removed, the user is kept, and the
//! request can simply be repeated.

use crate::db;
use crate::error::{ApiError, ApiResult};
use crate::routes::compose_projects::teardown;
use crate::services::site_service;
use crate::state::AppState;
use panel_core::adapters::{DeleteDatabaseParams, RemoveCronJobParams};
use panel_core::types::DeploymentStatus;
use serde::Serialize;
use uuid::Uuid;

/// What a user owns that their deletion has to remove.
#[derive(Debug, Default, Serialize, PartialEq, Eq)]
pub struct Owned {
    pub sites: i64,
    pub services: i64,
}

impl Owned {
    pub fn is_empty(&self) -> bool {
        self.sites == 0 && self.services == 0
    }
}

pub async fn owned(state: &AppState, user_id: Uuid) -> ApiResult<Owned> {
    let (sites, services): (i64, i64) = sqlx::query_as(
        "SELECT (SELECT COUNT(*) FROM sites WHERE owner_user_id = $1),
                (SELECT COUNT(*) FROM compose_projects WHERE owner_user_id = $1)",
    )
    .bind(user_id)
    .fetch_one(&state.db)
    .await?;
    Ok(Owned { sites, services })
}

/// What [`purge`] removed.
#[derive(Debug, Default, Serialize)]
pub struct Purged {
    pub sites: Vec<String>,
    pub services: Vec<String>,
    pub databases: usize,
    pub cron_jobs: usize,
}

/// Tears down every service and site `user_id` owns. Services go first:
/// a preview is removed with its parent, so only top-level services are
/// torn down directly. Nothing is touched when one of the services has a
/// deployment running.
pub async fn purge(state: &AppState, user_id: Uuid, actor: Uuid) -> ApiResult<Purged> {
    let mut purged = Purged::default();

    let services: Vec<(Uuid,)> = sqlx::query_as(
        "SELECT c.id FROM compose_projects c
         WHERE c.owner_user_id = $1
           AND NOT EXISTS (SELECT 1 FROM preview_deployments p WHERE p.preview_project_id = c.id)
         ORDER BY c.created_at",
    )
    .bind(user_id)
    .fetch_all(&state.db)
    .await?;
    let mut projects = Vec::with_capacity(services.len());
    for (id,) in services {
        if let Some(project) = db::compose_projects::find_by_id(&state.db, id).await? {
            projects.push(project);
        }
    }
    if let Some(busy) = projects
        .iter()
        .find(|p| p.status == DeploymentStatus::Running)
    {
        return Err(ApiError::Conflict(format!(
            "service {} is deploying; wait for it to finish, then delete the user again",
            busy.slug
        )));
    }
    for project in projects {
        teardown(state, &project, false).await?;
        audit(
            state,
            actor,
            "remove_compose_project",
            &project.slug,
            user_id,
        )
        .await;
        purged.services.push(project.slug);
    }

    let sites: Vec<(Uuid, String)> =
        sqlx::query_as("SELECT id, domain FROM sites WHERE owner_user_id = $1 ORDER BY created_at")
            .bind(user_id)
            .fetch_all(&state.db)
            .await?;
    for (site_id, domain) in sites {
        let account = db::sites::owner_username(&state.db, site_id)
            .await?
            .unwrap_or_default();
        // Crontab entries live in the site account's crontab on the host;
        // their rows would cascade with the site, the entries would not.
        for job in db::cron::list_for_site(&state.db, site_id).await? {
            state
                .agent
                .remove_cron_job(RemoveCronJobParams {
                    owner_username: account.clone(),
                    job_id: job.id,
                })
                .await?;
            db::cron::delete(&state.db, job.id).await?;
            purged.cron_jobs += 1;
        }
        // Host databases keep running when their site goes (`site_id` is
        // only set to NULL), so drop them on the host.
        for database in db::databases::list_for_site(&state.db, site_id).await? {
            state
                .agent
                .delete_database(DeleteDatabaseParams {
                    engine: database.engine,
                    name: database.name,
                    username: database.username,
                })
                .await?;
            db::databases::delete(&state.db, database.id).await?;
            purged.databases += 1;
        }
        site_service::delete_site(&state.db, state.agent.as_ref(), site_id).await?;
        audit(state, actor, "delete_site", &domain, user_id).await;
        purged.sites.push(domain);
    }

    // A preview of another user's service is the only thing that can be left.
    let left = owned(state, user_id).await?;
    if !left.is_empty() {
        return Err(ApiError::Conflict(format!(
            "user still owns {} site(s) and {} service(s) that could not be removed",
            left.sites, left.services
        )));
    }
    Ok(purged)
}

async fn audit(state: &AppState, actor: Uuid, action: &str, target: &str, owner: Uuid) {
    db::audit::record(
        &state.db,
        Some(actor),
        "user",
        action,
        Some(target),
        serde_json::json!({ "reason": "owner_deleted", "owner_user_id": owner }),
    )
    .await
    .ok();
}
