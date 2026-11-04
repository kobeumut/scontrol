//! GitHub App installation tokens for private repositories.
//!
//! An operator registers a GitHub App (its numeric ID and private key) once
//! and links a project to one of the App's installations. When a deployment
//! starts, panel-api signs a ten-minute JWT with the App key, exchanges it
//! for an installation token scoped to the project's repository with
//! read-only `contents` access, and hands the token to the agent for that
//! deployment only (`DeployComposeProjectParams::git_credentials`). Git gets
//! it through the agent's `GIT_ASKPASS` helper, as for stored tokens.
//!
//! The private key is sealed with [`SecretBox`] (row-bound associated data)
//! and never returned; tokens are never stored, logged, or put in queue rows.

use crate::services::secret_box::SecretBox;
use base64::Engine;
use ring::rand::SystemRandom;
use ring::signature::{RsaKeyPair, RSA_PKCS1_SHA256};
use serde::Deserialize;
use std::time::Duration;
use uuid::Uuid;

/// Git host whose HTTPS remotes GitHub App tokens apply to.
pub const GITHUB_HOST: &str = "github.com";
const GITHUB_API: &str = "https://api.github.com";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
/// Username Git sends with an installation token.
pub const TOKEN_USERNAME: &str = "x-access-token";
/// Largest PEM accepted for an App private key.
pub const MAX_PRIVATE_KEY_LEN: usize = 16 * 1024;

#[derive(Clone)]
pub struct GitHubApps {
    secrets: SecretBox,
    http: reqwest::Client,
    api_base: String,
}

/// The sealed key of a registered App, as stored.
pub struct SealedApp {
    pub id: Uuid,
    pub app_id: i64,
    pub private_key_sealed: Vec<u8>,
}

impl GitHubApps {
    pub fn new(secrets: SecretBox) -> Self {
        Self::with_api_base(secrets, GITHUB_API)
    }

    /// Talks to `api_base` instead of api.github.com (tests).
    pub fn with_api_base(secrets: SecretBox, api_base: &str) -> Self {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            // The request carries a signed JWT; never follow it elsewhere.
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("scontrol/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("static HTTP client configuration");
        Self {
            secrets,
            http,
            api_base: api_base.trim_end_matches('/').to_string(),
        }
    }

    /// Seals a validated PEM key for the App row `id`.
    pub fn seal_private_key(&self, id: Uuid, pem: &str) -> Vec<u8> {
        self.secrets.seal(pem.as_bytes(), aad(id).as_bytes())
    }

    /// Mints an installation token limited to `repository` (`owner/name`)
    /// with read-only contents access. Errors never contain the token or JWT.
    pub async fn installation_token(
        &self,
        app: &SealedApp,
        installation_id: i64,
        repository: &str,
    ) -> anyhow::Result<String> {
        self.installation_token_with(
            app,
            installation_id,
            repository,
            serde_json::json!({ "contents": "read" }),
        )
        .await
    }

    /// An installation token scoped to `repository` with `permissions`
    /// (pull request previews ask for `pull_requests: write` to comment).
    pub async fn installation_token_with(
        &self,
        app: &SealedApp,
        installation_id: i64,
        repository: &str,
        permissions: serde_json::Value,
    ) -> anyhow::Result<String> {
        let pem = self
            .secrets
            .open(&app.private_key_sealed, aad(app.id).as_bytes())?;
        let pem = String::from_utf8(pem)
            .map_err(|_| anyhow::anyhow!("stored GitHub App key is not valid UTF-8"))?;
        let key = parse_private_key(&pem).map_err(anyhow::Error::msg)?;
        let jwt = app_jwt(app.app_id, &key, chrono::Utc::now().timestamp())?;
        let name = repository
            .split_once('/')
            .map(|(_, name)| name)
            .unwrap_or(repository);
        let response = self
            .http
            .post(format!(
                "{}/app/installations/{installation_id}/access_tokens",
                self.api_base
            ))
            .bearer_auth(jwt)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .json(&serde_json::json!({
                "repositories": [name],
                "permissions": permissions,
            }))
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("GitHub API request failed: {}", e.without_url()))?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            let message = serde_json::from_str::<serde_json::Value>(&body)
                .ok()
                .and_then(|v| v["message"].as_str().map(str::to_string))
                .unwrap_or_default();
            let message: String = message.chars().take(200).collect();
            anyhow::bail!(
                "GitHub refused the installation token request (HTTP {}): {message}",
                status.as_u16()
            );
        }
        #[derive(Deserialize)]
        struct TokenResponse {
            token: String,
        }
        let token = response
            .json::<TokenResponse>()
            .await
            .map_err(|_| anyhow::anyhow!("GitHub returned an unexpected token response"))?
            .token;
        if token.is_empty()
            || token.len() > 4096
            || token.chars().any(|c| c.is_control() || c.is_whitespace())
        {
            anyhow::bail!("GitHub returned a malformed installation token");
        }
        Ok(token)
    }
}

fn aad(id: Uuid) -> String {
    format!("github_app:{id}:private_key")
}

/// Parses the App private key GitHub hands out (PKCS#1, `BEGIN RSA PRIVATE
/// KEY`) or its PKCS#8 form (`BEGIN PRIVATE KEY`). Messages never echo key
/// material.
pub fn parse_private_key(pem: &str) -> Result<RsaKeyPair, String> {
    let pem = pem.trim();
    let (label, pkcs8) = if pem.starts_with("-----BEGIN RSA PRIVATE KEY-----") {
        ("RSA PRIVATE KEY", false)
    } else if pem.starts_with("-----BEGIN PRIVATE KEY-----") {
        ("PRIVATE KEY", true)
    } else {
        return Err("the private key must be the PEM file GitHub generated for the App".into());
    };
    let body: String = pem
        .strip_prefix(&format!("-----BEGIN {label}-----"))
        .and_then(|rest| rest.strip_suffix(&format!("-----END {label}-----")))
        .ok_or("the private key PEM is incomplete")?
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    let der = base64::engine::general_purpose::STANDARD
        .decode(body)
        .map_err(|_| "the private key PEM is not valid base64")?;
    let key = if pkcs8 {
        RsaKeyPair::from_pkcs8(&der)
    } else {
        RsaKeyPair::from_der(&der)
    };
    key.map_err(|_| "the private key is not a usable RSA key (2048 bits or more)".into())
}

/// The App JWT: RS256, issued a minute in the past for clock skew, valid
/// for nine minutes (GitHub accepts at most ten).
fn app_jwt(app_id: i64, key: &RsaKeyPair, now: i64) -> anyhow::Result<String> {
    let encode = |bytes: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    let header = encode(br#"{"alg":"RS256","typ":"JWT"}"#);
    let claims = encode(
        serde_json::json!({ "iat": now - 60, "exp": now + 9 * 60, "iss": app_id.to_string() })
            .to_string()
            .as_bytes(),
    );
    let message = format!("{header}.{claims}");
    let mut signature = vec![0u8; key.public().modulus_len()];
    key.sign(
        &RSA_PKCS1_SHA256,
        &SystemRandom::new(),
        message.as_bytes(),
        &mut signature,
    )
    .map_err(|_| anyhow::anyhow!("signing the GitHub App JWT failed"))?;
    Ok(format!("{message}.{}", encode(&signature)))
}

/// `owner/name` of an `https://github.com/<owner>/<name>[.git]` remote, the
/// only kind of remote GitHub App tokens apply to.
pub fn github_repository(url: &str) -> Option<String> {
    let (transport, host) = panel_core::types::validate::git_remote(url)?;
    if transport != panel_core::types::validate::GitTransport::Https || host != GITHUB_HOST {
        return None;
    }
    let path = url.split_once("://")?.1.split_once('/')?.1;
    let path = path.trim_end_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let (owner, name) = path.split_once('/')?;
    let valid = |part: &str| {
        !part.is_empty()
            && part.len() <= 100
            && part
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    (valid(owner) && valid(name)).then(|| format!("{owner}/{name}"))
}

#[cfg(test)]
pub(crate) const TEST_KEY_PKCS1: &str = include_str!("testdata/github-app-test-key.pem");
#[cfg(test)]
const TEST_KEY_PKCS8: &str = include_str!("testdata/github-app-test-key.pkcs8.pem");

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use ring::signature::{UnparsedPublicKey, RSA_PKCS1_2048_8192_SHA256};

    fn decode(part: &str) -> Vec<u8> {
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(part)
            .unwrap()
    }

    #[test]
    fn accepts_github_pem_keys_and_rejects_anything_else() {
        assert!(parse_private_key(TEST_KEY_PKCS1).is_ok());
        assert!(parse_private_key(TEST_KEY_PKCS8).is_ok());
        for bad in [
            "",
            "ghp_not_a_key",
            "-----BEGIN OPENSSH PRIVATE KEY-----\nAAAA\n-----END OPENSSH PRIVATE KEY-----",
            "-----BEGIN RSA PRIVATE KEY-----\n!!!\n-----END RSA PRIVATE KEY-----",
            "-----BEGIN RSA PRIVATE KEY-----\nAAAA",
        ] {
            let error = parse_private_key(bad).expect_err(bad);
            assert!(!error.contains("AAAA"), "{error}");
        }
    }

    #[test]
    fn jwt_is_rs256_signed_by_the_app_key_with_short_validity() {
        let key = parse_private_key(TEST_KEY_PKCS1).unwrap();
        let jwt = app_jwt(424242, &key, 1_800_000_000).unwrap();
        let parts: Vec<&str> = jwt.split('.').collect();
        assert_eq!(parts.len(), 3);
        let header: serde_json::Value = serde_json::from_slice(&decode(parts[0])).unwrap();
        assert_eq!(header["alg"], "RS256");
        let claims: serde_json::Value = serde_json::from_slice(&decode(parts[1])).unwrap();
        assert_eq!(claims["iss"], "424242");
        assert_eq!(claims["iat"], 1_800_000_000 - 60);
        let lifetime = claims["exp"].as_i64().unwrap() - claims["iat"].as_i64().unwrap();
        assert!(
            lifetime <= 600,
            "GitHub rejects JWTs valid for over 10 minutes"
        );
        let public = UnparsedPublicKey::new(&RSA_PKCS1_2048_8192_SHA256, key.public().as_ref());
        let message = format!("{}.{}", parts[0], parts[1]);
        public
            .verify(message.as_bytes(), &decode(parts[2]))
            .expect("signature verifies with the App public key");
        assert!(public.verify(b"tampered", &decode(parts[2])).is_err());
    }

    #[test]
    fn only_github_https_remotes_have_a_repository() {
        assert_eq!(
            github_repository("https://github.com/acme/shop.git").as_deref(),
            Some("acme/shop")
        );
        assert_eq!(
            github_repository("https://GitHub.com/acme/shop").as_deref(),
            Some("acme/shop")
        );
        for url in [
            "git@github.com:acme/shop.git",
            "https://gitlab.com/acme/shop.git",
            "https://github.com.evil.example/acme/shop.git",
            "https://github.com/acme",
            "https://github.com/acme/shop/extra",
        ] {
            assert_eq!(github_repository(url), None, "{url}");
        }
    }

    /// A stand-in for api.github.com that checks the JWT and request body.
    pub(crate) async fn mock_github(expected_installation: i64) -> String {
        use axum::extract::Path;
        use axum::http::{HeaderMap, StatusCode};
        use axum::routing::post;
        use axum::Json;

        let handler = move |Path(installation): Path<i64>,
                            headers: HeaderMap,
                            Json(body): Json<serde_json::Value>| async move {
            let key = parse_private_key(TEST_KEY_PKCS1).unwrap();
            let jwt = headers["authorization"]
                .to_str()
                .unwrap()
                .strip_prefix("Bearer ")
                .unwrap()
                .to_string();
            let parts: Vec<&str> = jwt.split('.').collect();
            let public = UnparsedPublicKey::new(&RSA_PKCS1_2048_8192_SHA256, key.public().as_ref());
            let signed = format!("{}.{}", parts[0], parts[1]);
            let valid = public.verify(signed.as_bytes(), &decode(parts[2])).is_ok();
            if !valid || installation != expected_installation {
                return (
                    StatusCode::NOT_FOUND,
                    Json(serde_json::json!({ "message": "Not Found" })),
                );
            }
            assert_eq!(body["permissions"]["contents"], "read");
            assert_eq!(body["repositories"].as_array().unwrap().len(), 1);
            (
                StatusCode::CREATED,
                Json(serde_json::json!({
                    "token": format!("ghs_minted_for_{}", body["repositories"][0].as_str().unwrap()),
                    "expires_at": "2099-01-01T00:00:00Z",
                })),
            )
        };
        let router =
            axum::Router::new().route("/app/installations/:id/access_tokens", post(handler));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.ok() });
        format!("http://{address}")
    }

    #[tokio::test]
    async fn mints_a_repository_scoped_token_with_the_sealed_key() {
        let base = mock_github(77).await;
        let apps = GitHubApps::with_api_base(SecretBox::derive(&[7u8; 32]), &base);
        let id = Uuid::new_v4();
        let sealed = apps.seal_private_key(id, TEST_KEY_PKCS1);
        assert!(!String::from_utf8_lossy(&sealed).contains("PRIVATE KEY"));
        let app = SealedApp {
            id,
            app_id: 1234,
            private_key_sealed: sealed.clone(),
        };
        let token = apps
            .installation_token(&app, 77, "acme/shop")
            .await
            .unwrap();
        assert_eq!(token, "ghs_minted_for_shop");

        let error = apps
            .installation_token(&app, 78, "acme/shop")
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("HTTP 404") && error.contains("Not Found"),
            "{error}"
        );

        // Sealed for another row: refused before any request.
        let moved = SealedApp {
            id: Uuid::new_v4(),
            app_id: 1234,
            private_key_sealed: sealed,
        };
        assert!(apps
            .installation_token(&moved, 77, "acme/shop")
            .await
            .is_err());
    }
}
