//! Sealing and unsealing private registry credentials.

use crate::db::registries::RegistryRow;
use crate::services::secret_box::SecretBox;
use panel_core::adapters::DockerRegistryAuth;
use uuid::Uuid;

/// Associated data binding a sealed credential to its registry row.
pub fn aad(registry_id: Uuid) -> Vec<u8> {
    let mut aad = registry_id.as_bytes().to_vec();
    aad.extend_from_slice(b":docker-registry");
    aad
}

pub fn seal(secrets: &SecretBox, registry_id: Uuid, password: &str) -> Vec<u8> {
    secrets.seal(password.as_bytes(), &aad(registry_id))
}

/// The login to hand to the agent. The plaintext password exists only in
/// the returned value, which has a redacting `Debug`.
pub fn open(secrets: &SecretBox, row: &RegistryRow) -> anyhow::Result<DockerRegistryAuth> {
    let plain = secrets.open(&row.credential_ciphertext, &aad(row.id))?;
    Ok(DockerRegistryAuth {
        server: row.server_url.clone(),
        username: row.username.clone(),
        password: String::from_utf8(plain)
            .map_err(|_| anyhow::anyhow!("stored registry credential is corrupt"))?,
    })
}
