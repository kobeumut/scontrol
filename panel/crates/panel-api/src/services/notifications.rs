//! Deployment, scheduled-job, and metric-alert notifications: Slack, Discord, Telegram,
//! ntfy, Gotify, a generic signed webhook, and SMTP.
//!
//! Each channel has non-secret settings (`ChannelConfig`, stored as JSON)
//! and at most one secret (webhook URL, bot/app token, SMTP password,
//! signing secret) that is sealed with [`SecretBox`] and never returned by
//! the API. Delivery runs in the background after a deployment finishes and
//! records only a sanitized error on the channel: request errors are
//! stripped of their URL (which carries the secret for Slack, Discord, and
//! Telegram) and every message is redacted against the secret.

use super::secret_box::SecretBox;
use crate::db;
use hmac::{Hmac, Mac};
use once_cell::sync::Lazy;
use panel_core::schedule::{JobRunInfo, ScheduledJob};
use panel_core::types::{validate, DeploymentStatus};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::Sha256;
use sqlx::PgPool;
use std::time::Duration;
use uuid::Uuid;

/// Events a channel can subscribe to.
pub const EVENTS: &[&str] = &[
    "deploy_succeeded",
    "deploy_failed",
    "deploy_cancelled",
    "job_failed",
    "metric_alert",
    "metric_resolved",
];

const SEND_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_ERROR_LEN: usize = 300;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SmtpSecurity {
    /// Implicit TLS (SMTPS, usually port 465).
    Tls,
    /// STARTTLS, required (usually port 587).
    Starttls,
    /// Plain SMTP; accepted only for a loopback relay such as a local MTA.
    None,
}

/// Non-secret channel settings. The secret each kind uses is described by
/// [`ChannelConfig::secret_role`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ChannelConfig {
    Slack {},
    Discord {},
    Telegram {
        chat_id: String,
    },
    Ntfy {
        server_url: String,
        topic: String,
    },
    Gotify {
        server_url: String,
    },
    Webhook {
        url: String,
    },
    Email {
        host: String,
        port: u16,
        security: SmtpSecurity,
        #[serde(default)]
        username: Option<String>,
        from: String,
        to: Vec<String>,
    },
}

/// Whether a kind's secret is required, and what it is.
pub struct SecretRole {
    pub required: bool,
    pub label: &'static str,
}

static TELEGRAM_CHAT_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^(-?[0-9]{1,20}|@[A-Za-z][A-Za-z0-9_]{4,31})$").unwrap());
static TELEGRAM_TOKEN_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^[0-9]{1,20}:[A-Za-z0-9_-]{30,100}$").unwrap());
static NTFY_TOPIC_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"^[A-Za-z0-9_-]{1,64}$").unwrap());

impl ChannelConfig {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Slack {} => "slack",
            Self::Discord {} => "discord",
            Self::Telegram { .. } => "telegram",
            Self::Ntfy { .. } => "ntfy",
            Self::Gotify { .. } => "gotify",
            Self::Webhook { .. } => "webhook",
            Self::Email { .. } => "email",
        }
    }

    pub fn secret_role(&self) -> SecretRole {
        let (required, label) = match self {
            Self::Slack {} | Self::Discord {} => (true, "webhook URL"),
            Self::Telegram { .. } => (true, "bot token"),
            Self::Ntfy { .. } => (false, "access token"),
            Self::Gotify { .. } => (true, "application token"),
            Self::Webhook { .. } => (false, "signing secret"),
            Self::Email { username, .. } => (username.is_some(), "SMTP password"),
        };
        SecretRole { required, label }
    }

    /// Validates settings and the secret that will be stored with them.
    pub fn validate(&self, secret: Option<&str>) -> Result<(), String> {
        let role = self.secret_role();
        match secret {
            None if role.required => return Err(format!("the {} is required", role.label)),
            Some(s) if s.len() > 2048 || s.chars().any(char::is_control) => {
                return Err(format!("invalid {}", role.label));
            }
            _ => {}
        }
        match self {
            Self::Slack {} => {
                let url = check_url(secret.unwrap_or_default(), "webhook URL", true)?;
                if url.host_str() != Some("hooks.slack.com") {
                    return Err("a Slack webhook URL starts with https://hooks.slack.com/".into());
                }
            }
            Self::Discord {} => {
                let url = check_url(secret.unwrap_or_default(), "webhook URL", true)?;
                let host_ok = matches!(url.host_str(), Some("discord.com" | "discordapp.com"));
                if !host_ok || !url.path().starts_with("/api/webhooks/") {
                    return Err(
                        "a Discord webhook URL starts with https://discord.com/api/webhooks/"
                            .into(),
                    );
                }
            }
            Self::Telegram { chat_id } => {
                if !TELEGRAM_CHAT_RE.is_match(chat_id) {
                    return Err("chat ID must be a number or an @channel name".into());
                }
                if !secret.is_some_and(|t| TELEGRAM_TOKEN_RE.is_match(t)) {
                    return Err("bot token must look like 123456:ABC… (from @BotFather)".into());
                }
            }
            Self::Ntfy { server_url, topic } => {
                check_url(server_url, "server URL", false)?;
                if !NTFY_TOPIC_RE.is_match(topic) {
                    return Err("topic may use letters, digits, '-' and '_' only".into());
                }
            }
            Self::Gotify { server_url } => {
                check_url(server_url, "server URL", false)?;
            }
            Self::Webhook { url } => {
                check_url(url, "URL", false)?;
            }
            Self::Email {
                host,
                port,
                security,
                username,
                from,
                to,
            } => {
                let loopback = matches!(host.as_str(), "localhost" | "127.0.0.1" | "::1");
                let host_ok = loopback
                    || validate::is_valid_domain(host)
                    || host.parse::<std::net::IpAddr>().is_ok();
                if !host_ok {
                    return Err("invalid SMTP host".into());
                }
                if *port == 0 {
                    return Err("invalid SMTP port".into());
                }
                if *security == SmtpSecurity::None && !loopback {
                    return Err("unencrypted SMTP is allowed only to a loopback relay".into());
                }
                if username.as_deref().is_some_and(|u| {
                    u.is_empty() || u.len() > 200 || u.chars().any(char::is_control)
                }) {
                    return Err("invalid SMTP username".into());
                }
                from.parse::<lettre::message::Mailbox>()
                    .map_err(|_| "invalid sender address".to_string())?;
                if to.is_empty() || to.len() > 10 {
                    return Err("give between 1 and 10 recipients".into());
                }
                for address in to {
                    address
                        .parse::<lettre::message::Mailbox>()
                        .map_err(|_| format!("invalid recipient address {address:?}"))?;
                }
            }
        }
        Ok(())
    }
}

fn check_url(raw: &str, field: &str, https_only: bool) -> Result<reqwest::Url, String> {
    let invalid = || format!("invalid {field}");
    if raw.is_empty() || raw.len() > 512 || raw.chars().any(char::is_whitespace) {
        return Err(invalid());
    }
    let url = reqwest::Url::parse(raw).map_err(|_| invalid())?;
    let scheme_ok = url.scheme() == "https" || (!https_only && url.scheme() == "http");
    if !scheme_ok {
        return Err(format!(
            "{field} must use {}",
            if https_only { "https" } else { "http or https" }
        ));
    }
    if !url.username().is_empty() || url.password().is_some() || url.host_str().is_none() {
        return Err(format!("{field} must not contain credentials"));
    }
    Ok(url)
}

pub fn validate_events(events: &[String]) -> Result<(), String> {
    if events.is_empty() {
        return Err("choose at least one event".into());
    }
    match events.iter().find(|e| !EVENTS.contains(&e.as_str())) {
        Some(bad) => Err(format!("unknown event {bad:?}")),
        None => Ok(()),
    }
}

/// What gets sent: a title and plain-text body for chat channels, plus
/// structured data for the generic webhook.
#[derive(Debug, Clone)]
pub struct Notification {
    pub event: &'static str,
    pub title: String,
    pub body: String,
    pub data: serde_json::Value,
    /// Raises priority where the channel supports it.
    pub urgent: bool,
}

impl Notification {
    pub fn test() -> Self {
        Self {
            event: "test",
            title: "scontrol test notification".into(),
            body: "This channel is set up correctly.".into(),
            data: json!({}),
            urgent: false,
        }
    }

    fn deployment(
        project: &panel_core::types::ComposeProject,
        deployment: &panel_core::types::DeploymentInfo,
    ) -> Option<Self> {
        let (event, verb, urgent) = match deployment.status {
            DeploymentStatus::Succeeded => ("deploy_succeeded", "succeeded", false),
            DeploymentStatus::Failed => ("deploy_failed", "failed", true),
            DeploymentStatus::Cancelled => ("deploy_cancelled", "was cancelled", false),
            _ => return None,
        };
        let seconds = match (deployment.started_at, deployment.finished_at) {
            (Some(start), Some(end)) => Some((end - start).num_seconds().max(0)),
            _ => None,
        };
        let mut body = format!(
            "Project: {} ({})\nTrigger: {}",
            project.name, project.slug, deployment.trigger
        );
        if let Some(target) = deployment.rollback_of {
            body.push_str(&format!("\nRolled back to deployment: {target}"));
        }
        if let Some(sha) = &deployment.commit_sha {
            body.push_str(&format!("\nCommit: {}", &sha[..sha.len().min(12)]));
        }
        if let Some(seconds) = seconds {
            body.push_str(&format!("\nDuration: {seconds}s"));
        }
        if let Some(error) = &deployment.error_message {
            let error: String = error.chars().take(500).collect();
            body.push_str(&format!("\nError: {error}"));
        }
        Some(Self {
            event,
            title: format!(
                "{} {verb}: {}",
                if deployment.rollback_of.is_some() {
                    "Rollback"
                } else {
                    "Deployment"
                },
                project.name
            ),
            body,
            data: json!({
                "project": { "id": project.id, "name": project.name, "slug": project.slug },
                "deployment": {
                    "id": deployment.id,
                    "status": deployment.status,
                    "trigger": deployment.trigger,
                    "commit_sha": deployment.commit_sha,
                    "rollback_of": deployment.rollback_of,
                    "error_message": deployment.error_message,
                    "queued_at": deployment.queued_at,
                    "started_at": deployment.started_at,
                    "finished_at": deployment.finished_at,
                    "duration_seconds": seconds,
                },
            }),
            urgent,
        })
    }
}

impl Notification {
    /// A failed scheduled-job run. Command output is not included: it can
    /// hold data that should not leave the server, and the log is one click
    /// away in the panel.
    fn job_run_failed(
        project: &panel_core::types::ComposeProject,
        job: &ScheduledJob,
        run: &JobRunInfo,
    ) -> Self {
        let seconds = run
            .finished_at
            .map(|end| (end - run.started_at).num_seconds().max(0));
        let mut body = format!(
            "Project: {} ({})\nService: {}\nTrigger: {}",
            project.name,
            project.slug,
            job.service_name,
            run.trigger.as_str()
        );
        if let Some(code) = run.exit_code {
            body.push_str(&format!("\nExit code: {code}"));
        }
        if let Some(seconds) = seconds {
            body.push_str(&format!("\nDuration: {seconds}s"));
        }
        if let Some(error) = &run.error_message {
            let error: String = error.chars().take(500).collect();
            body.push_str(&format!("\nError: {error}"));
        }
        Self {
            event: "job_failed",
            title: format!("Scheduled job failed: {} ({})", job.name, project.name),
            body,
            data: json!({
                "project": { "id": project.id, "name": project.name, "slug": project.slug },
                "job": {
                    "id": job.id,
                    "name": job.name,
                    "service_name": job.service_name,
                    "schedule": job.schedule,
                    "timezone": job.timezone,
                },
                "run": {
                    "id": run.id,
                    "status": run.status,
                    "trigger": run.trigger,
                    "exit_code": run.exit_code,
                    "error_message": run.error_message,
                    "started_at": run.started_at,
                    "finished_at": run.finished_at,
                    "duration_seconds": seconds,
                },
            }),
            urgent: true,
        }
    }
}

#[derive(Clone)]
pub struct Notifier {
    secrets: SecretBox,
    http: reqwest::Client,
}

impl Notifier {
    pub fn new(secrets: SecretBox) -> Self {
        let http = reqwest::Client::builder()
            .timeout(SEND_TIMEOUT)
            // A redirect could carry a token-bearing request to another host.
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("scontrol/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("static HTTP client configuration");
        Self { secrets, http }
    }

    pub fn secrets(&self) -> &SecretBox {
        &self.secrets
    }

    /// Associated data binding a sealed secret to its channel and kind, so a
    /// kind change cannot reuse a secret meant for another service.
    pub fn secret_aad(channel_id: Uuid, kind: &str) -> Vec<u8> {
        let mut aad = channel_id.as_bytes().to_vec();
        aad.push(b':');
        aad.extend_from_slice(kind.as_bytes());
        aad
    }

    /// Notifies subscribed channels about a finished deployment, in the
    /// background. Never fails the caller.
    pub fn deployment_finished(&self, db: PgPool, project_id: Uuid, deployment_id: Uuid) {
        let notifier = self.clone();
        tokio::spawn(async move {
            if let Err(error) = notifier
                .notify_deployment(&db, project_id, deployment_id)
                .await
            {
                tracing::warn!(%deployment_id, %error, "deployment notification failed");
            }
        });
    }

    /// Sends `notification` to every channel subscribed to its event, in
    /// the background. Never fails the caller.
    pub fn broadcast(&self, db: PgPool, notification: Notification) {
        let notifier = self.clone();
        tokio::spawn(async move {
            let channels = match db::notifications::list_for_event(&db, notification.event).await {
                Ok(channels) => channels,
                Err(error) => {
                    tracing::warn!(event = notification.event, %error, "notification lookup failed");
                    return;
                }
            };
            futures::future::join_all(
                channels
                    .iter()
                    .map(|channel| notifier.deliver_and_record(&db, channel, &notification)),
            )
            .await;
        });
    }

    /// Notifies `job_failed` channels about a failed run, in the background.
    pub fn job_run_failed(&self, db: PgPool, run: JobRunInfo) {
        let notifier = self.clone();
        tokio::spawn(async move {
            if let Err(error) = notifier.notify_job_run(&db, &run).await {
                tracing::warn!(run_id = %run.id, %error, "job notification failed");
            }
        });
    }

    async fn notify_job_run(&self, db: &PgPool, run: &JobRunInfo) -> anyhow::Result<()> {
        let Some(job) = db::scheduled_jobs::find_by_id(db, run.job_id).await? else {
            return Ok(());
        };
        let Some(project) = db::compose_projects::find_by_id(db, job.compose_project_id).await?
        else {
            return Ok(());
        };
        let notification = Notification::job_run_failed(&project, &job, run);
        let channels = db::notifications::list_for_event(db, notification.event).await?;
        futures::future::join_all(
            channels
                .iter()
                .map(|channel| self.deliver_and_record(db, channel, &notification)),
        )
        .await;
        Ok(())
    }

    async fn notify_deployment(
        &self,
        db: &PgPool,
        project_id: Uuid,
        deployment_id: Uuid,
    ) -> anyhow::Result<()> {
        let Some(project) = db::compose_projects::find_by_id(db, project_id).await? else {
            return Ok(());
        };
        let Some(deployment) =
            db::compose_projects::find_deployment(db, project_id, deployment_id).await?
        else {
            return Ok(());
        };
        let Some(notification) = Notification::deployment(&project, &deployment) else {
            return Ok(());
        };
        let channels = db::notifications::list_for_event(db, notification.event).await?;
        futures::future::join_all(
            channels
                .iter()
                .map(|channel| self.deliver_and_record(db, channel, &notification)),
        )
        .await;
        Ok(())
    }

    /// Sends one notification and records the outcome on the channel.
    pub async fn deliver_and_record(
        &self,
        db: &PgPool,
        channel: &db::notifications::ChannelRow,
        notification: &Notification,
    ) -> Result<(), String> {
        let result = self.deliver(channel, notification).await;
        if let Err(error) =
            db::notifications::record_result(db, channel.id, result.as_ref().err()).await
        {
            tracing::warn!(channel_id = %channel.id, %error, "recording notification result failed");
        }
        result
    }

    async fn deliver(
        &self,
        channel: &db::notifications::ChannelRow,
        notification: &Notification,
    ) -> Result<(), String> {
        let config: ChannelConfig = serde_json::from_value(channel.config.clone())
            .map_err(|_| "stored channel settings are invalid".to_string())?;
        let secret = match &channel.secret_sealed {
            Some(sealed) => {
                let aad = Self::secret_aad(channel.id, config.kind());
                let plain = self.secrets.open(sealed, &aad).map_err(|e| e.to_string())?;
                Some(String::from_utf8(plain).map_err(|_| "stored secret is corrupt")?)
            }
            None => None,
        };
        send(&self.http, &config, secret.as_deref(), notification)
            .await
            .map_err(|error| sanitize(&error, secret.as_deref()))
    }
}

/// Bounds an error for storage and masks the secret if a server echoed it.
fn sanitize(error: &str, secret: Option<&str>) -> String {
    let mut error = error.to_string();
    if let Some(secret) = secret.filter(|s| s.len() >= 4) {
        error = error.replace(secret, "********");
    }
    error.chars().take(MAX_ERROR_LEN).collect()
}

async fn send(
    http: &reqwest::Client,
    config: &ChannelConfig,
    secret: Option<&str>,
    n: &Notification,
) -> Result<(), String> {
    let text = format!("{}\n{}", n.title, n.body);
    let request = match config {
        ChannelConfig::Slack {} => http
            .post(secret.unwrap_or_default())
            .json(&json!({ "text": format!("*{}*\n{}", n.title, n.body) })),
        ChannelConfig::Discord {} => {
            let content: String = format!("**{}**\n{}", n.title, n.body)
                .chars()
                .take(2000)
                .collect();
            http.post(secret.unwrap_or_default())
                .json(&json!({ "content": content }))
        }
        ChannelConfig::Telegram { chat_id } => http
            .post(format!(
                "https://api.telegram.org/bot{}/sendMessage",
                secret.unwrap_or_default()
            ))
            .json(&json!({ "chat_id": chat_id, "text": text, "disable_web_page_preview": true })),
        ChannelConfig::Ntfy { server_url, topic } => {
            // JSON publishing to the server root keeps non-ASCII titles out
            // of HTTP headers.
            let request = http.post(server_url.trim_end_matches('/')).json(&json!({
                "topic": topic,
                "title": n.title,
                "message": n.body,
                "priority": if n.urgent { 4 } else { 3 },
            }));
            match secret {
                Some(token) => request.bearer_auth(token),
                None => request,
            }
        }
        ChannelConfig::Gotify { server_url } => http
            .post(format!("{}/message", server_url.trim_end_matches('/')))
            .header("x-gotify-key", secret.unwrap_or_default())
            .json(&json!({
                "title": n.title,
                "message": n.body,
                "priority": if n.urgent { 8 } else { 5 },
            })),
        ChannelConfig::Webhook { url } => {
            let mut payload = n.data.clone();
            payload["event"] = json!(n.event);
            payload["title"] = json!(n.title);
            payload["message"] = json!(n.body);
            let body = payload.to_string();
            let mut request = http
                .post(url)
                .header("content-type", "application/json")
                .header("x-scontrol-event", n.event);
            if let Some(secret) = secret {
                request = request.header("x-scontrol-signature", sign(secret, body.as_bytes()));
            }
            request.body(body)
        }
        ChannelConfig::Email { .. } => return send_email(config, secret, n).await,
    };
    let response = request
        .send()
        .await
        .map_err(|e| e.without_url().to_string())?;
    let status = response.status();
    if status.is_success() {
        return Ok(());
    }
    let body = response.text().await.unwrap_or_default();
    Err(format!("HTTP {status}: {}", body.trim()))
}

/// `sha256=<hex HMAC of the body>`, the same scheme GitHub uses.
pub fn sign(secret: &str, body: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("any key length");
    mac.update(body);
    format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
}

async fn send_email(
    config: &ChannelConfig,
    secret: Option<&str>,
    n: &Notification,
) -> Result<(), String> {
    use lettre::message::header::ContentType;
    use lettre::transport::smtp::authentication::Credentials;
    use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};

    let ChannelConfig::Email {
        host,
        port,
        security,
        username,
        from,
        to,
    } = config
    else {
        unreachable!("send_email is only called for email channels");
    };
    let mut message = Message::builder()
        .from(from.parse().map_err(|_| "invalid sender address")?)
        .subject(&n.title)
        .header(ContentType::TEXT_PLAIN);
    for address in to {
        message = message.to(address.parse().map_err(|_| "invalid recipient address")?);
    }
    let message = message
        .body(n.body.clone())
        .map_err(|e| format!("building email: {e}"))?;

    let builder = match security {
        SmtpSecurity::Tls => AsyncSmtpTransport::<Tokio1Executor>::relay(host),
        SmtpSecurity::Starttls => AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(host),
        SmtpSecurity::None => Ok(AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(
            host.as_str(),
        )),
    }
    .map_err(|e| format!("SMTP setup: {e}"))?;
    let mut builder = builder.port(*port).timeout(Some(SEND_TIMEOUT));
    if let Some(username) = username {
        builder = builder.credentials(Credentials::new(
            username.clone(),
            secret.unwrap_or_default().to_string(),
        ));
    }
    builder
        .build()
        .send(message)
        .await
        .map(|_| ())
        .map_err(|e| format!("SMTP: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn email(security: SmtpSecurity, host: &str, username: Option<&str>) -> ChannelConfig {
        ChannelConfig::Email {
            host: host.into(),
            port: 587,
            security,
            username: username.map(Into::into),
            from: "scontrol <panel@example.com>".into(),
            to: vec!["ops@example.com".into()],
        }
    }

    #[test]
    fn validates_channel_settings_and_secrets() {
        let ok = |c: ChannelConfig, s: Option<&str>| c.validate(s).unwrap();
        let bad = |c: ChannelConfig, s: Option<&str>| assert!(c.validate(s).is_err(), "{c:?}");

        ok(
            ChannelConfig::Slack {},
            Some("https://hooks.slack.com/services/T0/B0/x"),
        );
        bad(ChannelConfig::Slack {}, None);
        bad(
            ChannelConfig::Slack {},
            Some("https://evil.example/services/x"),
        );
        bad(
            ChannelConfig::Slack {},
            Some("http://hooks.slack.com/services/x"),
        );
        ok(
            ChannelConfig::Discord {},
            Some("https://discord.com/api/webhooks/1/abc"),
        );
        bad(ChannelConfig::Discord {}, Some("https://discord.com/other"));

        let tg = |chat: &str| ChannelConfig::Telegram {
            chat_id: chat.into(),
        };
        let token = "123456:ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefgh";
        ok(tg("-100123"), Some(token));
        ok(tg("@my_channel"), Some(token));
        bad(tg("x y"), Some(token));
        bad(tg("-100123"), Some("not-a-token"));

        let ntfy = |url: &str, topic: &str| ChannelConfig::Ntfy {
            server_url: url.into(),
            topic: topic.into(),
        };
        ok(ntfy("https://ntfy.sh", "deploys"), None);
        bad(ntfy("https://user:pw@ntfy.sh", "deploys"), None);
        bad(ntfy("ftp://ntfy.sh", "deploys"), None);
        bad(ntfy("https://ntfy.sh", "bad/topic"), None);
        bad(
            ChannelConfig::Gotify {
                server_url: "https://gotify.example".into(),
            },
            None,
        );
        ok(
            ChannelConfig::Webhook {
                url: "http://10.0.0.5:9000/hook".into(),
            },
            None,
        );
        bad(
            ChannelConfig::Webhook {
                url: "not a url".into(),
            },
            None,
        );

        ok(
            email(SmtpSecurity::Starttls, "smtp.example.com", None),
            None,
        );
        ok(email(SmtpSecurity::None, "127.0.0.1", None), None);
        bad(email(SmtpSecurity::None, "smtp.example.com", None), None);
        bad(
            email(SmtpSecurity::Tls, "smtp.example.com", Some("user")),
            None,
        );
        ok(
            email(SmtpSecurity::Tls, "smtp.example.com", Some("user")),
            Some("pw"),
        );
        bad(email(SmtpSecurity::Tls, "bad host", None), None);

        // Control characters are refused in any secret.
        bad(
            ChannelConfig::Webhook {
                url: "https://x.example".into(),
            },
            Some("a\nb"),
        );
        assert!(validate_events(&["deploy_failed".into()]).is_ok());
        assert!(validate_events(&[]).is_err());
        assert!(validate_events(&["nope".into()]).is_err());
    }

    #[test]
    fn channel_config_round_trips_with_a_kind_tag() {
        let config: ChannelConfig =
            serde_json::from_value(json!({ "kind": "telegram", "chat_id": "-1" })).unwrap();
        assert_eq!(config.kind(), "telegram");
        assert_eq!(serde_json::to_value(&config).unwrap()["kind"], "telegram");
        let slack: ChannelConfig = serde_json::from_value(json!({ "kind": "slack" })).unwrap();
        assert_eq!(slack, ChannelConfig::Slack {});
    }

    #[test]
    fn errors_are_bounded_and_never_carry_the_secret() {
        let secret = "https://hooks.slack.com/services/T/B/very-secret";
        let error = format!("HTTP 404: no such hook {secret} {}", "x".repeat(400));
        let clean = sanitize(&error, Some(secret));
        assert!(!clean.contains("very-secret"));
        assert!(clean.chars().count() <= MAX_ERROR_LEN);
    }

    #[test]
    fn webhook_signature_matches_github_scheme() {
        // RFC-style vector: HMAC-SHA256("key", "The quick brown fox ...").
        assert_eq!(
            sign("key", b"The quick brown fox jumps over the lazy dog"),
            "sha256=f7bc83f430538424b13298e6aa6fb143ef4d59a14946175997479dbc2d1a3cd8"
        );
    }
}
