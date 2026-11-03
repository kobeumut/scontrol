//! Encryption at rest for secrets panel-api must store and use again later
//! (notification credentials now; registry, S3, and Git credentials in D2).
//!
//! AES-256-GCM with a random 96-bit nonce per seal. The caller passes
//! associated data that names the owning row, so a sealed value copied to
//! another row fails to open.

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use hmac::{Hmac, Mac};
use rand::RngCore;
use sha2::Sha256;

const NONCE_LEN: usize = 12;

#[derive(Clone)]
pub struct SecretBox {
    cipher: Aes256Gcm,
}

impl SecretBox {
    /// Derives the storage key from the panel's secret key with a distinct
    /// label, so the raw key is never used directly.
    pub fn derive(master: &[u8; 32]) -> Self {
        let mut mac =
            <Hmac<Sha256> as Mac>::new_from_slice(master).expect("HMAC accepts any key length");
        mac.update(b"scontrol/secret-box/v1");
        let key: [u8; 32] = mac.finalize().into_bytes().into();
        Self {
            cipher: Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key)),
        }
    }

    /// Returns `nonce || ciphertext`.
    pub fn seal(&self, plaintext: &[u8], aad: &[u8]) -> Vec<u8> {
        let mut nonce = [0u8; NONCE_LEN];
        rand::rngs::OsRng.fill_bytes(&mut nonce);
        let ciphertext = self
            .cipher
            .encrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: plaintext,
                    aad,
                },
            )
            .expect("AES-GCM encryption of an in-memory buffer cannot fail");
        let mut sealed = nonce.to_vec();
        sealed.extend_from_slice(&ciphertext);
        sealed
    }

    pub fn open(&self, sealed: &[u8], aad: &[u8]) -> anyhow::Result<Vec<u8>> {
        if sealed.len() < NONCE_LEN {
            anyhow::bail!("sealed secret is too short");
        }
        let (nonce, ciphertext) = sealed.split_at(NONCE_LEN);
        self.cipher
            .decrypt(
                Nonce::from_slice(nonce),
                Payload {
                    msg: ciphertext,
                    aad,
                },
            )
            .map_err(|_| {
                anyhow::anyhow!(
                    "stored secret cannot be decrypted (was PANEL_SECRET_KEY changed?); \
                     enter it again"
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seals_and_opens_only_with_the_same_key_and_aad() {
        let sb = SecretBox::derive(&[1u8; 32]);
        let sealed = sb.seal(b"hunter2", b"row-a");
        assert!(!sealed.windows(7).any(|w| w == b"hunter2"));
        assert_eq!(sb.open(&sealed, b"row-a").unwrap(), b"hunter2");
        assert!(sb.open(&sealed, b"row-b").is_err());
        assert!(SecretBox::derive(&[2u8; 32])
            .open(&sealed, b"row-a")
            .is_err());
        assert!(sb.open(&sealed[..5], b"row-a").is_err());
        // Fresh nonce per seal.
        assert_ne!(sb.seal(b"x", b"a"), sb.seal(b"x", b"a"));
    }
}
