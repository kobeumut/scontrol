//! Session management. The session cookie carries an AES-GCM encrypted,
//! random opaque token; only a SHA-256 hash of the token is stored server
//! side, so a leaked database dump cannot be replayed as a valid cookie.

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use base64::Engine;
use chrono::Duration;
use rand::RngCore;
use sha2::{Digest, Sha256};

pub const SESSION_COOKIE_NAME: &str = "scontrol_session";

#[derive(Clone)]
pub struct CookieCipher {
    cipher: Aes256Gcm,
}

impl CookieCipher {
    pub fn new(key_bytes: &[u8; 32]) -> Self {
        let key = Key::<Aes256Gcm>::from_slice(key_bytes);
        Self {
            cipher: Aes256Gcm::new(key),
        }
    }

    /// Encrypts a raw session token into an opaque cookie value
    /// (`base64(nonce || ciphertext)`).
    pub fn encrypt(&self, token: &[u8]) -> anyhow::Result<String> {
        let mut nonce_bytes = [0u8; 12];
        rand::rngs::OsRng.fill_bytes(&mut nonce_bytes);
        let nonce = Nonce::from_slice(&nonce_bytes);
        let ciphertext = self
            .cipher
            .encrypt(nonce, token)
            .map_err(|e| anyhow::anyhow!("encrypting session cookie: {e}"))?;
        let mut combined = nonce_bytes.to_vec();
        combined.extend_from_slice(&ciphertext);
        Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(combined))
    }

    pub fn decrypt(&self, cookie_value: &str) -> anyhow::Result<Vec<u8>> {
        let combined = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(cookie_value)
            .map_err(|e| anyhow::anyhow!("decoding cookie: {e}"))?;
        if combined.len() < 12 {
            anyhow::bail!("cookie too short");
        }
        let (nonce_bytes, ciphertext) = combined.split_at(12);
        let nonce = Nonce::from_slice(nonce_bytes);
        self.cipher
            .decrypt(nonce, ciphertext)
            .map_err(|e| anyhow::anyhow!("decrypting session cookie: {e}"))
    }
}

pub fn generate_token() -> (Vec<u8>, String) {
    let mut token = vec![0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut token);
    let hash = hex::encode(Sha256::digest(&token));
    (token, hash)
}

pub fn session_ttl(hours: i64) -> Duration {
    Duration::hours(hours)
}

pub fn hash_token(token: &[u8]) -> String {
    hex::encode(Sha256::digest(token))
}
