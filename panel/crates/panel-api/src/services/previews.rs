//! Pull request preview deployments.
//!
//! The project's push webhook also receives pull (merge) request events
//! (`routes/webhooks.rs`). [`handle_event`] turns an authenticated,
//! validated event into a state transition (`panel_core::previews::plan`)
//! under a per-project advisory lock:
//!
//! - opened / new commits: create `<slug>-pr-<n>` (a Compose project on the
//!   PR head branch, inheriting the parent's owner, environment, GitHub App
//!   link and registry) with the domain `<slug>-pr-<n>.<base domain>`, or
//!   record the new head, and queue a deployment;
//! - closed / merged: mark the preview `closing` and tear it down.
//!
//! Requests from forks are never previewed: their code would run with the
//! project's environment. The number of active previews is capped per
//! project; above it the request gets a comment instead of a preview.
//!
//! The queue worker deploys a preview like any service, with the parent's
//! service variables, a wildcard certificate if configured, the
//! [`ResourceLimits`] of the settings on every container, and as an
//! ephemeral project whose volumes are removed with it
//! ([`deploy_options`]). When a preview deployment finishes, the link (or
//! the failure) is posted on the pull request ([`Previews::deployment_finished`])
//! and the comment is edited in place afterwards.
//!
//! Teardown (containers, volumes, vhost, files, rows) runs in the
//! background; the sweeper ([`Previews::start_sweeper`]) expires previews
//! idle for longer than their TTL and retries every unfinished teardown.
//!
//! Comments never carry an error message or a secret: only the preview URL,
//! the commit, and the deployment status.

use crate::db;
use crate::db::previews::{NewPreview, PreviewRow, SettingsRow};
use crate::error::{ApiError, ApiResult};
use crate::services::deploy_queue::DeployQueue;
use crate::services::github_app::{self, GitHubApps};
use crate::services::secret_box::SecretBox;
use chrono::{DateTime, Utc};
use panel_core::adapters::{AdapterPort, CancelDeploymentParams, ComposeProjectActionParams};
use panel_core::error::CoreError;
use panel_core::previews::{
    self, is_expired, PrProvider, PreviewPlan, PreviewStatus, PullRequestEvent, ResourceLimits,
};
use panel_core::types::{validate, ApplicationBuild, ComposeProject, ComposeSource};
use serde_json::{json, Value};
use sqlx::PgPool;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

/// How often idle previews are expired and unfinished teardowns retried.
const SWEEP_INTERVAL: Duration = Duration::from_secs(5 * 60);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
/// Marks the comment scontrol owns on a pull request.
const COMMENT_MARKER: &str = "<!-- scontrol-preview -->";

/// Preview side effects that need HTTP or secrets: Git host comments and
/// the background sweeper.
#[derive(Clone)]
pub struct Previews {
    http: reqwest::Client,
    secrets: SecretBox,
    github: GitHubApps,
    /// Replaces every Git host API root (tests).
    api_base_override: Option<String>,
}

impl Previews {
    pub fn new(secrets: SecretBox, github: GitHubApps) -> Self {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            // Requests carry an API token; never follow them elsewhere.
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("scontrol/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("HTTP client configuration is valid");
        Self {
            http,
            secrets,
            github,
            api_base_override: None,
        }
    }

    /// Sends every Git host API request to `base` (tests).
    #[cfg(test)]
    pub fn with_api_base(secrets: SecretBox, github: GitHubApps, base: &str) -> Self {
        Self {
            api_base_override: Some(base.trim_end_matches('/').to_string()),
            ..Self::new(secrets, github)
        }
    }

    pub fn seal_comment_token(&self, project_id: Uuid, token: &str) -> Vec<u8> {
        self.secrets
            .seal(token.as_bytes(), comment_token_aad(project_id).as_bytes())
    }

    /// Spawns the TTL / teardown sweeper. Call once, from the HTTP process.
    pub fn start_sweeper(&self, db: PgPool, agent: Arc<dyn AdapterPort>) {
        let previews = self.clone();
        tokio::spawn(async move {
            // Let the queue workers settle after a restart first.
            tokio::time::sleep(Duration::from_secs(60)).await;
            loop {
                match sweep(&db, agent.as_ref(), &previews, Utc::now()).await {
                    Ok(report) if report.expired > 0 || report.removed > 0 => {
                        tracing::info!(
                            expired = report.expired,
                            removed = report.removed,
                            "preview sweep"
                        );
                    }
                    Ok(_) => {}
                    Err(error) => tracing::warn!(%error, "preview sweep failed"),
                }
                tokio::time::sleep(SWEEP_INTERVAL).await;
            }
        });
    }

    /// Posts (or edits) the preview comment for a finished deployment, in
    /// the background. A no-op for projects that are not previews.
    pub fn deployment_finished(&self, db: PgPool, project_id: Uuid, deployment_id: Uuid) {
        let previews = self.clone();
        tokio::spawn(async move {
            if let Err(error) = previews
                .comment_deployment(&db, project_id, deployment_id)
                .await
            {
                tracing::warn!(%deployment_id, %error, "preview comment failed");
            }
        });
    }

    async fn comment_deployment(
        &self,
        db: &PgPool,
        project_id: Uuid,
        deployment_id: Uuid,
    ) -> anyhow::Result<()> {
        let Some(preview) = db::previews::find_by_preview_project(db, project_id).await? else {
            return Ok(());
        };
        let Some(deployment) =
            db::compose_projects::find_deployment(db, project_id, deployment_id).await?
        else {
            return Ok(());
        };
        let settings = db::previews::settings(db, preview.parent_project_id).await?;
        let url = preview
            .domain
            .as_deref()
            .map(|domain| preview_url(domain, settings.as_ref().is_some_and(|s| s.https)));
        let commit = deployment
            .commit_sha
            .clone()
            .unwrap_or_else(|| preview.head_sha.clone());
        let body = comment_body(&CommentKind::Deployed {
            url: url.as_deref(),
            commit: &commit,
            status: deployment.status,
            deployment_id,
        });
        if let Some(id) = self.post_for(db, &preview, &body).await {
            db::previews::set_comment_id(db, preview.id, id).await?;
        }
        Ok(())
    }

    /// Posts `body` on the preview's request, editing its comment if it has
    /// one. Failures are logged and yield `None`: a comment never decides
    /// whether a preview exists.
    async fn post_for(&self, db: &PgPool, preview: &PreviewRow, body: &str) -> Option<i64> {
        let provider = PrProvider::from_str(&preview.provider).ok()?;
        self.post(
            db,
            preview.parent_project_id,
            provider,
            preview.pr_number,
            preview.comment_id,
            body,
        )
        .await
    }

    async fn post(
        &self,
        db: &PgPool,
        parent_id: Uuid,
        provider: PrProvider,
        number: i64,
        comment_id: Option<i64>,
        body: &str,
    ) -> Option<i64> {
        match self
            .try_post(db, parent_id, provider, number, comment_id, body)
            .await
        {
            Ok(id) => id,
            Err(error) => {
                tracing::warn!(%parent_id, number, %error, "posting the preview comment failed");
                None
            }
        }
    }

    async fn try_post(
        &self,
        db: &PgPool,
        parent_id: Uuid,
        provider: PrProvider,
        number: i64,
        comment_id: Option<i64>,
        body: &str,
    ) -> anyhow::Result<Option<i64>> {
        let Some(parent) = db::compose_projects::find_by_id(db, parent_id).await? else {
            return Ok(None);
        };
        let Some((url, _)) = parent.source.git_checkout() else {
            return Ok(None);
        };
        let target = comment_target(provider, url, self.api_base_override.as_deref())
            .ok_or_else(|| anyhow::anyhow!("the repository URL names no repository"))?;
        let Some(credential) = self.credential(db, &parent, provider, url).await? else {
            tracing::debug!(%parent_id, "no token to comment on pull requests with");
            return Ok(None);
        };
        let full_body = format!("{COMMENT_MARKER}\n{body}");
        if let Some(id) = comment_id {
            match self
                .send(&target, &credential, number, Some(id), &full_body)
                .await
            {
                Ok(id) => return Ok(Some(id)),
                // The comment was deleted on the Git host: post a new one.
                Err(SendError::NotFound) => {}
                Err(SendError::Other(error)) => return Err(error),
            }
        }
        match self
            .send(&target, &credential, number, None, &full_body)
            .await
        {
            Ok(id) => Ok(Some(id)),
            Err(SendError::NotFound) => anyhow::bail!("the Git host does not know this request"),
            Err(SendError::Other(error)) => Err(error),
        }
    }

    /// A GitHub App installation token (GitHub only) or the stored comment
    /// token, never logged.
    async fn credential(
        &self,
        db: &PgPool,
        parent: &ComposeProject,
        provider: PrProvider,
        repository_url: &str,
    ) -> anyhow::Result<Option<String>> {
        if provider == PrProvider::Github {
            let access = db::compose_projects::git_access(db, parent.id).await?;
            if let (Some(app_id), Some(installation), Some(repository)) = (
                access.github_app_id,
                access.github_installation_id,
                github_app::github_repository(repository_url),
            ) {
                if let Some(app) = db::github_apps::find_sealed(db, app_id).await? {
                    let token = self
                        .github
                        .installation_token_with(
                            &app,
                            installation,
                            &repository,
                            json!({ "pull_requests": "write" }),
                        )
                        .await?;
                    return Ok(Some(token));
                }
            }
        }
        let Some(sealed) = db::previews::comment_token(db, parent.id).await? else {
            return Ok(None);
        };
        let token = self
            .secrets
            .open(&sealed, comment_token_aad(parent.id).as_bytes())?;
        Ok(Some(String::from_utf8(token).map_err(|_| {
            anyhow::anyhow!("stored comment token is not valid UTF-8")
        })?))
    }

    async fn send(
        &self,
        target: &CommentTarget,
        token: &str,
        number: i64,
        comment_id: Option<i64>,
        body: &str,
    ) -> Result<i64, SendError> {
        let (method, url) = target.endpoint(number, comment_id);
        let mut request = self
            .http
            .request(method, url)
            .header("Accept", "application/json");
        request = match target.provider {
            PrProvider::Github => request
                .bearer_auth(token)
                .header("Accept", "application/vnd.github+json")
                .header("X-GitHub-Api-Version", "2022-11-28"),
            PrProvider::Gitea => request.header("Authorization", format!("token {token}")),
            PrProvider::Gitlab => request.header("PRIVATE-TOKEN", token),
        };
        let response = request
            .json(&json!({ "body": body }))
            .send()
            .await
            .map_err(|e| {
                SendError::Other(anyhow::anyhow!(
                    "Git host API request failed: {}",
                    e.without_url()
                ))
            })?;
        let status = response.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(SendError::NotFound);
        }
        if !status.is_success() {
            return Err(SendError::Other(anyhow::anyhow!(
                "the Git host refused the comment (HTTP {})",
                status.as_u16()
            )));
        }
        #[derive(serde::Deserialize)]
        struct Created {
            id: i64,
        }
        response
            .json::<Created>()
            .await
            .map(|created| created.id)
            .map_err(|_| SendError::Other(anyhow::anyhow!("unexpected comment response")))
    }
}

enum SendError {
    NotFound,
    Other(anyhow::Error),
}

fn comment_token_aad(project_id: Uuid) -> String {
    format!("preview_settings:{project_id}:comment_token")
}

/// Where comments for a repository go.
#[derive(Debug, PartialEq, Eq)]
pub struct CommentTarget {
    provider: PrProvider,
    api_root: String,
    /// `owner/name` (GitLab: `group/subgroup/name`).
    repository: String,
}

impl CommentTarget {
    fn endpoint(&self, number: i64, comment_id: Option<i64>) -> (reqwest::Method, String) {
        let root = &self.api_root;
        let repo = &self.repository;
        match (self.provider, comment_id) {
            (PrProvider::Github | PrProvider::Gitea, None) => (
                reqwest::Method::POST,
                format!("{root}/repos/{repo}/issues/{number}/comments"),
            ),
            (PrProvider::Github | PrProvider::Gitea, Some(id)) => (
                reqwest::Method::PATCH,
                format!("{root}/repos/{repo}/issues/comments/{id}"),
            ),
            (PrProvider::Gitlab, None) => (
                reqwest::Method::POST,
                format!(
                    "{root}/projects/{}/merge_requests/{number}/notes",
                    repo.replace('/', "%2F")
                ),
            ),
            (PrProvider::Gitlab, Some(id)) => (
                reqwest::Method::PUT,
                format!(
                    "{root}/projects/{}/merge_requests/{number}/notes/{id}",
                    repo.replace('/', "%2F")
                ),
            ),
        }
    }
}

/// The API root and repository path for commenting on requests of the
/// repository at `url` (HTTPS or SSH remote).
pub fn comment_target(
    provider: PrProvider,
    url: &str,
    api_base_override: Option<&str>,
) -> Option<CommentTarget> {
    let (_, host) = validate::git_remote(url)?;
    let rest = match url.split_once("://") {
        Some((_, rest)) => rest.split_once('/')?.1,
        // scp-like `git@host:owner/repo.git`
        None => url.split_once(':')?.1,
    };
    let repository = rest.trim_end_matches('/');
    let repository = repository.strip_suffix(".git").unwrap_or(repository);
    let segments: Vec<&str> = repository.split('/').collect();
    let valid = segments.len() >= 2
        && segments.iter().all(|segment| {
            !segment.is_empty()
                && *segment != "."
                && *segment != ".."
                && segment
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        });
    if !valid || (provider != PrProvider::Gitlab && segments.len() != 2) {
        return None;
    }
    let api_root = match (api_base_override, provider) {
        (Some(base), _) => base.to_string(),
        (None, PrProvider::Github) if host == github_app::GITHUB_HOST => {
            "https://api.github.com".to_string()
        }
        (None, PrProvider::Github) => format!("https://{host}/api/v3"),
        (None, PrProvider::Gitea) => format!("https://{host}/api/v1"),
        (None, PrProvider::Gitlab) => format!("https://{host}/api/v4"),
    };
    Some(CommentTarget {
        provider,
        api_root,
        repository: repository.to_string(),
    })
}

/// What a comment reports.
pub enum CommentKind<'a> {
    Deployed {
        url: Option<&'a str>,
        commit: &'a str,
        status: panel_core::types::DeploymentStatus,
        deployment_id: Uuid,
    },
    LimitReached {
        limit: u32,
    },
    Removed {
        reason: &'a str,
    },
}

/// Comment text. Never includes an error message: those may name hosts,
/// paths or values the pull request audience must not see.
pub fn comment_body(kind: &CommentKind<'_>) -> String {
    use panel_core::types::DeploymentStatus;
    match kind {
        CommentKind::Deployed {
            url,
            commit,
            status,
            deployment_id,
        } => {
            let short: String = commit.chars().take(7).collect();
            match status {
                DeploymentStatus::Succeeded => format!(
                    "**Preview deployed** for `{short}`\n\n{}\n\n<sub>scontrol deployment `{deployment_id}`</sub>",
                    url.map(|u| format!("🔗 {u}"))
                        .unwrap_or_else(|| "The preview has no domain.".into())
                ),
                DeploymentStatus::Cancelled => format!(
                    "**Preview deployment cancelled** for `{short}`.\n\n<sub>scontrol deployment `{deployment_id}`</sub>"
                ),
                _ => format!(
                    "**Preview deployment failed** for `{short}`. The deployment log is in the scontrol panel.\n\n<sub>scontrol deployment `{deployment_id}`</sub>"
                ),
            }
        }
        CommentKind::LimitReached { limit } => format!(
            "**No preview**: this project already has {limit} active preview(s), its limit. \
             Close another pull request or raise the limit, then push again."
        ),
        CommentKind::Removed { reason } => {
            let why = match *reason {
                "closed" => "the pull request was closed",
                "expired" => "it saw no activity for longer than its time to live",
                "disabled" => "previews were disabled for this project",
                _ => "an operator removed it",
            };
            format!("**Preview removed**: {why}.")
        }
    }
}

fn preview_url(domain: &str, https: bool) -> String {
    format!("{}://{domain}", if https { "https" } else { "http" })
}

/// The parent source on another branch. `None` for sources that do not
/// deploy from Git.
pub fn source_for_branch(source: &ComposeSource, branch: &str) -> Option<ComposeSource> {
    let mut source = source.clone();
    match &mut source {
        ComposeSource::Git { branch: b, .. } => *b = branch.to_string(),
        ComposeSource::Application(spec) => match &mut spec.build {
            ApplicationBuild::Dockerfile { branch: b, .. }
            | ApplicationBuild::Builder { branch: b, .. } => *b = branch.to_string(),
            ApplicationBuild::Image { .. } => return None,
        },
        ComposeSource::Static(spec) => spec.branch = branch.to_string(),
        ComposeSource::Raw { .. } | ComposeSource::Database(_) => return None,
    }
    Some(source)
}

/// What the deploy queue adds to a preview's deployment.
pub struct PreviewDeployOptions {
    /// The parent service, whose service-layer variables the preview uses.
    pub parent_project_id: Uuid,
    pub limits: ResourceLimits,
    /// Wildcard certificate lineage for the preview's HTTPS domain.
    pub certificate: Option<String>,
}

/// `Some` when `project_id` is a preview.
pub async fn deploy_options(
    db: &PgPool,
    project_id: Uuid,
) -> anyhow::Result<Option<PreviewDeployOptions>> {
    let Some(preview) = db::previews::find_by_preview_project(db, project_id).await? else {
        return Ok(None);
    };
    let settings = db::previews::settings(db, preview.parent_project_id).await?;
    // A preview is always limited, even if its settings row is gone.
    let limits = settings.as_ref().map(limits_of).unwrap_or_default();
    Ok(Some(PreviewDeployOptions {
        parent_project_id: preview.parent_project_id,
        limits,
        certificate: settings
            .filter(|s| s.https)
            .and_then(|s| s.wildcard_certificate),
    }))
}

pub fn limits_of(settings: &SettingsRow) -> ResourceLimits {
    ResourceLimits {
        memory_mb: u32::try_from(settings.memory_mb).unwrap_or(512),
        cpu_millis: u32::try_from(settings.cpu_millis).unwrap_or(500),
    }
}

/// Applies a pull request event to `parent`'s previews. Returns the JSON
/// the webhook answers with.
pub async fn handle_event(
    db: &PgPool,
    agent: Arc<dyn AdapterPort>,
    queue: &DeployQueue,
    previews: &Previews,
    parent: &ComposeProject,
    event: &PullRequestEvent,
) -> ApiResult<Value> {
    if db::previews::find_by_preview_project(db, parent.id)
        .await?
        .is_some()
    {
        return Ok(json!({ "ignored": "previews do not have previews" }));
    }
    let Some(settings) = db::previews::settings(db, parent.id)
        .await?
        .filter(|s| s.enabled)
    else {
        return Ok(json!({ "ignored": "previews are disabled for this project" }));
    };
    let number = i64::try_from(event.number)
        .map_err(|_| ApiError::BadRequest("pull request number is out of range".into()))?;
    let slug = previews::preview_slug(&parent.slug, event.number).ok_or_else(|| {
        ApiError::BadRequest("the project slug leaves no room for a preview suffix".into())
    })?;
    let host = previews::preview_host(&slug, &settings.base_domain)
        .ok_or_else(|| ApiError::BadRequest("the preview host is not a valid domain".into()))?;
    let source = source_for_branch(&parent.source, &event.head_branch)
        .ok_or_else(|| ApiError::BadRequest("project does not deploy from Git".into()))?;
    let source_json = serde_json::to_value(&source).map_err(anyhow::Error::from)?;

    let mut tx = db.begin().await?;
    db::previews::lock_parent(&mut tx, parent.id).await?;
    let existing = db::previews::status_in(&mut tx, parent.id, number).await?;
    let active = db::previews::count_active(&mut tx, parent.id).await?;
    let status = existing
        .as_ref()
        .map(|(_, _, status)| PreviewStatus::from_str(status))
        .transpose()
        .map_err(anyhow::Error::msg)?;
    let limit = u32::try_from(settings.max_active).unwrap_or(previews::DEFAULT_MAX_ACTIVE);
    let plan = previews::plan(
        event,
        status,
        u32::try_from(active).unwrap_or(u32::MAX),
        limit,
    );
    let preview_project = match (&plan, &existing) {
        (PreviewPlan::Create, _) => {
            if db::compose_projects::domain_in_use(db, &host).await? {
                return Err(ApiError::Conflict(format!("{host} is already in use")));
            }
            let id = Uuid::new_v4();
            let name = format!("{} PR #{}", truncate_name(&parent.name), event.number);
            let inserted = db::previews::insert_preview(
                &mut tx,
                &NewPreview {
                    parent_project_id: parent.id,
                    preview_project_id: id,
                    name: &name,
                    slug: &slug,
                    source: &source_json,
                    provider: event.provider.as_str(),
                    number,
                    head_branch: &event.head_branch,
                    head_sha: &event.head_sha,
                    domain: &host,
                    service_name: &settings.service_name,
                    container_port: settings.container_port,
                    https: settings.https,
                },
            )
            .await
            .map_err(|e| unique_conflict(e, &slug))?;
            if !inserted {
                return Err(ApiError::Conflict(
                    "no free loopback port for the preview domain".into(),
                ));
            }
            Some(id)
        }
        (PreviewPlan::Redeploy, Some((id, project, _))) => {
            db::previews::touch(
                &mut tx,
                *id,
                *project,
                &event.head_branch,
                &event.head_sha,
                &source_json,
            )
            .await?;
            Some(*project)
        }
        (PreviewPlan::Destroy, Some((id, _, _))) => {
            sqlx::query(
                "UPDATE preview_deployments SET status = 'closing', close_reason = 'closed' \
                 WHERE id = $1",
            )
            .bind(id)
            .execute(&mut *tx)
            .await?;
            None
        }
        _ => None,
    };
    tx.commit().await?;

    let audit = |action: &'static str, extra: Value| {
        let db = db.clone();
        let slug = slug.clone();
        let mut metadata = json!({
            "parent_project_id": parent.id,
            "pr_number": event.number,
            "provider": event.provider.as_str(),
            "commit": event.head_sha,
        });
        if let (Value::Object(m), Value::Object(e)) = (&mut metadata, extra) {
            m.extend(e);
        }
        async move {
            db::audit::record(&db, None, "webhook", action, Some(&slug), metadata)
                .await
                .ok();
        }
    };
    match plan {
        PreviewPlan::Create | PreviewPlan::Redeploy => {
            let project_id = preview_project.expect("set for create and redeploy");
            let created = plan == PreviewPlan::Create;
            if !created {
                if let Some(queued) =
                    db::compose_projects::find_queued_deployment(db, project_id).await?
                {
                    return Ok(json!({ "preview": slug, "coalesced": true, "deployment": queued }));
                }
            }
            let deployment = queue
                .enqueue(db, project_id, &slug, false, "preview")
                .await?;
            audit(
                if created {
                    "create_preview"
                } else {
                    "redeploy_preview"
                },
                json!({ "project_id": project_id, "deployment_id": deployment.id }),
            )
            .await;
            Ok(json!({
                "preview": slug,
                "created": created,
                "coalesced": false,
                "deployment": deployment,
            }))
        }
        PreviewPlan::Destroy => {
            audit("close_preview", json!({ "reason": "closed" })).await;
            spawn_teardown(db.clone(), agent, previews.clone(), parent.id, number);
            Ok(json!({ "preview": slug, "closing": true }))
        }
        PreviewPlan::LimitReached { limit } => {
            let previews = previews.clone();
            let (db, parent_id, provider) = (db.clone(), parent.id, event.provider);
            tokio::spawn(async move {
                let body = comment_body(&CommentKind::LimitReached { limit });
                previews
                    .post(&db, parent_id, provider, number, None, &body)
                    .await;
            });
            Ok(json!({ "ignored": "the active preview limit is reached", "limit": limit }))
        }
        PreviewPlan::Ignore(reason) => Ok(json!({ "ignored": reason })),
    }
}

fn truncate_name(name: &str) -> String {
    name.chars().take(80).collect()
}

fn unique_conflict(error: sqlx::Error, slug: &str) -> ApiError {
    if error
        .as_database_error()
        .is_some_and(|e| e.is_unique_violation())
    {
        ApiError::Conflict(format!(
            "a project named {slug} or its domain already exists"
        ))
    } else {
        error.into()
    }
}

pub fn spawn_teardown(
    db: PgPool,
    agent: Arc<dyn AdapterPort>,
    previews: Previews,
    parent: Uuid,
    number: i64,
) {
    tokio::spawn(async move {
        let row = match db::previews::find_by_pr(&db, parent, number).await {
            Ok(Some(row)) => row,
            Ok(None) => return,
            Err(error) => {
                tracing::warn!(%error, "loading a closing preview failed");
                return;
            }
        };
        if let Err(error) = teardown(&db, agent.as_ref(), &previews, &row).await {
            // The sweeper retries.
            tracing::warn!(preview = %row.slug, %error, "preview teardown failed");
        }
    });
}

/// Removes a `closing` preview: cancels its queued deployments, waits out a
/// running one (cancelling it), then removes containers, volumes, vhost
/// and files on the agent and the project row (which deletes the preview
/// record). Returns false when a running deployment must stop first; the
/// sweeper comes back for it.
pub async fn teardown(
    db: &PgPool,
    agent: &dyn AdapterPort,
    previews: &Previews,
    row: &PreviewRow,
) -> ApiResult<bool> {
    sqlx::query(
        "UPDATE compose_deployments SET status = 'cancelled', finished_at = now() \
         WHERE project_id = $1 AND status = 'queued'",
    )
    .bind(row.preview_project_id)
    .execute(db)
    .await?;
    let running: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM compose_deployments WHERE project_id = $1 AND status = 'running' LIMIT 1",
    )
    .bind(row.preview_project_id)
    .fetch_optional(db)
    .await?;
    if let Some(deployment_id) = running {
        agent
            .cancel_deployment(CancelDeploymentParams { deployment_id })
            .await
            .ok();
        return Ok(false);
    }
    match agent
        .remove_compose_project(ComposeProjectActionParams {
            project_slug: row.slug.clone(),
        })
        .await
    {
        Ok(()) | Err(CoreError::NotFound(_)) => {}
        Err(error) => return Err(error.into()),
    }
    db::compose_projects::delete(db, row.preview_project_id).await?;
    db::audit::record(
        db,
        None,
        "system",
        "remove_preview",
        Some(&row.slug),
        json!({
            "parent_project_id": row.parent_project_id,
            "pr_number": row.pr_number,
            "reason": row.close_reason,
        }),
    )
    .await
    .ok();
    let reason = row.close_reason.as_deref().unwrap_or("removed");
    previews
        .post_for(db, row, &comment_body(&CommentKind::Removed { reason }))
        .await;
    Ok(true)
}

/// Marks every active preview of `parent` closing and tears them all down
/// now. Used when previews are disabled and before the parent is removed.
pub async fn close_all(
    db: &PgPool,
    agent: &dyn AdapterPort,
    previews: &Previews,
    parent: Uuid,
    reason: &str,
) -> ApiResult<usize> {
    db::previews::mark_all_closing(db, parent, reason).await?;
    let mut pending = 0;
    for row in db::previews::list_for_parent(db, parent).await? {
        if !teardown(db, agent, previews, &row).await? {
            pending += 1;
        }
    }
    Ok(pending)
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct SweepReport {
    pub expired: usize,
    pub removed: usize,
}

/// Expires idle previews and tears down every closing one.
pub async fn sweep(
    db: &PgPool,
    agent: &dyn AdapterPort,
    previews: &Previews,
    now: DateTime<Utc>,
) -> ApiResult<SweepReport> {
    let mut report = SweepReport::default();
    for (row, ttl_hours) in db::previews::list_active_with_ttl(db).await? {
        let ttl = u32::try_from(ttl_hours).unwrap_or(previews::DEFAULT_TTL_HOURS);
        if is_expired(row.last_activity_at, ttl, now)
            && db::previews::mark_closing(db, row.id, "expired").await?
        {
            report.expired += 1;
        }
    }
    for row in db::previews::list_closing(db).await? {
        match teardown(db, agent, previews, &row).await {
            Ok(true) => report.removed += 1,
            Ok(false) => {}
            Err(error) => tracing::warn!(preview = %row.slug, %error, "preview teardown failed"),
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use panel_core::types::DeploymentStatus;

    #[test]
    fn comment_targets_cover_each_provider_and_remote_style() {
        let gh =
            comment_target(PrProvider::Github, "https://github.com/acme/shop.git", None).unwrap();
        assert_eq!(gh.api_root, "https://api.github.com");
        assert_eq!(gh.repository, "acme/shop");
        assert_eq!(
            gh.endpoint(12, None).1,
            "https://api.github.com/repos/acme/shop/issues/12/comments"
        );
        assert_eq!(
            gh.endpoint(12, Some(99)),
            (
                reqwest::Method::PATCH,
                "https://api.github.com/repos/acme/shop/issues/comments/99".into()
            )
        );
        let ghe = comment_target(
            PrProvider::Github,
            "git@git.corp.example:acme/shop.git",
            None,
        )
        .unwrap();
        assert_eq!(ghe.api_root, "https://git.corp.example/api/v3");

        let gitea = comment_target(
            PrProvider::Gitea,
            "ssh://git@code.example.org/acme/shop",
            None,
        )
        .unwrap();
        assert_eq!(gitea.api_root, "https://code.example.org/api/v1");

        let gl = comment_target(
            PrProvider::Gitlab,
            "https://gitlab.com/group/sub/shop.git",
            None,
        )
        .unwrap();
        assert_eq!(
            gl.endpoint(7, None).1,
            "https://gitlab.com/api/v4/projects/group%2Fsub%2Fshop/merge_requests/7/notes"
        );
        assert_eq!(gl.endpoint(7, Some(3)).0, reqwest::Method::PUT);

        // Nested paths only exist on GitLab; junk never becomes a URL path.
        assert!(comment_target(PrProvider::Github, "https://github.com/a/b/c", None).is_none());
        assert!(comment_target(PrProvider::Github, "https://github.com/acme", None).is_none());
        assert!(comment_target(PrProvider::Github, "/etc/passwd", None).is_none());
    }

    #[test]
    fn comments_carry_links_never_errors() {
        let id = Uuid::new_v4();
        let ok = comment_body(&CommentKind::Deployed {
            url: Some("https://shop-pr-3.preview.example.com"),
            commit: "0123456789abcdef",
            status: DeploymentStatus::Succeeded,
            deployment_id: id,
        });
        assert!(ok.contains("https://shop-pr-3.preview.example.com"));
        assert!(ok.contains("`0123456`"));
        let failed = comment_body(&CommentKind::Deployed {
            url: Some("https://shop-pr-3.preview.example.com"),
            commit: "0123456789abcdef",
            status: DeploymentStatus::Failed,
            deployment_id: id,
        });
        assert!(failed.contains("failed"));
        assert!(!failed.contains("https://"));
        assert!(comment_body(&CommentKind::LimitReached { limit: 5 }).contains('5'));
        assert!(comment_body(&CommentKind::Removed { reason: "expired" }).contains("time to live"));
    }

    #[test]
    fn previews_switch_the_branch_of_git_sources_only() {
        let git = ComposeSource::Git {
            repository_url: "https://github.com/acme/shop.git".into(),
            branch: "main".into(),
            compose_path: "compose.yml".into(),
        };
        match source_for_branch(&git, "feature/x").unwrap() {
            ComposeSource::Git { branch, .. } => assert_eq!(branch, "feature/x"),
            other => panic!("unexpected {other:?}"),
        }
        let raw = ComposeSource::Raw {
            compose_yaml: "services: {}".into(),
        };
        assert!(source_for_branch(&raw, "feature/x").is_none());
    }
}
