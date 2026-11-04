//! Pull request preview deployments: the pure rules shared by panel-api and
//! the agent.
//!
//! A Git-sourced Compose project with previews enabled gets one temporary
//! project `<slug>-pr-<n>` per open pull (merge) request, served on
//! `<slug>-pr-<n>.<base domain>`. Opening or pushing to the request deploys
//! it, closing or merging it destroys it, and a preview that sees no
//! activity for its TTL is destroyed by the panel's sweeper. The number of
//! active previews per project is capped, and every preview container runs
//! under memory, CPU and PID limits so a preview cannot starve production.
//!
//! Everything here is a pure function of validated input; I/O lives in
//! `panel-api` (`services/previews.rs`) and the agent.

use crate::types::validate;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;

/// Largest pull request number accepted (seven digits).
pub const MAX_PR_NUMBER: u64 = 9_999_999;
/// `-pr-` plus seven digits.
const PREVIEW_SUFFIX_MAX: usize = 4 + 7;
/// Longest project slug [`crate::types::validate::is_valid_project_slug`]
/// accepts.
const MAX_SLUG_LEN: usize = 48;

pub const DEFAULT_MAX_ACTIVE: u32 = 5;
pub const MAX_ACTIVE_LIMIT: u32 = 50;
pub const DEFAULT_TTL_HOURS: u32 = 48;
pub const MAX_TTL_HOURS: u32 = 30 * 24;
/// Process limit for every preview container (fork bombs, runaway workers).
pub const PREVIEW_PIDS_LIMIT: u32 = 1024;

/// Per-container limits applied to every service of an ephemeral
/// deployment through the generated Compose override.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ResourceLimits {
    /// Memory limit in MiB (also the memory+swap limit: no swap).
    pub memory_mb: u32,
    /// CPU limit in thousandths of a CPU (`500` = half a core).
    pub cpu_millis: u32,
}

impl Default for ResourceLimits {
    fn default() -> Self {
        Self {
            memory_mb: 512,
            cpu_millis: 500,
        }
    }
}

impl ResourceLimits {
    pub fn validate(&self) -> Result<(), String> {
        if !(64..=65_536).contains(&self.memory_mb) {
            return Err("memory limit must be 64..=65536 MiB".into());
        }
        if !(100..=64_000).contains(&self.cpu_millis) {
            return Err("CPU limit must be 100..=64000 millicores".into());
        }
        Ok(())
    }

    /// `mem_limit` / `memswap_limit` value, e.g. `512m`.
    pub fn memory(&self) -> String {
        format!("{}m", self.memory_mb)
    }

    /// `cpus` value, e.g. `0.5`.
    pub fn cpus(&self) -> String {
        let whole = self.cpu_millis / 1000;
        let fraction = self.cpu_millis % 1000;
        if fraction == 0 {
            format!("{whole}")
        } else {
            let digits = format!("{fraction:03}");
            format!("{whole}.{}", digits.trim_end_matches('0'))
        }
    }
}

/// Whether a project's slug leaves room for the `-pr-<n>` suffix.
pub fn parent_slug_supports_previews(slug: &str) -> bool {
    validate::is_valid_project_slug(slug) && slug.len() <= MAX_SLUG_LEN - PREVIEW_SUFFIX_MAX
}

/// The preview project slug `<parent>-pr-<number>`, or `None` when the
/// number is out of range or the result is not a valid slug.
pub fn preview_slug(parent: &str, number: u64) -> Option<String> {
    if number == 0 || number > MAX_PR_NUMBER || !parent_slug_supports_previews(parent) {
        return None;
    }
    let slug = format!("{parent}-pr-{number}");
    validate::is_valid_project_slug(&slug).then_some(slug)
}

/// Normalises a base domain entered as `preview.example.com` or
/// `*.preview.example.com` (lower case, wildcard label removed).
pub fn normalize_base_domain(input: &str) -> Option<String> {
    let trimmed = input.trim().trim_end_matches('.').to_ascii_lowercase();
    let base = trimmed.strip_prefix("*.").unwrap_or(&trimmed);
    validate::is_valid_domain(base).then(|| base.to_string())
}

/// The host a preview is served on: `<preview slug>.<base domain>`.
pub fn preview_host(preview_slug: &str, base_domain: &str) -> Option<String> {
    let host = format!("{preview_slug}.{base_domain}");
    validate::is_valid_domain(&host).then_some(host)
}

/// Git hosting service a pull request event came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PrProvider {
    Github,
    Gitlab,
    Gitea,
}

impl PrProvider {
    pub fn as_str(self) -> &'static str {
        match self {
            PrProvider::Github => "github",
            PrProvider::Gitlab => "gitlab",
            PrProvider::Gitea => "gitea",
        }
    }
}

impl fmt::Display for PrProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for PrProvider {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "github" => Ok(PrProvider::Github),
            "gitlab" => Ok(PrProvider::Gitlab),
            "gitea" => Ok(PrProvider::Gitea),
            other => Err(format!("unknown pull request provider: {other}")),
        }
    }
}

/// What happened to a pull request, normalised across providers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrAction {
    /// Opened or reopened.
    Opened,
    /// New commits on the head branch.
    Updated,
    /// Closed or merged.
    Closed,
}

/// An authenticated, validated pull request event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequestEvent {
    pub provider: PrProvider,
    pub action: PrAction,
    pub number: u64,
    pub head_branch: String,
    pub head_sha: String,
    /// The head branch lives in the project's own repository. Requests from
    /// forks run code nobody with push access has vetted, with the
    /// project's environment, so they are never previewed.
    pub same_repository: bool,
}

impl PullRequestEvent {
    /// The number, branch and commit are about to name a project, a Git
    /// ref and a log line: reject anything outside the allow-lists.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.number == 0 || self.number > MAX_PR_NUMBER {
            return Err("pull request number is out of range");
        }
        if !validate::is_valid_git_branch(&self.head_branch) {
            return Err("pull request head branch is not a valid branch name");
        }
        if !is_valid_commit_sha(&self.head_sha) {
            return Err("pull request head commit is not a commit SHA");
        }
        Ok(())
    }
}

/// A hexadecimal commit SHA (abbreviated or full SHA-1/SHA-256).
pub fn is_valid_commit_sha(sha: &str) -> bool {
    (7..=64).contains(&sha.len()) && sha.chars().all(|c| c.is_ascii_hexdigit())
}

/// Lifecycle state of a stored preview.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PreviewStatus {
    /// Deployed (or deploying) and counted against the limit.
    Active,
    /// Closed, merged, expired, or removed by an operator: waiting for its
    /// containers, vhost and files to be torn down.
    Closing,
}

impl PreviewStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            PreviewStatus::Active => "active",
            PreviewStatus::Closing => "closing",
        }
    }
}

impl fmt::Display for PreviewStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for PreviewStatus {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "active" => Ok(PreviewStatus::Active),
            "closing" => Ok(PreviewStatus::Closing),
            other => Err(format!("unknown preview status: {other}")),
        }
    }
}

/// What a pull request event does to the project's previews.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreviewPlan {
    /// Create `<slug>-pr-<n>` and queue its first deployment.
    Create,
    /// Record the new head and queue a deployment of the existing preview.
    Redeploy,
    /// Mark the preview closing and tear it down.
    Destroy,
    /// No preview may be created: the project already has `limit` active.
    LimitReached { limit: u32 },
    /// Nothing to do, with the reason reported to the Git host.
    Ignore(&'static str),
}

/// The state transition for `event`, given the request's existing preview
/// (if any), how many previews of the project are active, and the limit.
pub fn plan(
    event: &PullRequestEvent,
    existing: Option<PreviewStatus>,
    active: u32,
    limit: u32,
) -> PreviewPlan {
    match event.action {
        PrAction::Closed => match existing {
            Some(PreviewStatus::Active) => PreviewPlan::Destroy,
            Some(PreviewStatus::Closing) => PreviewPlan::Ignore("preview is already being removed"),
            None => PreviewPlan::Ignore("no preview exists for this pull request"),
        },
        PrAction::Opened | PrAction::Updated => {
            if !event.same_repository {
                return PreviewPlan::Ignore("pull requests from forks are not previewed");
            }
            match existing {
                Some(PreviewStatus::Active) => PreviewPlan::Redeploy,
                Some(PreviewStatus::Closing) => {
                    PreviewPlan::Ignore("the previous preview is still being removed; push again")
                }
                None if active >= limit => PreviewPlan::LimitReached { limit },
                None => PreviewPlan::Create,
            }
        }
    }
}

/// Whether a preview last active at `last_activity` has outlived its TTL.
pub fn is_expired(last_activity: DateTime<Utc>, ttl_hours: u32, now: DateTime<Utc>) -> bool {
    now - last_activity >= Duration::hours(i64::from(ttl_hours))
}

/// Validated preview settings of a project.
pub fn validate_limits(max_active: u32, ttl_hours: u32) -> Result<(), String> {
    if !(1..=MAX_ACTIVE_LIMIT).contains(&max_active) {
        return Err(format!(
            "the active preview limit must be 1..={MAX_ACTIVE_LIMIT}"
        ));
    }
    if !(1..=MAX_TTL_HOURS).contains(&ttl_hours) {
        return Err(format!("the preview TTL must be 1..={MAX_TTL_HOURS} hours"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(action: PrAction, same_repository: bool) -> PullRequestEvent {
        PullRequestEvent {
            provider: PrProvider::Github,
            action,
            number: 42,
            head_branch: "feature/login".into(),
            head_sha: "0123456789abcdef0123456789abcdef01234567".into(),
            same_repository,
        }
    }

    #[test]
    fn opening_creates_until_the_limit_and_redeploys_existing_previews() {
        let opened = event(PrAction::Opened, true);
        assert_eq!(plan(&opened, None, 0, 5), PreviewPlan::Create);
        assert_eq!(plan(&opened, None, 4, 5), PreviewPlan::Create);
        assert_eq!(
            plan(&opened, None, 5, 5),
            PreviewPlan::LimitReached { limit: 5 }
        );
        // Existing previews never count against their own redeploy.
        assert_eq!(
            plan(&opened, Some(PreviewStatus::Active), 5, 5),
            PreviewPlan::Redeploy
        );
        let synced = event(PrAction::Updated, true);
        assert_eq!(
            plan(&synced, Some(PreviewStatus::Active), 1, 5),
            PreviewPlan::Redeploy
        );
        // A push to a request whose preview expired brings it back.
        assert_eq!(plan(&synced, None, 1, 5), PreviewPlan::Create);
        assert!(matches!(
            plan(&synced, Some(PreviewStatus::Closing), 1, 5),
            PreviewPlan::Ignore(_)
        ));
    }

    #[test]
    fn closing_destroys_only_active_previews() {
        let closed = event(PrAction::Closed, true);
        assert_eq!(
            plan(&closed, Some(PreviewStatus::Active), 3, 5),
            PreviewPlan::Destroy
        );
        assert!(matches!(
            plan(&closed, Some(PreviewStatus::Closing), 3, 5),
            PreviewPlan::Ignore(_)
        ));
        assert!(matches!(plan(&closed, None, 3, 5), PreviewPlan::Ignore(_)));
        // A fork's preview never exists, but closing still cleans up.
        let fork_closed = event(PrAction::Closed, false);
        assert_eq!(
            plan(&fork_closed, Some(PreviewStatus::Active), 1, 5),
            PreviewPlan::Destroy
        );
    }

    #[test]
    fn forks_are_never_previewed() {
        for action in [PrAction::Opened, PrAction::Updated] {
            assert_eq!(
                plan(&event(action, false), None, 0, 5),
                PreviewPlan::Ignore("pull requests from forks are not previewed")
            );
        }
    }

    #[test]
    fn ttl_expiry_is_measured_from_the_last_activity() {
        let now = Utc::now();
        assert!(!is_expired(now - Duration::hours(47), 48, now));
        assert!(is_expired(now - Duration::hours(48), 48, now));
        assert!(is_expired(now - Duration::days(9), 48, now));
        assert!(!is_expired(now, 1, now));
    }

    #[test]
    fn slugs_and_hosts_are_validated() {
        assert_eq!(preview_slug("shop", 7).as_deref(), Some("shop-pr-7"));
        assert_eq!(
            preview_slug("shop", MAX_PR_NUMBER).as_deref(),
            Some("shop-pr-9999999")
        );
        assert_eq!(preview_slug("shop", 0), None);
        assert_eq!(preview_slug("shop", MAX_PR_NUMBER + 1), None);
        assert_eq!(preview_slug("Shop; rm -rf /", 1), None);
        let longest = "a".repeat(37);
        assert!(preview_slug(&longest, MAX_PR_NUMBER).is_some());
        assert!(!parent_slug_supports_previews(&"a".repeat(38)));

        assert_eq!(
            normalize_base_domain("*.Preview.Example.com.").as_deref(),
            Some("preview.example.com")
        );
        assert_eq!(normalize_base_domain("*.*.example.com"), None);
        assert_eq!(normalize_base_domain("example.com/x"), None);
        assert_eq!(
            preview_host("shop-pr-7", "preview.example.com").as_deref(),
            Some("shop-pr-7.preview.example.com")
        );
    }

    #[test]
    fn events_reject_injection_in_branch_sha_and_number() {
        let mut ok = event(PrAction::Opened, true);
        assert!(ok.validate().is_ok());
        ok.head_branch = "main; rm -rf /".into();
        assert!(ok.validate().is_err());
        let mut bad_sha = event(PrAction::Opened, true);
        bad_sha.head_sha = "--upload-pack=evil".into();
        assert!(bad_sha.validate().is_err());
        let mut bad_number = event(PrAction::Opened, true);
        bad_number.number = 0;
        assert!(bad_number.validate().is_err());
        let mut traversal = event(PrAction::Opened, true);
        traversal.head_branch = "a/../../etc".into();
        assert!(traversal.validate().is_err());
    }

    #[test]
    fn resource_limits_render_compose_values() {
        let limits = ResourceLimits::default();
        assert!(limits.validate().is_ok());
        assert_eq!(limits.memory(), "512m");
        assert_eq!(limits.cpus(), "0.5");
        assert_eq!(
            ResourceLimits {
                memory_mb: 1024,
                cpu_millis: 2000
            }
            .cpus(),
            "2"
        );
        assert_eq!(
            ResourceLimits {
                memory_mb: 1024,
                cpu_millis: 1250
            }
            .cpus(),
            "1.25"
        );
        assert!(ResourceLimits {
            memory_mb: 8,
            cpu_millis: 500
        }
        .validate()
        .is_err());
        assert!(ResourceLimits {
            memory_mb: 512,
            cpu_millis: 0
        }
        .validate()
        .is_err());
        assert!(validate_limits(5, 48).is_ok());
        assert!(validate_limits(0, 48).is_err());
        assert!(validate_limits(5, 0).is_err());
        assert!(validate_limits(5, MAX_TTL_HOURS + 1).is_err());
    }
}
