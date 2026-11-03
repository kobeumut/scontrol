//! Site provisioning business logic: orchestrates the DB (source of truth
//! for panel state) and the agent (source of truth for host state), with
//! explicit rollback on partial failure. Takes `&dyn AdapterPort` so it can
//! be unit tested against `panel_core::adapters::mock::MockAdapter` without
//! a real agent or host.

use crate::db;
use panel_core::adapters::{AdapterPort, CreateSiteParams, DeleteSiteParams};
use panel_core::error::{CoreError, CoreResult};
use panel_core::types::{Runtime, Site, SiteStatus};
use sqlx::PgPool;
use uuid::Uuid;

pub struct CreateSiteRequest {
    pub domain: String,
    pub owner_user_id: Uuid,
    pub owner_username: String,
    pub runtime: Runtime,
}

/// Creates a site: persists it as `provisioning`, asks the agent to
/// provision the host resources, then marks it `active`. If the agent call
/// fails, the site row is deleted (rollback) so the panel never shows a site
/// that doesn't actually exist on disk.
pub async fn create_site(
    pool: &PgPool,
    agent: &dyn AdapterPort,
    req: CreateSiteRequest,
) -> CoreResult<Site> {
    if !panel_core::types::validate::is_valid_domain(&req.domain) {
        return Err(CoreError::Validation(format!(
            "invalid domain: {}",
            req.domain
        )));
    }
    if db::sites::find_by_domain(pool, &req.domain)
        .await
        .map_err(|e| CoreError::External(e.to_string()))?
        .is_some()
    {
        return Err(CoreError::AlreadyExists(format!(
            "site {} already exists",
            req.domain
        )));
    }

    let site_id = Uuid::new_v4();
    let document_root = format!(
        "/srv/scontrol/sites/{}/{}/public",
        req.owner_username, req.domain
    );

    let site = db::sites::create_site(
        pool,
        site_id,
        &req.domain,
        req.owner_user_id,
        &req.owner_username,
        &req.runtime,
        &document_root,
    )
    .await
    .map_err(|e| CoreError::External(e.to_string()))?;

    let agent_result = agent
        .create_site(CreateSiteParams {
            site_id,
            domain: req.domain.clone(),
            owner_username: req.owner_username.clone(),
            runtime: req.runtime.clone(),
        })
        .await;

    match agent_result {
        Ok(()) => {
            db::sites::set_status(pool, site_id, SiteStatus::Active)
                .await
                .map_err(|e| CoreError::External(e.to_string()))?;
            let mut active = site;
            active.status = SiteStatus::Active;
            Ok(active)
        }
        Err(agent_err) => {
            // Rollback: the host was never (fully) provisioned, so the
            // panel must not remember a site that doesn't exist.
            db::sites::delete(pool, site_id).await.ok();
            Err(agent_err)
        }
    }
}

pub async fn delete_site(pool: &PgPool, agent: &dyn AdapterPort, site_id: Uuid) -> CoreResult<()> {
    let site = db::sites::find_by_id(pool, site_id)
        .await
        .map_err(|e| CoreError::External(e.to_string()))?
        .ok_or_else(|| CoreError::NotFound(format!("site {site_id} not found")))?;

    let owner_username = db::users::find_by_id(pool, site.owner_user_id)
        .await
        .map_err(|e| CoreError::External(e.to_string()))?
        .map(|u| u.username)
        .unwrap_or_default();

    let php_version = match &site.runtime {
        Runtime::Php { version } => Some(version.clone()),
        _ => None,
    };

    agent
        .delete_site(DeleteSiteParams {
            site_id,
            domain: site.domain.clone(),
            owner_username,
            php_version,
        })
        .await?;

    db::sites::delete(pool, site_id)
        .await
        .map_err(|e| CoreError::External(e.to_string()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use panel_core::adapters::mock::MockAdapter;
    use panel_core::types::Runtime;

    async fn test_pool() -> PgPool {
        let url = std::env::var("DATABASE_URL").unwrap_or_else(|_| {
            "postgres://scontrol:scontrol@localhost:55432/scontrol".to_string()
        });
        PgPool::connect(&url)
            .await
            .expect("connect to test database")
    }

    #[tokio::test]
    async fn create_site_rolls_back_db_row_on_agent_failure() {
        let pool = test_pool().await;
        let adapter = MockAdapter::new();
        adapter.fail_next_call("create_site");

        let owner = db::users::create_user(
            &pool,
            &format!("u{}", &Uuid::new_v4().simple().to_string()[..8]),
            &format!("{}@example.com", Uuid::new_v4()),
            "hash",
            false,
        )
        .await
        .unwrap();

        let domain = format!("{}.example.com", Uuid::new_v4().simple());
        let result = create_site(
            &pool,
            &adapter,
            CreateSiteRequest {
                domain: domain.clone(),
                owner_user_id: owner.id,
                owner_username: owner.username.clone(),
                runtime: Runtime::Static,
            },
        )
        .await;

        assert!(result.is_err(), "expected agent failure to propagate");
        let found = db::sites::find_by_domain(&pool, &domain).await.unwrap();
        assert!(
            found.is_none(),
            "site row must be rolled back on agent failure"
        );
    }

    #[tokio::test]
    async fn create_site_succeeds_and_marks_active() {
        let pool = test_pool().await;
        let adapter = MockAdapter::new();

        let owner = db::users::create_user(
            &pool,
            &format!("u{}", &Uuid::new_v4().simple().to_string()[..8]),
            &format!("{}@example.com", Uuid::new_v4()),
            "hash",
            false,
        )
        .await
        .unwrap();

        let domain = format!("{}.example.com", Uuid::new_v4().simple());
        let site = create_site(
            &pool,
            &adapter,
            CreateSiteRequest {
                domain: domain.clone(),
                owner_user_id: owner.id,
                owner_username: owner.username.clone(),
                runtime: Runtime::Static,
            },
        )
        .await
        .unwrap();

        assert_eq!(site.status, SiteStatus::Active);
        assert!(adapter
            .calls
            .lock()
            .unwrap()
            .contains(&"create_site".to_string()));
    }
}
