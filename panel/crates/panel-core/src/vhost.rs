//! Typed Nginx vhost options for a project domain: redirects, HTTP Basic
//! Auth, a path prefix, and a custom TLS certificate.
//!
//! The options are plain data; the agent turns them into Nginx directives.
//! Nothing here reaches a config file as free text: the redirect target and
//! prefix are validated against strict allow-lists first. Secrets (password
//! hashes, the certificate's private key) are not part of the options; they
//! travel through [`VhostSecrets`] and live in 0600 files on the agent.

use crate::types::validate;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Redirect {
    /// 301 (permanent) or 302 (temporary).
    pub status: u16,
    /// Absolute `http(s)` URL without a query string, fragment, or `$`.
    pub target: String,
    /// Append the request path and query (`$request_uri`) to the target.
    #[serde(default)]
    pub preserve_path: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct VhostOptions {
    /// Answer every request with a redirect instead of serving the app.
    #[serde(default)]
    pub redirect: Option<Redirect>,
    /// Serve the app under `/prefix/` instead of `/` (the prefix is
    /// stripped before the request reaches the container).
    #[serde(default)]
    pub path_prefix: Option<String>,
    /// Require HTTP Basic Auth. Users live in the agent's htpasswd file.
    #[serde(default)]
    pub basic_auth: bool,
    /// Use the uploaded certificate instead of a Let's Encrypt one.
    #[serde(default)]
    pub custom_cert: bool,
}

impl VhostOptions {
    pub fn is_default(&self) -> bool {
        *self == VhostOptions::default()
    }

    pub fn validate(&self) -> Result<(), String> {
        if let Some(redirect) = &self.redirect {
            if redirect.status != 301 && redirect.status != 302 {
                return Err("redirect status must be 301 or 302".into());
            }
            if !is_valid_redirect_target(&redirect.target) {
                return Err("redirect target must be an http(s) URL".into());
            }
            if self.path_prefix.is_some() || self.basic_auth {
                return Err(
                    "a redirect cannot be combined with a path prefix or basic auth".into(),
                );
            }
        }
        if let Some(prefix) = &self.path_prefix {
            if !is_valid_path_prefix(prefix) {
                return Err("path prefix must look like /name/ (letters, digits, - _ .)".into());
            }
        }
        Ok(())
    }
}

/// `http(s)://host[:port][/path]` using only URL-safe characters. Quotes,
/// whitespace, `;`, `{}`, `$`, `\`, `?`, and `#` are refused, so the value
/// cannot end the Nginx directive or reference a variable.
pub fn is_valid_redirect_target(target: &str) -> bool {
    let Some(rest) = target
        .strip_prefix("https://")
        .or_else(|| target.strip_prefix("http://"))
    else {
        return false;
    };
    if target.len() > 2048 || rest.is_empty() {
        return false;
    }
    let (authority, _) = rest.split_once('/').unwrap_or((rest, ""));
    let host = authority.split_once(':').map_or(authority, |(h, _)| h);
    if !validate::is_valid_domain(host) && !is_ipv4(host) {
        return false;
    }
    if let Some((_, port)) = authority.split_once(':') {
        if port.parse::<u16>().map_or(true, |p| p == 0) {
            return false;
        }
    }
    rest.bytes().all(|b| {
        b.is_ascii_alphanumeric()
            || matches!(
                b,
                b'-' | b'.' | b'_' | b'~' | b'/' | b':' | b'%' | b'@' | b'+' | b'=' | b'&' | b','
            )
    })
}

fn is_ipv4(host: &str) -> bool {
    host.parse::<std::net::Ipv4Addr>().is_ok()
}

/// `/seg/` or `/a/b/`: each segment 1..=64 of `[A-Za-z0-9._-]`, never `.`
/// or `..`, and at most four segments.
pub fn is_valid_path_prefix(prefix: &str) -> bool {
    if prefix.len() > 200 || !prefix.starts_with('/') || !prefix.ends_with('/') || prefix == "/" {
        return false;
    }
    let segments: Vec<&str> = prefix[1..prefix.len() - 1].split('/').collect();
    segments.len() <= 4
        && segments.iter().all(|s| {
            !s.is_empty()
                && s.len() <= 64
                && *s != "."
                && *s != ".."
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        })
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct BasicAuthUser {
    pub username: String,
    /// Plain text on the wire to the agent only; the agent stores a hash.
    pub password: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CustomCertificate {
    pub certificate_pem: String,
    pub private_key_pem: String,
}

/// What to do with one stored secret.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "action", content = "value", rename_all = "snake_case")]
pub enum SecretChange<T> {
    Keep,
    Set(T),
    Remove,
}

// Derive would demand `T: Default`, which certificates do not implement.
#[allow(clippy::derivable_impls)]
impl<T> Default for SecretChange<T> {
    fn default() -> Self {
        SecretChange::Keep
    }
}

pub const MAX_BASIC_AUTH_USERS: usize = 20;

pub fn validate_basic_auth_users(users: &[BasicAuthUser]) -> Result<(), String> {
    if users.is_empty() || users.len() > MAX_BASIC_AUTH_USERS {
        return Err(format!("basic auth needs 1..={MAX_BASIC_AUTH_USERS} users"));
    }
    let mut seen = std::collections::HashSet::new();
    for user in users {
        let name_ok = !user.username.is_empty()
            && user.username.len() <= 64
            && user
                .username
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'@'));
        if !name_ok {
            return Err("basic auth usernames use letters, digits, . _ - @".into());
        }
        if !seen.insert(user.username.as_str()) {
            return Err(format!("duplicate basic auth user: {}", user.username));
        }
        if user.password.len() < 8
            || user.password.len() > 128
            || user.password.chars().any(|c| c.is_control())
        {
            return Err("basic auth passwords need 8..=128 printable characters".into());
        }
    }
    Ok(())
}

/// Structural PEM checks only; the agent finishes the job with `nginx -t`,
/// which verifies the key belongs to the certificate.
pub fn validate_certificate(cert: &CustomCertificate) -> Result<(), String> {
    let limit = 64 * 1024;
    if cert.certificate_pem.len() > limit || cert.private_key_pem.len() > limit {
        return Err("certificate or key is too large".into());
    }
    let cert_ok = cert.certificate_pem.contains("-----BEGIN CERTIFICATE-----")
        && cert.certificate_pem.contains("-----END CERTIFICATE-----");
    let key_ok = cert.private_key_pem.contains("PRIVATE KEY-----")
        && cert.private_key_pem.contains("-----BEGIN ")
        && !cert.private_key_pem.contains("ENCRYPTED");
    if !cert_ok {
        return Err("certificate must be PEM encoded".into());
    }
    if !key_ok {
        return Err("private key must be an unencrypted PEM key".into());
    }
    if cert.certificate_pem.contains("PRIVATE KEY") {
        return Err("the certificate field must not contain a private key".into());
    }
    for pem in [&cert.certificate_pem, &cert.private_key_pem] {
        if pem
            .chars()
            .any(|c| c.is_control() && c != '\n' && c != '\r')
        {
            return Err("PEM contains control characters".into());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redirect_targets_are_strict() {
        assert!(is_valid_redirect_target("https://example.com"));
        assert!(is_valid_redirect_target("http://www.example.com:8080/a/b"));
        for bad in [
            "ftp://example.com",
            "https://",
            "https://exa mple.com",
            "https://example.com;return 200",
            "https://example.com/$request_uri",
            "https://example.com/\"",
            "https://example.com/a{b}",
            "https://example.com/?q=1",
            "https://example.com:0",
            "https://example.com\nfoo",
            "https://-bad.com",
        ] {
            assert!(!is_valid_redirect_target(bad), "{bad}");
        }
    }

    #[test]
    fn prefixes_are_strict() {
        assert!(is_valid_path_prefix("/app/"));
        assert!(is_valid_path_prefix("/a-b/c_d/"));
        for bad in [
            "/",
            "app/",
            "/app",
            "/../",
            "/./",
            "//",
            "/a b/",
            "/a;b/",
            "/a/b/c/d/e/",
            "/~x/",
        ] {
            assert!(!is_valid_path_prefix(bad), "{bad}");
        }
    }

    #[test]
    fn options_validate_combinations() {
        let ok = VhostOptions {
            redirect: Some(Redirect {
                status: 301,
                target: "https://a.example.com".into(),
                preserve_path: true,
            }),
            ..Default::default()
        };
        assert!(ok.validate().is_ok());
        let mut bad = ok.clone();
        bad.redirect.as_mut().unwrap().status = 307;
        assert!(bad.validate().is_err());
        let mut mixed = ok;
        mixed.basic_auth = true;
        assert!(mixed.validate().is_err());
        assert!(VhostOptions {
            path_prefix: Some("/x/".into()),
            basic_auth: true,
            ..Default::default()
        }
        .validate()
        .is_ok());
        assert!(VhostOptions::default().is_default());
    }

    #[test]
    fn basic_auth_and_certificate_validation() {
        let user = |u: &str, p: &str| BasicAuthUser {
            username: u.into(),
            password: p.into(),
        };
        assert!(validate_basic_auth_users(&[user("alice", "longenough")]).is_ok());
        assert!(validate_basic_auth_users(&[]).is_err());
        assert!(validate_basic_auth_users(&[user("al:ice", "longenough")]).is_err());
        assert!(validate_basic_auth_users(&[user("alice", "short")]).is_err());
        assert!(
            validate_basic_auth_users(&[user("a", "longenough"), user("a", "longenough")]).is_err()
        );
        let cert = CustomCertificate {
            certificate_pem: "-----BEGIN CERTIFICATE-----\nAAA\n-----END CERTIFICATE-----\n".into(),
            private_key_pem: "-----BEGIN PRIVATE KEY-----\nBBB\n-----END PRIVATE KEY-----\n".into(),
        };
        assert!(validate_certificate(&cert).is_ok());
        let swapped = CustomCertificate {
            certificate_pem: cert.private_key_pem.clone(),
            private_key_pem: cert.certificate_pem.clone(),
        };
        assert!(validate_certificate(&swapped).is_err());
        let encrypted = CustomCertificate {
            private_key_pem:
                "-----BEGIN ENCRYPTED PRIVATE KEY-----\n-----END ENCRYPTED PRIVATE KEY-----".into(),
            ..cert
        };
        assert!(validate_certificate(&encrypted).is_err());
    }
}
