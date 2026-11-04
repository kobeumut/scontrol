//! Advanced vhost options of a project domain
//! (`/api/compose-projects/:id/domains/:domain_id/options`): redirects,
//! HTTP Basic Auth, a path prefix, and an uploaded TLS certificate.
//!
//! Basic Auth passwords and certificate private keys are write-only. They go
//! straight to the agent, which hashes passwords and stores everything in
//! 0600 files; the database keeps only the flags and non-secret settings.
//! Nginx validation (`nginx -t`) and rollback happen in the agent during the
//! redeploy this endpoint queues.

use crate::auth::AdminUser;
use crate::db;
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use panel_core::adapters::WriteVhostSecretsParams;
use panel_core::types::{ComposeSource, DeploymentInfo};
use panel_core::vhost::{
    validate_basic_auth_users, validate_certificate, BasicAuthUser, CustomCertificate,
    SecretChange, VhostOptions,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub fn router() -> Router<AppState> {
    Router::new().route("/:id/domains/:domain_id/options", get(show).put(update))
}

async fn show(
    AdminUser(_): AdminUser,
    State(state): State<AppState>,
    Path((id, domain_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<Json<VhostOptions>> {
    let domain = db::compose_projects::list_domains(&state.db, id)
        .await?
        .into_iter()
        .find(|d| d.id == domain_id)
        .ok_or_else(|| ApiError::NotFound("domain binding not found".into()))?;
    Ok(Json(domain.options))
}

#[derive(Deserialize)]
pub struct UpdateOptionsBody {
    pub options: VhostOptions,
    /// `{"action":"set","value":[{username,password}]}`, `keep`, or `remove`.
    #[serde(default)]
    pub basic_auth: SecretChange<Vec<BasicAuthUser>>,
    #[serde(default)]
    pub certificate: SecretChange<CustomCertificate>,
    /// Queue a deployment that applies the options (default true).
    #[serde(default = "default_true")]
    pub redeploy: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Serialize)]
struct Updated {
    options: VhostOptions,
    deployment: Option<DeploymentInfo>,
}

/// Checks that the flags in `options` agree with what the secret changes
/// will leave on the agent, and that every value is well formed.
pub fn validate_update(previous: &VhostOptions, body: &UpdateOptionsBody) -> Result<(), String> {
    body.options.validate()?;
    let will_have_auth = match &body.basic_auth {
        SecretChange::Set(users) => {
            validate_basic_auth_users(users)?;
            true
        }
        SecretChange::Remove => false,
        SecretChange::Keep => previous.basic_auth,
    };
    if body.options.basic_auth != will_have_auth {
        return Err(if body.options.basic_auth {
            "basic_auth needs at least one user: send basic_auth.action = set".into()
        } else {
            "basic_auth users exist: send basic_auth.action = remove to disable it".into()
        });
    }
    let will_have_cert = match &body.certificate {
        SecretChange::Set(cert) => {
            validate_certificate(cert)?;
            true
        }
        SecretChange::Remove => false,
        SecretChange::Keep => previous.custom_cert,
    };
    if body.options.custom_cert != will_have_cert {
        return Err(if body.options.custom_cert {
            "custom_cert needs an uploaded certificate: send certificate.action = set".into()
        } else {
            "a custom certificate exists: send certificate.action = remove to drop it".into()
        });
    }
    Ok(())
}

async fn update(
    AdminUser(user): AdminUser,
    State(state): State<AppState>,
    Path((id, domain_id)): Path<(Uuid, Uuid)>,
    Json(body): Json<UpdateOptionsBody>,
) -> ApiResult<(StatusCode, Json<Updated>)> {
    let project = db::compose_projects::find_by_id(&state.db, id)
        .await?
        .ok_or_else(|| ApiError::NotFound("Compose project not found".into()))?;
    let domain = db::compose_projects::list_domains(&state.db, id)
        .await?
        .into_iter()
        .find(|d| d.id == domain_id)
        .ok_or_else(|| ApiError::NotFound("domain binding not found".into()))?;
    validate_update(&domain.options, &body).map_err(ApiError::BadRequest)?;
    if body.options.custom_cert && !domain.https {
        return Err(ApiError::BadRequest(
            "enable HTTPS on the domain before using a custom certificate".into(),
        ));
    }
    if body.options.path_prefix.is_some() && matches!(project.source, ComposeSource::Static(_)) {
        return Err(ApiError::BadRequest(
            "a path prefix is only available for container services".into(),
        ));
    }
    let touches_secrets = !matches!(body.basic_auth, SecretChange::Keep)
        || !matches!(body.certificate, SecretChange::Keep);
    if touches_secrets {
        state
            .agent
            .write_vhost_secrets(WriteVhostSecretsParams {
                project_slug: project.slug.clone(),
                domain: domain.domain.clone(),
                basic_auth: body.basic_auth,
                certificate: body.certificate,
            })
            .await?;
    }
    db::compose_projects::set_domain_options(&state.db, id, domain_id, &body.options).await?;
    db::audit::record(
        &state.db,
        Some(user.id),
        "user",
        "update_domain_options",
        Some(&domain.domain),
        serde_json::json!({
            "project_id": id,
            "redirect": body.options.redirect.is_some(),
            "path_prefix": body.options.path_prefix,
            "basic_auth": body.options.basic_auth,
            "custom_cert": body.options.custom_cert,
        }),
    )
    .await
    .ok();
    let deployment = if body.redeploy {
        Some(
            state
                .deploy_queue
                .enqueue(&state.db, id, &project.slug, false, "domain")
                .await?,
        )
    } else {
        None
    };
    Ok((
        StatusCode::ACCEPTED,
        Json(Updated {
            options: body.options,
            deployment,
        }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use panel_core::vhost::Redirect;

    fn body(options: VhostOptions) -> UpdateOptionsBody {
        UpdateOptionsBody {
            options,
            basic_auth: SecretChange::Keep,
            certificate: SecretChange::Keep,
            redeploy: true,
        }
    }

    fn user() -> BasicAuthUser {
        BasicAuthUser {
            username: "alice".into(),
            password: "longenough".into(),
        }
    }

    #[test]
    fn flags_must_match_the_secret_changes() {
        let none = VhostOptions::default();
        let auth = VhostOptions {
            basic_auth: true,
            ..Default::default()
        };
        // Enabling basic auth without users is refused.
        assert!(validate_update(&none, &body(auth.clone())).is_err());
        let mut set = body(auth.clone());
        set.basic_auth = SecretChange::Set(vec![user()]);
        assert!(validate_update(&none, &set).is_ok());
        // Keeping existing users while the flag stays on is fine.
        assert!(validate_update(&auth, &body(auth.clone())).is_ok());
        // Turning the flag off requires an explicit removal.
        assert!(validate_update(&auth, &body(none.clone())).is_err());
        let mut remove = body(none.clone());
        remove.basic_auth = SecretChange::Remove;
        assert!(validate_update(&auth, &remove).is_ok());
        // Weak passwords are refused.
        let mut weak = body(auth);
        weak.basic_auth = SecretChange::Set(vec![BasicAuthUser {
            username: "a".into(),
            password: "x".into(),
        }]);
        assert!(validate_update(&none, &weak).is_err());
    }

    #[test]
    fn certificates_and_redirects_are_checked() {
        let cert = VhostOptions {
            custom_cert: true,
            ..Default::default()
        };
        assert!(validate_update(&VhostOptions::default(), &body(cert.clone())).is_err());
        let mut set = body(cert);
        set.certificate = SecretChange::Set(CustomCertificate {
            certificate_pem: "nope".into(),
            private_key_pem: "nope".into(),
        });
        assert!(validate_update(&VhostOptions::default(), &set).is_err());
        let bad_redirect = VhostOptions {
            redirect: Some(Redirect {
                status: 301,
                target: "javascript:alert(1)".into(),
                preserve_path: false,
            }),
            ..Default::default()
        };
        assert!(validate_update(&VhostOptions::default(), &body(bad_redirect)).is_err());
    }
}
