//! Private registry credentials for image pulls.
//!
//! Each project gets its own Docker client configuration directory,
//! `<project root>/.docker`, handed to `docker compose` through the
//! `DOCKER_CONFIG` environment variable. Isolating it means a project never
//! sees another project's registry logins, nor root's own `~/.docker`.
//!
//! ```text
//! .docker/            0700
//! .docker/config.json 0600  {"auths": {"<registry>": {"auth": base64(user:password)}}}
//! ```
//!
//! The password never appears on argv (`ps` shows argv to every local user):
//! deployments read it from `config.json`, and `docker login` for a
//! connectivity test gets it on stdin. The panel writes the file right
//! before a deployment and removes it right after (see
//! `panel-api/src/services/deploy_queue.rs`), and the agent masks the
//! password in deployment logs.

use super::exec::run_with_stdin_env;
use crate::config::AgentConfig;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use panel_core::adapters::{DockerRegistryAuth, TestDockerRegistryParams, WriteDockerConfigParams};
use panel_core::error::{CoreError, CoreResult};
use panel_core::types::validate;
use std::path::{Path, PathBuf};

const CONFIG_DIR: &str = ".docker";
const CONFIG_FILE: &str = "config.json";
const MAX_REGISTRIES: usize = 16;

/// The `DOCKER_CONFIG` directory of the project rooted at `root`.
pub fn config_dir(root: &Path) -> PathBuf {
    root.join(CONFIG_DIR)
}

/// Replaces the project's `config.json` (or removes the directory when no
/// registries are given).
pub async fn write_config(cfg: &AgentConfig, params: WriteDockerConfigParams) -> CoreResult<()> {
    if !validate::is_valid_project_slug(&params.project_slug) {
        return Err(CoreError::Validation(format!(
            "invalid project slug: {}",
            params.project_slug
        )));
    }
    let root = cfg.docker_projects_root.join(&params.project_slug);
    let dir = config_dir(&root);
    if params.registries.is_empty() {
        return match tokio::fs::remove_dir_all(&dir).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(CoreError::Io(format!("removing registry credentials: {e}"))),
        };
    }
    let body = render_config(&params.registries)?;
    create_private_dir(&root).await?;
    create_private_dir(&dir).await?;
    // Stage, then rename, so a reader never sees a half-written file.
    let staged = dir.join("config.json.new");
    write_private(&staged, body.as_bytes()).await?;
    tokio::fs::rename(&staged, dir.join(CONFIG_FILE))
        .await
        .map_err(|e| CoreError::Io(format!("installing registry credentials: {e}")))
}

/// `docker login` against the registry with the password on stdin and a
/// throwaway `DOCKER_CONFIG`, so nothing is left behind on disk.
pub async fn test_registry(cfg: &AgentConfig, params: TestDockerRegistryParams) -> CoreResult<()> {
    let auth = validated(&params.auth)?;
    let scratch = cfg
        .docker_projects_root
        .join(format!(".registry-test-{}", uuid::Uuid::new_v4()));
    create_private_dir(&scratch).await?;
    let dir = scratch.to_string_lossy().into_owned();
    // Docker Hub is the default registry, so `docker login` takes no server.
    let mut args = vec![
        "login",
        "--username",
        auth.username.as_str(),
        "--password-stdin",
    ];
    if auth.key != validate::DOCKER_HUB_AUTH_KEY {
        args.push(auth.key.as_str());
    }
    let result = run_with_stdin_env(
        "docker",
        &args,
        &[("DOCKER_CONFIG", dir.as_str())],
        auth.password.as_bytes(),
    )
    .await;
    tokio::fs::remove_dir_all(&scratch).await.ok();
    result.map(|_| ()).map_err(|e| match e {
        CoreError::External(message) => CoreError::External(mask(&message, &auth.password)),
        other => other,
    })
}

/// Values a deployment log must mask: the stored passwords and their
/// base64 `auth` forms.
pub async fn stored_secrets(root: &Path) -> Vec<String> {
    let Ok(body) = tokio::fs::read_to_string(config_dir(root).join(CONFIG_FILE)).await else {
        return Vec::new();
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&body) else {
        return Vec::new();
    };
    let mut secrets = Vec::new();
    for entry in json["auths"]
        .as_object()
        .into_iter()
        .flat_map(|m| m.values())
    {
        let Some(auth) = entry["auth"].as_str() else {
            continue;
        };
        secrets.push(auth.to_string());
        if let Some((_, password)) = STANDARD
            .decode(auth)
            .ok()
            .and_then(|raw| String::from_utf8(raw).ok())
            .and_then(|pair| {
                pair.split_once(':')
                    .map(|(u, p)| (u.to_string(), p.to_string()))
            })
        {
            secrets.push(password);
        }
    }
    secrets
}

struct Validated {
    key: String,
    username: String,
    password: String,
}

fn validated(auth: &DockerRegistryAuth) -> CoreResult<Validated> {
    let key = validate::registry_auth_key(&auth.server)
        .ok_or_else(|| CoreError::Validation("invalid registry address".into()))?;
    if !validate::is_valid_git_username(&auth.username) {
        return Err(CoreError::Validation("invalid registry username".into()));
    }
    if !validate::is_valid_registry_secret(&auth.password) {
        return Err(CoreError::Validation(
            "registry password must be 1..=4096 printable characters".into(),
        ));
    }
    Ok(Validated {
        key,
        username: auth.username.clone(),
        password: auth.password.clone(),
    })
}

fn render_config(registries: &[DockerRegistryAuth]) -> CoreResult<String> {
    if registries.len() > MAX_REGISTRIES {
        return Err(CoreError::Validation("too many registries".into()));
    }
    let mut auths = serde_json::Map::new();
    for registry in registries {
        let v = validated(registry)?;
        let pair = format!("{}:{}", v.username, v.password);
        auths.insert(
            v.key,
            serde_json::json!({ "auth": STANDARD.encode(pair.as_bytes()) }),
        );
    }
    serde_json::to_string_pretty(&serde_json::json!({ "auths": auths }))
        .map_err(|e| CoreError::Io(format!("rendering registry config: {e}")))
}

fn mask(message: &str, password: &str) -> String {
    message.replace(password, "********")
}

async fn create_private_dir(dir: &Path) -> CoreResult<()> {
    tokio::fs::create_dir_all(dir)
        .await
        .map_err(|e| CoreError::Io(format!("creating {}: {e}", dir.display())))?;
    #[cfg(unix)]
    tokio::fs::set_permissions(dir, std::os::unix::fs::PermissionsExt::from_mode(0o700))
        .await
        .map_err(|e| CoreError::Io(format!("protecting {}: {e}", dir.display())))?;
    Ok(())
}

async fn write_private(path: &Path, body: &[u8]) -> CoreResult<()> {
    let mut options = tokio::fs::OpenOptions::new();
    options.create(true).write(true).truncate(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options
        .open(path)
        .await
        .map_err(|e| CoreError::Io(format!("writing registry credentials: {e}")))?;
    tokio::io::AsyncWriteExt::write_all(&mut file, body)
        .await
        .map_err(|e| CoreError::Io(format!("writing registry credentials: {e}")))?;
    #[cfg(unix)]
    tokio::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o600))
        .await
        .map_err(|e| CoreError::Io(format!("protecting registry credentials: {e}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn auth(server: &str, username: &str, password: &str) -> DockerRegistryAuth {
        DockerRegistryAuth {
            server: server.into(),
            username: username.into(),
            password: password.into(),
        }
    }

    fn config(root: &Path) -> AgentConfig {
        AgentConfig {
            docker_projects_root: root.to_path_buf(),
            docker_volumes_root: std::path::PathBuf::from("/nonexistent-volumes"),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn config_json_is_private_decodes_to_the_credentials_and_is_removable() {
        let dir = std::env::temp_dir().join(format!("scontrol-dcfg-{}", uuid::Uuid::new_v4()));
        let cfg = config(&dir);
        write_config(
            &cfg,
            WriteDockerConfigParams {
                project_slug: "shop".into(),
                registries: vec![
                    auth("GHCR.io", "octocat", "ghp_s3cret"),
                    auth("docker.io", "hubuser", "hub-token"),
                ],
            },
        )
        .await
        .unwrap();
        let root = dir.join("shop");
        let file = config_dir(&root).join(CONFIG_FILE);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&file), 0o600);
            assert_eq!(mode(&config_dir(&root)), 0o700);
        }
        let json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        let decode = |key: &str| {
            String::from_utf8(
                STANDARD
                    .decode(json["auths"][key]["auth"].as_str().unwrap())
                    .unwrap(),
            )
            .unwrap()
        };
        assert_eq!(decode("ghcr.io"), "octocat:ghp_s3cret");
        assert_eq!(decode(validate::DOCKER_HUB_AUTH_KEY), "hubuser:hub-token");
        assert!(json.get("credsStore").is_none());
        assert!(!dir.join("shop/.docker/config.json.new").exists());

        let secrets = stored_secrets(&root).await;
        assert!(secrets.contains(&"ghp_s3cret".to_string()));
        assert!(secrets.contains(&"hub-token".to_string()));

        write_config(
            &cfg,
            WriteDockerConfigParams {
                project_slug: "shop".into(),
                registries: vec![],
            },
        )
        .await
        .unwrap();
        assert!(!config_dir(&root).exists());
        // Removing again is not an error.
        write_config(
            &cfg,
            WriteDockerConfigParams {
                project_slug: "shop".into(),
                registries: vec![],
            },
        )
        .await
        .unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn hostile_values_are_rejected_before_anything_is_written() {
        let dir = std::env::temp_dir().join(format!("scontrol-dcfg-{}", uuid::Uuid::new_v4()));
        let cfg = config(&dir);
        for bad in [
            auth("ghcr.io/org", "u", "p"),
            auth("ghcr.io", "-u", "p"),
            auth("ghcr.io", "u", "two words"),
            auth("ghcr.io", "u", "line\nbreak"),
            auth("ghcr.io", "u", ""),
        ] {
            let result = write_config(
                &cfg,
                WriteDockerConfigParams {
                    project_slug: "shop".into(),
                    registries: vec![bad.clone()],
                },
            )
            .await;
            assert!(matches!(result, Err(CoreError::Validation(_))), "{bad:?}");
        }
        let result = write_config(
            &cfg,
            WriteDockerConfigParams {
                project_slug: "../etc".into(),
                registries: vec![auth("ghcr.io", "u", "p")],
            },
        )
        .await;
        assert!(matches!(result, Err(CoreError::Validation(_))));
        assert!(!dir.exists(), "nothing may be created for invalid input");
    }

    #[test]
    fn debug_output_hides_the_password() {
        let shown = format!("{:?}", auth("ghcr.io", "octocat", "ghp_s3cret"));
        assert!(!shown.contains("ghp_s3cret"), "{shown}");
    }
}
