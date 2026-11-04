//! Secrets behind a domain's vhost options: the HTTP Basic Auth user file
//! and an uploaded TLS certificate with its private key.
//!
//! Files live in `<project root>/vhost/<domain>/` (directory 0700, files
//! 0600, owned by the root agent), so only root, which also runs the Nginx
//! master process that reads them, can open them. Passwords are hashed
//! here with SHA-512 crypt; the plain text is never written or logged.

use crate::config::AgentConfig;
use panel_core::adapters::WriteVhostSecretsParams;
use panel_core::error::{CoreError, CoreResult};
use panel_core::types::validate;
use panel_core::vhost::{
    validate_basic_auth_users, validate_certificate, BasicAuthUser, SecretChange,
};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use tokio::io::AsyncWriteExt;

pub fn secrets_dir(project_root: &Path, domain: &str) -> PathBuf {
    project_root.join("vhost").join(domain)
}

fn hash_user(user: &BasicAuthUser) -> CoreResult<String> {
    let params = sha_crypt::Sha512Params::new(sha_crypt::ROUNDS_DEFAULT)
        .map_err(|e| CoreError::External(format!("SHA512-CRYPT parameters: {e:?}")))?;
    let hash = sha_crypt::sha512_simple(&user.password, &params)
        .map_err(|e| CoreError::External(format!("hashing password: {e:?}")))?;
    Ok(format!("{}:{hash}\n", user.username))
}

/// htpasswd content for `users` (crypt(3) `$6$` hashes, which Nginx hands to
/// the C library).
pub fn render_htpasswd(users: &[BasicAuthUser]) -> CoreResult<String> {
    validate_basic_auth_users(users).map_err(CoreError::Validation)?;
    users.iter().map(hash_user).collect()
}

/// Writes `content` to `path` as a new 0600 file and renames it into place,
/// so the secret is never visible with looser permissions.
async fn write_private(path: &Path, content: &[u8]) -> CoreResult<()> {
    let staging = path.with_extension("tmp");
    let _ = tokio::fs::remove_file(&staging).await;
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&staging)
        .await
        .map_err(|e| CoreError::Io(format!("creating {}: {e}", staging.display())))?;
    file.write_all(content)
        .await
        .map_err(|e| CoreError::Io(format!("writing {}: {e}", staging.display())))?;
    file.flush().await.ok();
    drop(file);
    tokio::fs::rename(&staging, path)
        .await
        .map_err(|e| CoreError::Io(format!("installing {}: {e}", path.display())))
}

async fn remove_if_present(path: &Path) {
    let _ = tokio::fs::remove_file(path).await;
}

pub async fn write_vhost_secrets(
    cfg: &AgentConfig,
    params: WriteVhostSecretsParams,
) -> CoreResult<()> {
    if !validate::is_valid_project_slug(&params.project_slug) {
        return Err(CoreError::Validation("invalid project slug".into()));
    }
    if !validate::is_valid_domain(&params.domain) {
        return Err(CoreError::Validation("invalid domain".into()));
    }
    // Validate everything before touching the disk.
    let htpasswd = match &params.basic_auth {
        SecretChange::Set(users) => Some(render_htpasswd(users)?),
        _ => None,
    };
    if let SecretChange::Set(cert) = &params.certificate {
        validate_certificate(cert).map_err(CoreError::Validation)?;
    }
    let root = cfg.docker_projects_root.join(&params.project_slug);
    let dir = secrets_dir(&root, &params.domain);
    let wants_files = htpasswd.is_some() || matches!(params.certificate, SecretChange::Set(_));
    if wants_files {
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|e| CoreError::Io(format!("creating {}: {e}", dir.display())))?;
        for path in [root.join("vhost"), dir.clone()] {
            tokio::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
                .await
                .map_err(|e| CoreError::Io(format!("securing {}: {e}", path.display())))?;
        }
    }
    if let Some(content) = htpasswd {
        write_private(&dir.join("htpasswd"), content.as_bytes()).await?;
    } else if matches!(params.basic_auth, SecretChange::Remove) {
        remove_if_present(&dir.join("htpasswd")).await;
    }
    match &params.certificate {
        SecretChange::Set(cert) => {
            write_private(&dir.join("cert.pem"), cert.certificate_pem.as_bytes()).await?;
            write_private(&dir.join("key.pem"), cert.private_key_pem.as_bytes()).await?;
        }
        SecretChange::Remove => {
            remove_if_present(&dir.join("cert.pem")).await;
            remove_if_present(&dir.join("key.pem")).await;
        }
        SecretChange::Keep => {}
    }
    Ok(())
}

/// Deletes all vhost secrets of a domain (its binding was removed).
pub async fn remove_domain_secrets(project_root: &Path, domain: &str) {
    if validate::is_valid_domain(domain) {
        let _ = tokio::fs::remove_dir_all(secrets_dir(project_root, domain)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use panel_core::vhost::CustomCertificate;

    fn cfg(root: &Path) -> AgentConfig {
        AgentConfig {
            docker_projects_root: root.to_path_buf(),
            ..AgentConfig::default()
        }
    }

    fn user(name: &str, password: &str) -> BasicAuthUser {
        BasicAuthUser {
            username: name.into(),
            password: password.into(),
        }
    }

    #[test]
    fn htpasswd_holds_only_hashes() {
        let text = render_htpasswd(&[
            user("alice", "correct horse"),
            user("bob", "battery staple"),
        ])
        .unwrap();
        assert!(!text.contains("correct horse") && !text.contains("battery"));
        let line = text.lines().next().unwrap();
        let (name, hash) = line.split_once(':').unwrap();
        assert_eq!(name, "alice");
        assert!(hash.starts_with("$6$"));
        sha_crypt::sha512_check("correct horse", hash).unwrap();
        assert_eq!(text.lines().count(), 2);
        assert!(render_htpasswd(&[user("a:b", "longenough")]).is_err());
    }

    #[tokio::test]
    async fn secrets_are_written_0600_and_removed() {
        let tmp = std::env::temp_dir().join(format!("scontrol-vhost-{}", uuid::Uuid::new_v4()));
        let config = cfg(&tmp);
        let cert = CustomCertificate {
            certificate_pem: "-----BEGIN CERTIFICATE-----\nAAA\n-----END CERTIFICATE-----\n".into(),
            private_key_pem: "-----BEGIN PRIVATE KEY-----\nBBB\n-----END PRIVATE KEY-----\n".into(),
        };
        write_vhost_secrets(
            &config,
            WriteVhostSecretsParams {
                project_slug: "shop".into(),
                domain: "shop.example.com".into(),
                basic_auth: SecretChange::Set(vec![user("alice", "longenough")]),
                certificate: SecretChange::Set(cert),
            },
        )
        .await
        .unwrap();
        let dir = secrets_dir(&tmp.join("shop"), "shop.example.com");
        for name in ["htpasswd", "cert.pem", "key.pem"] {
            let mode = std::fs::metadata(dir.join(name))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "{name}");
        }
        assert_eq!(
            std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert!(std::fs::read_to_string(dir.join("htpasswd"))
            .unwrap()
            .starts_with("alice:$6$"));

        // Keep leaves files alone; Remove deletes them.
        write_vhost_secrets(
            &config,
            WriteVhostSecretsParams {
                project_slug: "shop".into(),
                domain: "shop.example.com".into(),
                basic_auth: SecretChange::Keep,
                certificate: SecretChange::Remove,
            },
        )
        .await
        .unwrap();
        assert!(dir.join("htpasswd").exists());
        assert!(!dir.join("key.pem").exists() && !dir.join("cert.pem").exists());

        // Invalid input is rejected before anything is written.
        let bad = write_vhost_secrets(
            &config,
            WriteVhostSecretsParams {
                project_slug: "../x".into(),
                domain: "shop.example.com".into(),
                basic_auth: SecretChange::Keep,
                certificate: SecretChange::Keep,
            },
        )
        .await;
        assert!(bad.is_err());
        remove_domain_secrets(&tmp.join("shop"), "shop.example.com").await;
        assert!(!dir.exists());
        std::fs::remove_dir_all(&tmp).ok();
    }
}
