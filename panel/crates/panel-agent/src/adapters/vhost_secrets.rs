//! Secrets behind a domain's vhost options: the HTTP Basic Auth user file
//! and an uploaded TLS certificate with its private key.
//!
//! Files live in `<nginx_vhost_secrets_dir>/<domain>/` (directory 0711:
//! traversable but not listable). The TLS key and certificate are 0600 and
//! owned by root, which is the Nginx master that reads them. The htpasswd
//! file is read by the unprivileged Nginx *worker* on every request, so it
//! is 0600 and owned by the worker user (hashes only, never the key).
//! Passwords are hashed here with SHA-512 crypt; the plain text is never
//! written or logged. An uploaded pair is parsed and matched with `openssl`
//! in a staging area first; the active pair is swapped only when it is valid.

use crate::config::AgentConfig;
use panel_core::adapters::WriteVhostSecretsParams;
use panel_core::error::{CoreError, CoreResult};
use panel_core::types::validate;
use panel_core::vhost::{
    validate_basic_auth_users, validate_certificate, BasicAuthUser, CustomCertificate, SecretChange,
};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use tokio::io::AsyncWriteExt;

pub fn secrets_dir(cfg: &AgentConfig, domain: &str) -> PathBuf {
    cfg.nginx_vhost_secrets_dir.join(domain)
}

/// uid/gid of the Nginx worker (`user` directive of the main config,
/// default `www-data`), or `None` when it cannot be resolved.
fn nginx_worker_ids(cfg: &AgentConfig) -> Option<(u32, u32)> {
    let conf = std::fs::read_to_string(&cfg.nginx_main_conf).unwrap_or_default();
    let mut user = "www-data".to_string();
    let mut group: Option<String> = None;
    for line in conf.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if let Some(rest) = line.strip_prefix("user ") {
            let mut words = rest.trim_end_matches(';').split_whitespace();
            if let Some(name) = words.next() {
                user = name.to_string();
                group = words.next().map(str::to_string);
            }
            break;
        }
    }
    let passwd = std::fs::read_to_string(&cfg.passwd_file).ok()?;
    let entry = passwd.lines().find_map(|line| {
        let f: Vec<&str> = line.split(':').collect();
        (f.len() > 3 && f[0] == user).then(|| (f[2].parse::<u32>().ok(), f[3].parse::<u32>().ok()))
    })?;
    let uid = entry.0?;
    let mut gid = entry.1?;
    if let Some(group) = group {
        let groups = std::fs::read_to_string(&cfg.group_file).unwrap_or_default();
        if let Some(found) = groups.lines().find_map(|line| {
            let f: Vec<&str> = line.split(':').collect();
            (f.len() > 2 && f[0] == group).then(|| f[2].parse::<u32>().ok())?
        }) {
            gid = found;
        }
    }
    Some((uid, gid))
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
async fn write_private(path: &Path, content: &[u8], owner: Option<(u32, u32)>) -> CoreResult<()> {
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
    if let Some((uid, gid)) = owner {
        // Skipped when the agent already runs as that user (tests, root).
        if rustix::process::geteuid().as_raw() != uid || rustix::process::getegid().as_raw() != gid
        {
            if let Err(e) = std::os::unix::fs::chown(&staging, Some(uid), Some(gid)) {
                let _ = tokio::fs::remove_file(&staging).await;
                return Err(CoreError::Io(format!(
                    "giving the Nginx worker access to {}: {e}",
                    path.display()
                )));
            }
        }
    }
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
    let dir = secrets_dir(cfg, &params.domain);
    let wants_files = htpasswd.is_some() || matches!(params.certificate, SecretChange::Set(_));
    if wants_files {
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|e| CoreError::Io(format!("creating {}: {e}", dir.display())))?;
        for path in [cfg.nginx_vhost_secrets_dir.clone(), dir.clone()] {
            tokio::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o711))
                .await
                .map_err(|e| CoreError::Io(format!("securing {}: {e}", path.display())))?;
        }
    }
    // Validate and stage the certificate pair before anything is replaced:
    // a bad upload must leave the active (possibly referenced) pair alone.
    if let SecretChange::Set(cert) = &params.certificate {
        stage_certificate(&dir, cert).await?;
    }
    if let Some(content) = htpasswd {
        let owner = nginx_worker_ids(cfg);
        if let Err(error) = write_private(&dir.join("htpasswd"), content.as_bytes(), owner).await {
            discard_staged_certificate(&dir).await;
            return Err(error);
        }
    } else if matches!(params.basic_auth, SecretChange::Remove) {
        remove_if_present(&dir.join("htpasswd")).await;
    }
    match &params.certificate {
        SecretChange::Set(_) => install_staged_certificate(&dir).await?,
        SecretChange::Remove => {
            remove_if_present(&dir.join("cert.pem")).await;
            remove_if_present(&dir.join("key.pem")).await;
        }
        SecretChange::Keep => {}
    }
    Ok(())
}

async fn run_openssl(args: &[&str]) -> CoreResult<String> {
    super::exec::run("openssl", args)
        .await
        .map_err(|e| match e {
            CoreError::Io(_) => {
                CoreError::External(format!("running openssl (is it installed?): {e}"))
            }
            _ => CoreError::Validation(
                "the certificate or private key could not be parsed as an unencrypted PEM pair"
                    .into(),
            ),
        })
}

/// Writes the pair to `*.new` files and proves that it parses, is not
/// expired, and that the key belongs to the certificate.
async fn stage_certificate(dir: &Path, cert: &CustomCertificate) -> CoreResult<()> {
    let (cert_new, key_new) = (dir.join("cert.pem.new"), dir.join("key.pem.new"));
    let result = async {
        // `write_private` renames `<name>.tmp` onto the target; the staging
        // names here already end in `.new`, so they are installed by rename.
        write_private(&cert_new, cert.certificate_pem.as_bytes(), None).await?;
        write_private(&key_new, cert.private_key_pem.as_bytes(), None).await?;
        let cert_arg = cert_new.to_string_lossy().into_owned();
        let key_arg = key_new.to_string_lossy().into_owned();
        let from_cert = run_openssl(&["x509", "-in", &cert_arg, "-noout", "-pubkey"]).await?;
        let from_key = run_openssl(&["pkey", "-in", &key_arg, "-pubout"]).await?;
        if from_cert.is_empty() || from_cert != from_key {
            return Err(CoreError::Validation(
                "the private key does not match the certificate".into(),
            ));
        }
        run_openssl(&["x509", "-in", &cert_arg, "-noout", "-checkend", "0"])
            .await
            .map_err(|_| CoreError::Validation("the certificate has expired".into()))?;
        Ok(())
    }
    .await;
    if result.is_err() {
        discard_staged_certificate(dir).await;
    }
    result
}

async fn discard_staged_certificate(dir: &Path) {
    remove_if_present(&dir.join("cert.pem.new")).await;
    remove_if_present(&dir.join("key.pem.new")).await;
}

/// Swaps the staged pair in; if the second rename fails the first is undone.
async fn install_staged_certificate(dir: &Path) -> CoreResult<()> {
    let (cert, key) = (dir.join("cert.pem"), dir.join("key.pem"));
    let old_cert = tokio::fs::read(&cert).await.ok();
    tokio::fs::rename(dir.join("cert.pem.new"), &cert)
        .await
        .map_err(|e| CoreError::Io(format!("installing {}: {e}", cert.display())))?;
    if let Err(e) = tokio::fs::rename(dir.join("key.pem.new"), &key).await {
        match old_cert {
            Some(old) => {
                let _ = write_private(&cert, &old, None).await;
            }
            None => remove_if_present(&cert).await,
        }
        discard_staged_certificate(dir).await;
        return Err(CoreError::Io(format!("installing {}: {e}", key.display())));
    }
    Ok(())
}

/// Deletes all vhost secrets of a domain (its binding was removed).
pub async fn remove_domain_secrets(cfg: &AgentConfig, domain: &str) {
    if validate::is_valid_domain(domain) {
        let _ = tokio::fs::remove_dir_all(secrets_dir(cfg, domain)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(root: &Path) -> AgentConfig {
        AgentConfig {
            nginx_vhost_secrets_dir: root.join("secrets"),
            // No worker user to resolve: ownership is left as is.
            passwd_file: root.join("no-passwd"),
            ..AgentConfig::default()
        }
    }

    fn user(name: &str, password: &str) -> BasicAuthUser {
        BasicAuthUser {
            username: name.into(),
            password: password.into(),
        }
    }

    /// A self-signed pair made with the host's openssl, or `None` without it.
    async fn make_pair(dir: &Path, name: &str, days: &str) -> Option<CustomCertificate> {
        let (c, k) = (
            dir.join(format!("{name}.crt")),
            dir.join(format!("{name}.key")),
        );
        let (c_arg, k_arg) = (
            c.to_string_lossy().into_owned(),
            k.to_string_lossy().into_owned(),
        );
        super::super::exec::run(
            "openssl",
            &[
                "req",
                "-x509",
                "-newkey",
                "ec",
                "-pkeyopt",
                "ec_paramgen_curve:prime256v1",
                "-nodes",
                "-subj",
                "/CN=shop.example.com",
                "-days",
                days,
                "-keyout",
                &k_arg,
                "-out",
                &c_arg,
            ],
        )
        .await
        .ok()?;
        Some(CustomCertificate {
            certificate_pem: std::fs::read_to_string(&c).unwrap(),
            private_key_pem: std::fs::read_to_string(&k).unwrap(),
        })
    }

    fn params(
        basic_auth: SecretChange<Vec<BasicAuthUser>>,
        certificate: SecretChange<CustomCertificate>,
    ) -> WriteVhostSecretsParams {
        WriteVhostSecretsParams {
            project_slug: "shop".into(),
            domain: "shop.example.com".into(),
            basic_auth,
            certificate,
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

    #[test]
    fn worker_user_comes_from_the_nginx_config() {
        let tmp = std::env::temp_dir().join(format!("scontrol-vhost-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(
            tmp.join("nginx.conf"),
            "# c\nuser  web deploy;\nevents {}\n",
        )
        .unwrap();
        std::fs::write(
            tmp.join("passwd"),
            "root:x:0:0::/root:/bin/sh\nweb:x:33:34::/:/bin/false\n",
        )
        .unwrap();
        std::fs::write(tmp.join("group"), "deploy:x:77:\n").unwrap();
        let config = AgentConfig {
            nginx_main_conf: tmp.join("nginx.conf"),
            passwd_file: tmp.join("passwd"),
            group_file: tmp.join("group"),
            ..AgentConfig::default()
        };
        assert_eq!(nginx_worker_ids(&config), Some((33, 77)));
        std::fs::write(tmp.join("nginx.conf"), "events {}\n").unwrap();
        assert_eq!(nginx_worker_ids(&config), None); // www-data absent
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[tokio::test]
    async fn secrets_are_written_private_and_removed() {
        let tmp = std::env::temp_dir().join(format!("scontrol-vhost-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        let config = cfg(&tmp);
        let Some(cert) = make_pair(&tmp, "a", "30").await else {
            eprintln!("openssl not installed; skipping");
            return;
        };
        write_vhost_secrets(
            &config,
            params(
                SecretChange::Set(vec![user("alice", "longenough")]),
                SecretChange::Set(cert),
            ),
        )
        .await
        .unwrap();
        let dir = secrets_dir(&config, "shop.example.com");
        for name in ["htpasswd", "cert.pem", "key.pem"] {
            let mode = std::fs::metadata(dir.join(name))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "{name}");
        }
        // Traversable (the Nginx worker reads htpasswd) but not listable.
        assert_eq!(
            std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o711
        );
        assert!(!dir.join("cert.pem.new").exists() && !dir.join("key.pem.new").exists());
        assert!(std::fs::read_to_string(dir.join("htpasswd"))
            .unwrap()
            .starts_with("alice:$6$"));

        write_vhost_secrets(&config, params(SecretChange::Keep, SecretChange::Remove))
            .await
            .unwrap();
        assert!(dir.join("htpasswd").exists());
        assert!(!dir.join("key.pem").exists() && !dir.join("cert.pem").exists());

        let bad = write_vhost_secrets(
            &config,
            WriteVhostSecretsParams {
                project_slug: "../x".into(),
                ..params(SecretChange::Keep, SecretChange::Keep)
            },
        )
        .await;
        assert!(bad.is_err());
        remove_domain_secrets(&config, "shop.example.com").await;
        assert!(!dir.exists());
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[tokio::test]
    async fn a_bad_replacement_leaves_the_active_pair_untouched() {
        let tmp = std::env::temp_dir().join(format!("scontrol-vhost-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        let config = cfg(&tmp);
        let (Some(good), Some(other), Some(expired)) = (
            make_pair(&tmp, "good", "30").await,
            make_pair(&tmp, "other", "30").await,
            make_pair(&tmp, "old", "-1").await,
        ) else {
            eprintln!("openssl not installed; skipping");
            return;
        };
        write_vhost_secrets(
            &config,
            params(SecretChange::Keep, SecretChange::Set(good.clone())),
        )
        .await
        .unwrap();
        let dir = secrets_dir(&config, "shop.example.com");
        let mismatched = CustomCertificate {
            certificate_pem: good.certificate_pem.clone(),
            private_key_pem: other.private_key_pem.clone(),
        };
        let malformed = CustomCertificate {
            certificate_pem: "-----BEGIN CERTIFICATE-----\nAAA\n-----END CERTIFICATE-----\n".into(),
            private_key_pem: good.private_key_pem.clone(),
        };
        for bad in [mismatched, malformed, expired] {
            let err =
                write_vhost_secrets(&config, params(SecretChange::Keep, SecretChange::Set(bad)))
                    .await
                    .unwrap_err();
            assert!(matches!(err, CoreError::Validation(_)), "{err:?}");
            assert_eq!(
                std::fs::read_to_string(dir.join("cert.pem")).unwrap(),
                good.certificate_pem
            );
            assert_eq!(
                std::fs::read_to_string(dir.join("key.pem")).unwrap(),
                good.private_key_pem
            );
            assert!(!dir.join("cert.pem.new").exists() && !dir.join("key.pem.new").exists());
        }
        // A valid replacement does go in.
        write_vhost_secrets(
            &config,
            params(SecretChange::Keep, SecretChange::Set(other.clone())),
        )
        .await
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("cert.pem")).unwrap(),
            other.certificate_pem
        );
        std::fs::remove_dir_all(&tmp).ok();
    }
}
