//! Credentials of managed database services.
//!
//! Generated here with the OS RNG, sealed with `SecretBox` (associated data
//! names the owning service, so a ciphertext cannot be moved to another
//! row), and opened only to send with a deployment of that service or for
//! the audited credentials endpoint.

use crate::db;
use crate::db::database_services::DatabaseServiceRow;
use crate::services::secret_box::SecretBox;
use panel_core::database_services::GENERATED_PASSWORD_LEN;
use panel_core::types::{DatabaseCredentials, DatabaseDeployment, ManagedDatabaseEngine};
use rand::Rng;
use sqlx::PgPool;
use uuid::Uuid;

const CHARSET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";

fn random_secret() -> String {
    let mut rng = rand::rngs::OsRng;
    (0..GENERATED_PASSWORD_LEN)
        .map(|_| CHARSET[rng.gen_range(0..CHARSET.len())] as char)
        .collect()
}

pub fn generate(engine: ManagedDatabaseEngine) -> DatabaseCredentials {
    DatabaseCredentials {
        password: random_secret(),
        root_password: engine.has_root_password().then(random_secret),
    }
}

fn aad(project_id: Uuid) -> Vec<u8> {
    format!("database_service:{project_id}").into_bytes()
}

pub fn seal(secrets: &SecretBox, project_id: Uuid, credentials: &DatabaseCredentials) -> Vec<u8> {
    let plaintext = serde_json::to_vec(credentials).expect("credentials serialize");
    secrets.seal(&plaintext, &aad(project_id))
}

pub fn open(secrets: &SecretBox, row: &DatabaseServiceRow) -> anyhow::Result<DatabaseCredentials> {
    let plaintext = secrets.open(&row.credentials_ciphertext, &aad(row.project_id))?;
    serde_json::from_slice(&plaintext)
        .map_err(|_| anyhow::anyhow!("stored database credentials are malformed"))
}

/// Loopback port and credentials to send with a deployment of the service.
pub async fn deployment_for(
    pool: &PgPool,
    secrets: &SecretBox,
    project_id: Uuid,
) -> anyhow::Result<DatabaseDeployment> {
    let row = db::database_services::find(pool, project_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("database service settings are missing"))?;
    Ok(DatabaseDeployment {
        loopback_port: u16::try_from(row.loopback_port)?,
        credentials: open(secrets, &row)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_credentials_are_strong_and_engine_shaped() {
        for engine in ManagedDatabaseEngine::ALL {
            let creds = generate(engine);
            creds.validate(engine).unwrap();
            assert_eq!(creds.password.len(), GENERATED_PASSWORD_LEN);
        }
        assert_ne!(
            generate(ManagedDatabaseEngine::Postgres).password,
            generate(ManagedDatabaseEngine::Postgres).password
        );
    }

    #[test]
    fn sealed_credentials_open_only_for_their_service() {
        let sb = SecretBox::derive(&[7u8; 32]);
        let creds = generate(ManagedDatabaseEngine::Mysql);
        let id = Uuid::new_v4();
        let row = |project_id, ciphertext| DatabaseServiceRow {
            project_id,
            loopback_port: 25000,
            credentials_ciphertext: ciphertext,
            public_port: None,
            allow_from: vec![],
        };
        let sealed = seal(&sb, id, &creds);
        assert!(!sealed
            .windows(creds.password.len())
            .any(|w| w == creds.password.as_bytes()));
        assert_eq!(open(&sb, &row(id, sealed.clone())).unwrap(), creds);
        assert!(open(&sb, &row(Uuid::new_v4(), sealed)).is_err());
    }
}
