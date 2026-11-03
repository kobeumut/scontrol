//! Git push webhooks: secret derivation, request authentication, and push
//! payload interpretation for GitHub, Gitea/Forgejo, and GitLab.
//!
//! Dokploy authenticates its deploy webhooks with a token in the URL only.
//! Here every request must carry an HMAC-SHA256 body signature
//! (`X-Hub-Signature-256` / `X-Gitea-Signature`) or, for GitLab, which does
//! not sign bodies, the secret in `X-Gitlab-Token`.

use axum::http::HeaderMap;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use uuid::Uuid;

type HmacSha256 = Hmac<Sha256>;

/// Key from which every webhook secret is derived. It is itself derived from
/// the panel's cookie key with a distinct label, so the two uses never share
/// key material directly.
#[derive(Clone)]
pub struct WebhookKey([u8; 32]);

impl WebhookKey {
    pub fn derive(master: &[u8; 32]) -> Self {
        let mut mac = HmacSha256::new_from_slice(master).expect("HMAC accepts any key length");
        mac.update(b"scontrol/webhook-key/v1");
        Self(mac.finalize().into_bytes().into())
    }

    /// The secret operators paste into their Git host. Deterministic for a
    /// (project, nonce) pair; a new nonce yields an unrelated secret.
    pub fn secret_for(&self, project_id: Uuid, nonce: &str) -> String {
        let mut mac = HmacSha256::new_from_slice(&self.0).expect("HMAC accepts any key length");
        mac.update(project_id.as_bytes());
        mac.update(b":");
        mac.update(nonce.as_bytes());
        hex::encode(mac.finalize().into_bytes())
    }
}

/// A new random nonce (128 bits, hex).
pub fn new_nonce() -> String {
    let mut bytes = [0u8; 16];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut bytes);
    hex::encode(bytes)
}

/// Checks the request against `secret`. Signature comparison is constant
/// time (`Mac::verify_slice`, [`constant_time_eq`]).
pub fn authenticate(headers: &HeaderMap, body: &[u8], secret: &str) -> bool {
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    if let Some(signature) = header("x-hub-signature-256") {
        return signature
            .strip_prefix("sha256=")
            .is_some_and(|hex_sig| verify_hmac(secret, body, hex_sig));
    }
    if let Some(signature) = header("x-gitea-signature") {
        return verify_hmac(secret, body, signature);
    }
    if let Some(token) = header("x-gitlab-token") {
        return constant_time_eq(token.as_bytes(), secret.as_bytes());
    }
    false
}

fn verify_hmac(secret: &str, body: &[u8], hex_signature: &str) -> bool {
    let Ok(expected) = hex::decode(hex_signature.trim()) else {
        return false;
    };
    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key length");
    mac.update(body);
    mac.verify_slice(&expected).is_ok()
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// What a webhook delivery asks for.
#[derive(Debug, PartialEq, Eq)]
pub enum Delivery {
    /// Connectivity check (GitHub `ping`); nothing to do.
    Ping,
    /// A push to `branch` that leaves it at `commit`. `changed_paths` is
    /// `None` when the payload does not list every changed file (GitLab
    /// truncates to 20 commits; force pushes may list none), in which case a
    /// watch-path filter cannot rule the push out.
    Push {
        branch: String,
        commit: String,
        changed_paths: Option<Vec<String>>,
    },
    /// A delivery that never deploys (tags, deletions, other events).
    Ignored(&'static str),
}

/// Interprets an authenticated delivery.
pub fn interpret(headers: &HeaderMap, body: &[u8]) -> Result<Delivery, &'static str> {
    let event = [
        "x-github-event",
        "x-gitea-event",
        "x-gogs-event",
        "x-gitlab-event",
    ]
    .iter()
    .find_map(|name| headers.get(*name).and_then(|v| v.to_str().ok()))
    .unwrap_or("push")
    .to_ascii_lowercase();
    match event.as_str() {
        "ping" => return Ok(Delivery::Ping),
        "push" | "push hook" => {}
        _ => return Ok(Delivery::Ignored("event is not a push")),
    }

    #[derive(serde::Deserialize)]
    struct PushPayload {
        #[serde(rename = "ref")]
        git_ref: String,
        #[serde(default)]
        after: String,
        #[serde(default)]
        deleted: bool,
        commits: Option<Vec<PushCommit>>,
        /// GitLab only: the real commit count when `commits` is truncated.
        total_commits_count: Option<usize>,
    }
    #[derive(serde::Deserialize)]
    struct PushCommit {
        added: Option<Vec<String>>,
        removed: Option<Vec<String>>,
        modified: Option<Vec<String>>,
    }
    let payload: PushPayload =
        serde_json::from_slice(body).map_err(|_| "push payload is not valid JSON")?;
    let Some(branch) = payload.git_ref.strip_prefix("refs/heads/") else {
        return Ok(Delivery::Ignored("push is not to a branch"));
    };
    if payload.deleted || payload.after.chars().all(|c| c == '0') {
        return Ok(Delivery::Ignored("branch was deleted"));
    }
    let changed_paths = payload.commits.and_then(|commits| {
        let complete = !commits.is_empty()
            && payload
                .total_commits_count
                .is_none_or(|total| total <= commits.len());
        if !complete {
            return None;
        }
        let mut paths = Vec::new();
        for commit in commits {
            if commit.added.is_none() && commit.removed.is_none() && commit.modified.is_none() {
                return None;
            }
            paths.extend(
                [commit.added, commit.removed, commit.modified]
                    .into_iter()
                    .flatten()
                    .flatten(),
            );
        }
        Some(paths)
    });
    Ok(Delivery::Push {
        branch: branch.to_string(),
        commit: payload.after,
        changed_paths,
    })
}

/// Upper bound on watch patterns per project.
pub const MAX_WATCH_PATHS: usize = 32;

/// A watch pattern is a repository-relative glob: `*` and `?` match within
/// one path segment and a `**` segment matches any number of segments.
/// Character classes, braces, and negation are refused rather than silently
/// taken literally.
pub fn is_valid_watch_path(pattern: &str) -> bool {
    let trimmed = pattern.strip_suffix('/').unwrap_or(pattern);
    !trimmed.is_empty()
        && pattern.len() <= 200
        && !pattern.starts_with('/')
        && !pattern
            .chars()
            .any(|c| c.is_control() || matches!(c, '\\' | '[' | ']' | '{' | '}' | '!'))
        && trimmed
            .split('/')
            .all(|segment| !segment.is_empty() && segment != "." && segment != "..")
}

/// Whether any changed path matches any pattern. A pattern matches a file
/// itself or any directory above it, so `docker` and `docker/` both cover
/// `docker/app/Dockerfile`. Patterns are anchored at the repository root.
pub fn matches_watch_paths(patterns: &[String], changed: &[String]) -> bool {
    changed.iter().any(|path| {
        let path: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        patterns.iter().any(|pattern| {
            let pattern: Vec<&str> = pattern
                .strip_suffix('/')
                .unwrap_or(pattern)
                .split('/')
                .collect();
            match_segments(&pattern, &path)
        })
    })
}

/// Matches pattern segments against a leading run of path segments. Once
/// the pattern is used up, any remaining path segments lie below a matched
/// directory.
fn match_segments(pattern: &[&str], path: &[&str]) -> bool {
    match pattern.split_first() {
        None => true,
        Some((&"**", rest)) => (0..=path.len()).any(|skip| match_segments(rest, &path[skip..])),
        Some((segment, rest)) => path
            .split_first()
            .is_some_and(|(head, tail)| match_segment(segment, head) && match_segments(rest, tail)),
    }
}

/// `*` and `?` wildcards within one segment (greedy with backtracking to the
/// last `*`, so linear in practice).
fn match_segment(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0, 0);
    let mut last_star: Option<(usize, usize)> = None;
    while ti < t.len() {
        match p.get(pi) {
            Some('*') => {
                last_star = Some((pi, ti));
                pi += 1;
            }
            Some(&c) if c == '?' || c == t[ti] => {
                pi += 1;
                ti += 1;
            }
            _ => match last_star {
                Some((star, consumed)) => {
                    pi = star + 1;
                    ti = consumed + 1;
                    last_star = Some((star, consumed + 1));
                }
                None => return false,
            },
        }
    }
    p[pi..].iter().all(|&c| c == '*')
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn sign(secret: &str, body: &[u8]) -> String {
        let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(body);
        hex::encode(mac.finalize().into_bytes())
    }

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(*name, HeaderValue::from_str(value).unwrap());
        }
        map
    }

    #[test]
    fn secrets_are_stable_per_nonce_and_change_on_rotation() {
        let key = WebhookKey::derive(&[7u8; 32]);
        let project = Uuid::new_v4();
        assert_eq!(key.secret_for(project, "a"), key.secret_for(project, "a"));
        assert_ne!(key.secret_for(project, "a"), key.secret_for(project, "b"));
        assert_ne!(
            key.secret_for(project, "a"),
            WebhookKey::derive(&[8u8; 32]).secret_for(project, "a")
        );
        assert_eq!(key.secret_for(project, "a").len(), 64);
    }

    #[test]
    fn authenticates_github_gitea_and_gitlab_styles() {
        let body = br#"{"ref":"refs/heads/main"}"#;
        let secret = "s3cret";
        let sig = sign(secret, body);
        assert!(authenticate(
            &headers(&[("x-hub-signature-256", &format!("sha256={sig}"))]),
            body,
            secret
        ));
        assert!(authenticate(
            &headers(&[("x-gitea-signature", &sig)]),
            body,
            secret
        ));
        assert!(authenticate(
            &headers(&[("x-gitlab-token", secret)]),
            body,
            secret
        ));

        // Wrong secret, tampered body, missing prefix, no credentials.
        assert!(!authenticate(
            &headers(&[("x-hub-signature-256", &format!("sha256={sig}"))]),
            body,
            "other"
        ));
        assert!(!authenticate(
            &headers(&[("x-hub-signature-256", &format!("sha256={sig}"))]),
            br#"{"ref":"refs/heads/evil"}"#,
            secret
        ));
        assert!(!authenticate(
            &headers(&[("x-hub-signature-256", &sig)]),
            body,
            secret
        ));
        assert!(!authenticate(
            &headers(&[("x-gitlab-token", "nope")]),
            body,
            secret
        ));
        assert!(!authenticate(&HeaderMap::new(), body, secret));
    }

    #[test]
    fn interprets_push_ping_tags_and_deletions() {
        let push = br#"{"ref":"refs/heads/main","after":"0123abcd"}"#;
        assert_eq!(
            interpret(&headers(&[("x-github-event", "push")]), push).unwrap(),
            Delivery::Push {
                branch: "main".into(),
                commit: "0123abcd".into(),
                changed_paths: None,
            }
        );
        assert_eq!(
            interpret(&headers(&[("x-gitlab-event", "Push Hook")]), push).unwrap(),
            Delivery::Push {
                branch: "main".into(),
                commit: "0123abcd".into(),
                changed_paths: None,
            }
        );
        assert_eq!(
            interpret(&headers(&[("x-github-event", "ping")]), b"{}").unwrap(),
            Delivery::Ping
        );
        assert!(matches!(
            interpret(&headers(&[("x-github-event", "issues")]), push).unwrap(),
            Delivery::Ignored(_)
        ));
        assert!(matches!(
            interpret(
                &headers(&[("x-github-event", "push")]),
                br#"{"ref":"refs/tags/v1","after":"ab"}"#
            )
            .unwrap(),
            Delivery::Ignored(_)
        ));
        assert!(matches!(
            interpret(
                &headers(&[("x-github-event", "push")]),
                br#"{"ref":"refs/heads/main","after":"0000000000","deleted":true}"#
            )
            .unwrap(),
            Delivery::Ignored(_)
        ));
        assert!(interpret(&headers(&[("x-github-event", "push")]), b"not json").is_err());
    }

    fn changed(body: &[u8]) -> Option<Vec<String>> {
        match interpret(&headers(&[("x-github-event", "push")]), body).unwrap() {
            Delivery::Push { changed_paths, .. } => changed_paths,
            other => panic!("expected a push, got {other:?}"),
        }
    }

    #[test]
    fn collects_changed_paths_only_when_the_payload_is_complete() {
        assert_eq!(
            changed(
                br#"{"ref":"refs/heads/main","after":"ab","commits":[
                    {"added":["a.txt"],"removed":[],"modified":["src/main.rs"]},
                    {"added":[],"removed":["old/x"],"modified":[]}]}"#
            ),
            Some(vec!["a.txt".into(), "src/main.rs".into(), "old/x".into()])
        );
        // A force push listing no commits, GitLab truncation, and commits
        // without file lists are all unknown.
        assert_eq!(
            changed(br#"{"ref":"refs/heads/main","after":"ab","commits":[]}"#),
            None
        );
        assert_eq!(
            changed(
                br#"{"ref":"refs/heads/main","after":"ab","total_commits_count":21,
                    "commits":[{"added":[],"removed":[],"modified":["a"]}]}"#
            ),
            None
        );
        assert_eq!(
            changed(br#"{"ref":"refs/heads/main","after":"ab","commits":[{"id":"ab"}]}"#),
            None
        );
    }

    #[test]
    fn validates_watch_patterns() {
        for ok in [
            "src",
            "src/",
            "docker/**",
            "**/*.rs",
            "apps/*/Dockerfile",
            "a?c.yml",
        ] {
            assert!(is_valid_watch_path(ok), "{ok}");
        }
        for bad in [
            "", "/", "/etc", "../x", "a/../b", "a//b", "./a", "src/[ab]", "{a,b}", "!a", "a\\b",
            "a\nb",
        ] {
            assert!(!is_valid_watch_path(bad), "{bad:?}");
        }
        assert!(!is_valid_watch_path(&"a".repeat(201)));
    }

    #[test]
    fn matches_watch_patterns() {
        let m = |pattern: &str, path: &str| {
            matches_watch_paths(&[pattern.to_string()], &[path.to_string()])
        };
        assert!(m("docker", "docker/app/Dockerfile"));
        assert!(m("docker/", "docker/app/Dockerfile"));
        assert!(m("compose.yml", "compose.yml"));
        assert!(!m("compose.yml", "other/compose.yml"));
        assert!(m("*.yml", "compose.yml"));
        assert!(!m("*.yml", "deploy/compose.yml"));
        assert!(m("**/*.yml", "deploy/compose.yml"));
        assert!(m("**/*.yml", "compose.yml"));
        assert!(m("apps/*/src/**", "apps/web/src/a/b.ts"));
        assert!(!m("apps/*/src/**", "apps/web/test/a.ts"));
        assert!(m("a?c", "abc"));
        assert!(!m("a?c", "ac"));
        assert!(m("*a*b", "xxaxxb"));
        assert!(!m("*a*b", "xxaxxbx"));
        assert!(!m("src", "srcx/main.rs"));
        assert!(!matches_watch_paths(&["src".into()], &[]));
        assert!(matches_watch_paths(
            &["docs".into(), "src".into()],
            &["README.md".into(), "src/lib.rs".into()]
        ));
    }
}
