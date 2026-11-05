//! API key tokens: `sck_<prefix>_<secret>`.
//!
//! - `prefix` is 12 random hex characters, stored in clear to find the row.
//! - `secret` is 32 random bytes (base64url). Only its Argon2id hash is
//!   stored, so the token is shown exactly once, when it is created, and a
//!   database dump cannot be replayed.
//! - Argon2 is deliberately slow, so a verified token is remembered (by its
//!   SHA-256, in memory) for a minute. The key row is still read on every
//!   request, so revocation and expiry apply immediately.

use base64::Engine;
use rand::RngCore;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use uuid::Uuid;

pub const TOKEN_PREFIX: &str = "sck_";
const PREFIX_LEN: usize = 12;
const SECRET_LEN: usize = 43;
const VERIFIED_TTL: Duration = Duration::from_secs(60);
const CACHE_LIMIT: usize = 1024;

/// A freshly generated token and what is stored for it.
pub struct NewToken {
    pub token: String,
    pub prefix: String,
    pub secret_hash: String,
}

pub fn generate() -> anyhow::Result<NewToken> {
    let mut prefix = [0u8; PREFIX_LEN / 2];
    rand::rngs::OsRng.fill_bytes(&mut prefix);
    let prefix = hex::encode(prefix);
    let mut secret = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut secret);
    let secret = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(secret);
    let secret_hash = panel_core::security::hash_password(&secret).map_err(anyhow::Error::msg)?;
    Ok(NewToken {
        token: format!("{TOKEN_PREFIX}{prefix}_{secret}"),
        prefix,
        secret_hash,
    })
}

/// Splits a presented token into `(prefix, secret)`; `None` if it is not
/// shaped like one of ours.
pub fn parse(token: &str) -> Option<(&str, &str)> {
    let rest = token.strip_prefix(TOKEN_PREFIX)?;
    let (prefix, secret) = rest.split_once('_')?;
    let prefix_ok = prefix.len() == PREFIX_LEN
        && prefix
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    let secret_ok = secret.len() == SECRET_LEN
        && secret
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    (prefix_ok && secret_ok).then_some((prefix, secret))
}

/// Constant-time comparison of two digests.
fn digests_equal(a: &[u8; 32], b: &[u8; 32]) -> bool {
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// SHA-256 of a verified secret and when it was verified.
type Verified = ([u8; 32], Instant);

/// Recently verified `(key id, SHA-256 of the secret)` pairs.
#[derive(Clone, Default)]
pub struct VerifiedKeys {
    inner: Arc<Mutex<HashMap<Uuid, Verified>>>,
}

impl VerifiedKeys {
    /// Checks `secret` against `secret_hash`, skipping Argon2 when the same
    /// secret verified for this key within the last minute.
    pub async fn verify(&self, key_id: Uuid, secret: &str, secret_hash: &str) -> bool {
        let digest: [u8; 32] = Sha256::digest(secret.as_bytes()).into();
        {
            let cache = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            if let Some((cached, at)) = cache.get(&key_id) {
                if at.elapsed() < VERIFIED_TTL && digests_equal(cached, &digest) {
                    return true;
                }
            }
        }
        let (secret, hash) = (secret.to_string(), secret_hash.to_string());
        let verified = tokio::task::spawn_blocking(move || {
            panel_core::security::verify_password(&secret, &hash)
        })
        .await
        .unwrap_or(false);
        if verified {
            let mut cache = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            if cache.len() >= CACHE_LIMIT {
                cache.retain(|_, (_, at)| at.elapsed() < VERIFIED_TTL);
                if cache.len() >= CACHE_LIMIT {
                    cache.clear();
                }
            }
            cache.insert(key_id, (digest, Instant::now()));
        }
        verified
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_parse_and_hashes_never_contain_the_secret() {
        let new = generate().unwrap();
        let (prefix, secret) = parse(&new.token).unwrap();
        assert_eq!(prefix, new.prefix);
        assert!(new.secret_hash.starts_with("$argon2id$"));
        assert!(!new.secret_hash.contains(secret));
        assert!(panel_core::security::verify_password(
            secret,
            &new.secret_hash
        ));
        assert!(!panel_core::security::verify_password(
            "wrong",
            &new.secret_hash
        ));
    }

    #[test]
    fn malformed_tokens_are_rejected() {
        let new = generate().unwrap();
        assert!(parse(&new.token.replace("sck_", "sk_")).is_none());
        assert!(parse(&new.token[..new.token.len() - 1]).is_none());
        assert!(parse("sck_ABCDEF012345_x").is_none());
        assert!(parse("").is_none());
        let (prefix, secret) = parse(&new.token).unwrap();
        assert!(parse(&format!("sck_{prefix}_{secret}!")).is_none());
    }

    #[tokio::test]
    async fn verification_is_cached_per_secret() {
        let new = generate().unwrap();
        let (_, secret) = parse(&new.token).unwrap();
        let cache = VerifiedKeys::default();
        let id = Uuid::new_v4();
        assert!(cache.verify(id, secret, &new.secret_hash).await);
        // A cached key still refuses a different secret.
        assert!(
            !cache
                .verify(id, "x".repeat(43).as_str(), &new.secret_hash)
                .await
        );
        assert!(
            cache.verify(id, secret, "not-a-hash").await,
            "served from the cache"
        );
        assert!(
            !VerifiedKeys::default()
                .verify(id, secret, "not-a-hash")
                .await
        );
    }
}
