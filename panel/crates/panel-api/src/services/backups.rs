//! Sealing and unsealing S3 destination credentials.

use crate::db::backups::DestinationRow;
use crate::services::secret_box::SecretBox;
use panel_core::adapters::S3Target;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Serialize, Deserialize)]
struct Credentials {
    access_key: String,
    secret_key: String,
}

/// Associated data binding sealed keys to their destination row.
pub fn aad(destination_id: Uuid) -> Vec<u8> {
    let mut aad = destination_id.as_bytes().to_vec();
    aad.extend_from_slice(b":backup-destination");
    aad
}

pub fn seal(secrets: &SecretBox, id: Uuid, access_key: &str, secret_key: &str) -> Vec<u8> {
    let json = serde_json::to_vec(&Credentials {
        access_key: access_key.to_string(),
        secret_key: secret_key.to_string(),
    })
    .expect("credentials serialize");
    secrets.seal(&json, &aad(id))
}

/// The target to hand to the agent; `S3Target` has a redacting `Debug`.
pub fn open(secrets: &SecretBox, row: &DestinationRow) -> anyhow::Result<S3Target> {
    let plain = secrets.open(&row.credential_ciphertext, &aad(row.id))?;
    let creds: Credentials = serde_json::from_slice(&plain)
        .map_err(|_| anyhow::anyhow!("stored S3 credentials are corrupt"))?;
    Ok(S3Target {
        endpoint: row.endpoint.clone(),
        region: row.region.clone(),
        bucket: row.bucket.clone(),
        access_key: creds.access_key,
        secret_key: creds.secret_key,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_round_trip_and_stay_bound_to_their_row() {
        let sb = SecretBox::derive(&[9u8; 32]);
        let id = Uuid::new_v4();
        let sealed = seal(&sb, id, "AKIAEXAMPLE", "s3-secret-value");
        assert!(!sealed.windows(14).any(|w| w == b"s3-secret-valu"));
        let row = |id: Uuid| DestinationRow {
            id,
            name: "n".into(),
            endpoint: "https://s3.example.com".into(),
            region: "auto".into(),
            bucket: "b".into(),
            prefix: "p".into(),
            retention_count: 7,
            credential_ciphertext: sealed.clone(),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        let target = open(&sb, &row(id)).unwrap();
        assert_eq!(target.secret_key, "s3-secret-value");
        assert!(!format!("{target:?}").contains("s3-secret-value"));
        assert!(open(&sb, &row(Uuid::new_v4())).is_err());
    }
}
