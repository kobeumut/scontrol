//! Deployment releases for rollbacks.
//!
//! After a successful Compose deployment the agent pins the image every
//! running service uses under `<slug>/<service>:deploy-<deployment>` and
//! keeps a private copy of the Compose file it deployed:
//!
//! ```text
//! releases/<deployment>/compose.yml    0600 the Compose file as deployed
//! releases/<deployment>/release.json   0600 its SHA-256 and the pinned images
//! ```
//!
//! panel-api stores a sanitized [`DeploymentSnapshot`] of the same release
//! (no environment values, no known secret). A rollback names the target
//! release with the hash and images from that snapshot; both must match the
//! agent's own record, and every tag must still point at the recorded
//! image. The rollback Compose file is the release copy with each pinned
//! service switched to its tag, `build` removed, and `pull_policy: never`,
//! so nothing is fetched or built: `docker compose up` only swaps
//! containers. Environment values come from the current 0600 `env` file.
//!
//! The newest `rollback_releases_retained` releases are kept; older ones are
//! removed together with their tags (`docker image rm <tag>` drops the tag,
//! and the image only once nothing else references it).

use super::compose::{create_private_dir, write_private_file};
use super::deploy_log::{redact, DeployLog};
use super::exec::{run, run_allow_nonzero};
use panel_core::error::{CoreError, CoreResult};
use panel_core::types::{
    deployment_image_tag, validate, DeploymentSnapshot, PinnedImage, RollbackTarget,
};
use serde::{Deserialize, Serialize};
use serde_yaml::Value as Yaml;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use uuid::Uuid;

const RELEASES_DIR: &str = "releases";
const RELEASE_FILE: &str = "release.json";
const RELEASE_COMPOSE: &str = "compose.yml";
/// Generated in the checkout for a rollback, so relative paths resolve the
/// same way as for the deployed file.
pub(crate) const ROLLBACK_COMPOSE_FILE: &str = ".scontrol-rollback.yml";
const MASK: &str = "********";
/// Upper bound for releases kept per project, whatever the config says.
pub(crate) const MAX_RELEASES_RETAINED: usize = 20;

/// The agent's own record of a release (`release.json`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Release {
    deployment_id: Uuid,
    compose_path: String,
    compose_sha256: String,
    images: Vec<PinnedImage>,
}

pub(crate) fn releases_dir(root: &Path) -> PathBuf {
    root.join(RELEASES_DIR)
}

fn release_dir(root: &Path, deployment_id: Uuid) -> PathBuf {
    releases_dir(root).join(deployment_id.to_string())
}

pub(crate) fn sha256_hex(body: &[u8]) -> String {
    format!("{:x}", Sha256::digest(body))
}

/// What [`record`] needs to know about the finished deployment.
pub(crate) struct DeployedRelease<'a> {
    pub slug: &'a str,
    pub deployment_id: Uuid,
    pub root: &'a Path,
    /// Compose file path relative to `code/`.
    pub compose_path: &'a str,
    /// Exact bytes of the deployed Compose file.
    pub compose_body: &'a str,
    pub compose: &'a Yaml,
    pub override_body: Option<&'a str>,
    pub env_keys: Vec<String>,
    /// Running container per service, from `docker compose ps -q`.
    pub containers: Vec<(String, Option<String>)>,
}

/// Pins the running images, keeps the release copy, and returns the
/// sanitized snapshot panel-api stores. On error nothing is left behind.
pub(crate) async fn record(
    release: DeployedRelease<'_>,
    log: &mut DeployLog,
) -> CoreResult<DeploymentSnapshot> {
    let services = release
        .compose
        .get("services")
        .and_then(Yaml::as_mapping)
        .ok_or_else(|| CoreError::Validation("Compose file requires a services mapping".into()))?;
    let mut planned = Vec::new();
    for (name, service) in services {
        let name = name.as_str().unwrap_or_default();
        let container = release
            .containers
            .iter()
            .find(|(service, _)| service == name)
            .and_then(|(_, id)| id.clone());
        let Some(container) = container else {
            if service.get("build").is_some() {
                // Without a pin the rollback would start the newer build.
                return Err(CoreError::Validation(format!(
                    "service {name} is built but has no running container to pin"
                )));
            }
            continue;
        };
        let tag =
            deployment_image_tag(release.slug, name, release.deployment_id).ok_or_else(|| {
                CoreError::Validation(format!(
                    "service {name} cannot be pinned: image tags need a lowercase service name"
                ))
            })?;
        planned.push((name.to_string(), container, tag));
    }

    let mut images = Vec::new();
    for (service, container, tag) in planned {
        let pinned = pin(&container, &tag).await;
        match pinned {
            Ok(image_id) => images.push(PinnedImage {
                service,
                tag,
                image_id,
            }),
            Err(error) => {
                untag(&images).await;
                return Err(error);
            }
        }
    }
    let dir = release_dir(release.root, release.deployment_id);
    let record = Release {
        deployment_id: release.deployment_id,
        compose_path: release.compose_path.to_string(),
        compose_sha256: sha256_hex(release.compose_body.as_bytes()),
        images: images.clone(),
    };
    let written = async {
        create_private_dir(&releases_dir(release.root)).await?;
        create_private_dir(&dir).await?;
        write_private_file(&dir.join(RELEASE_COMPOSE), release.compose_body).await?;
        write_record(&dir, &record).await
    }
    .await;
    if let Err(error) = written {
        untag(&images).await;
        tokio::fs::remove_dir_all(&dir).await.ok();
        return Err(error);
    }
    for image in &images {
        log.step(&format!("pinned {} as {}", image.service, image.tag))
            .await?;
    }
    let secrets = log.secrets().to_vec();
    Ok(DeploymentSnapshot {
        compose_path: record.compose_path,
        compose_sha256: record.compose_sha256,
        compose_file: sanitize_compose(release.compose, &secrets)?,
        override_file: release.override_body.map(|body| redact(body, &secrets)),
        env_keys: release.env_keys,
        images,
    })
}

async fn write_record(dir: &Path, record: &Release) -> CoreResult<()> {
    let body = serde_json::to_string_pretty(record)
        .map_err(|e| CoreError::Io(format!("encoding release record: {e}")))?;
    write_private_file(&dir.join(RELEASE_FILE), &body).await
}

/// Tags the image `container` runs as `tag`; returns the image ID.
async fn pin(container: &str, tag: &str) -> CoreResult<String> {
    let image_id = run(
        "docker",
        &["inspect", "--format", "{{.Image}}", "--", container],
    )
    .await?
    .trim()
    .to_string();
    if !is_image_id(&image_id) {
        return Err(CoreError::External(format!(
            "unexpected image ID for container {container}"
        )));
    }
    run("docker", &["tag", &image_id, tag]).await?;
    Ok(image_id)
}

fn is_image_id(id: &str) -> bool {
    id.strip_prefix("sha256:")
        .is_some_and(|hex| hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// Removes release tags. Failures are ignored: an image that a container
/// still uses keeps its tag until the next prune.
async fn untag(images: &[PinnedImage]) {
    for image in images {
        if validate::is_valid_image_reference(&image.tag) {
            run_allow_nonzero("docker", &["image", "rm", &image.tag])
                .await
                .ok();
        }
    }
}

/// Loads the target release and checks it against what panel-api recorded:
/// same Compose hash, same images, tags of this very deployment, and an
/// unchanged release copy. Returns the release's Compose body.
async fn load_release(root: &Path, slug: &str, target: &RollbackTarget) -> CoreResult<String> {
    let dir = release_dir(root, target.deployment_id);
    let record: Release = match tokio::fs::read_to_string(dir.join(RELEASE_FILE)).await {
        Ok(body) => serde_json::from_str(&body)
            .map_err(|e| CoreError::Validation(format!("unreadable release record: {e}")))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(CoreError::NotFound(format!(
                "release of deployment {} is no longer kept on this server",
                target.deployment_id
            )))
        }
        Err(e) => return Err(CoreError::Io(format!("reading release record: {e}"))),
    };
    if record.deployment_id != target.deployment_id
        || record.compose_sha256 != target.compose_sha256
        || record.images != target.images
    {
        return Err(CoreError::Validation(
            "the release on this server does not match the recorded snapshot".into(),
        ));
    }
    for image in &record.images {
        let expected = deployment_image_tag(slug, &image.service, target.deployment_id);
        if expected.as_deref() != Some(image.tag.as_str()) || !is_image_id(&image.image_id) {
            return Err(CoreError::Validation(format!(
                "release image for service {} is not a tag of this deployment",
                image.service
            )));
        }
    }
    let body = tokio::fs::read_to_string(dir.join(RELEASE_COMPOSE))
        .await
        .map_err(|e| CoreError::Io(format!("reading release Compose file: {e}")))?;
    if sha256_hex(body.as_bytes()) != record.compose_sha256 {
        return Err(CoreError::Validation(
            "the release Compose file changed since it was recorded".into(),
        ));
    }
    Ok(body)
}

/// Prepares a rollback: verifies the release and its tags, then writes the
/// rollback Compose file into `code/` and returns its path.
pub(crate) async fn prepare_rollback(
    root: &Path,
    code_dir: &Path,
    slug: &str,
    target: &RollbackTarget,
    log: &mut DeployLog,
) -> CoreResult<PathBuf> {
    log.step(&format!(
        "rolling back to deployment {} (no fetch, build, or pull)",
        target.deployment_id
    ))
    .await?;
    let body = load_release(root, slug, target).await?;
    for image in &target.images {
        let (code, stdout, _) = run_allow_nonzero(
            "docker",
            &["image", "inspect", "--format", "{{.Id}}", "--", &image.tag],
        )
        .await?;
        if code != 0 {
            return Err(CoreError::NotFound(format!(
                "image {} of service {} no longer exists on this server",
                image.tag, image.service
            )));
        }
        if stdout.trim() != image.image_id {
            return Err(CoreError::Validation(format!(
                "image {} no longer points at the recorded image",
                image.tag
            )));
        }
        log.step(&format!("{} → {}", image.service, image.tag))
            .await?;
    }
    if !code_dir.is_dir() {
        return Err(CoreError::NotFound(
            "the project's source directory is missing; deploy it once before rolling back".into(),
        ));
    }
    let compose: Yaml = serde_yaml::from_str(&body)
        .map_err(|e| CoreError::Validation(format!("invalid release Compose YAML: {e}")))?;
    let rendered = render_rollback(&compose, &target.images)?;
    let path = code_dir.join(ROLLBACK_COMPOSE_FILE);
    super::compose::write_generated_file(&path, &rendered).await?;
    Ok(path)
}

/// The release's Compose file with every pinned service running its tag:
/// `image` replaced, `build` removed, `pull_policy: never`.
fn render_rollback(compose: &Yaml, images: &[PinnedImage]) -> CoreResult<String> {
    let mut doc = compose.clone();
    let services = doc
        .get_mut("services")
        .and_then(Yaml::as_mapping_mut)
        .ok_or_else(|| CoreError::Validation("Compose file requires a services mapping".into()))?;
    for image in images {
        let service = services
            .get_mut(image.service.as_str())
            .and_then(Yaml::as_mapping_mut)
            .ok_or_else(|| {
                CoreError::Validation(format!("release has no service {} to pin", image.service))
            })?;
        service.remove("build");
        service.insert("image".into(), Yaml::String(image.tag.clone()));
        service.insert("pull_policy".into(), "never".into());
    }
    let body = serde_yaml::to_string(&doc)
        .map_err(|e| CoreError::External(format!("rendering rollback Compose: {e}")))?;
    Ok(format!(
        "# Generated by scontrol for a rollback; do not edit.\n{body}"
    ))
}

/// Marks the target release as the one running, so pruning keeps it.
pub(crate) async fn mark_current(root: &Path, deployment_id: Uuid) -> CoreResult<()> {
    let dir = release_dir(root, deployment_id);
    let body = tokio::fs::read_to_string(dir.join(RELEASE_FILE))
        .await
        .map_err(|e| CoreError::Io(format!("reading release record: {e}")))?;
    let record: Release = serde_json::from_str(&body)
        .map_err(|e| CoreError::Validation(format!("unreadable release record: {e}")))?;
    write_record(&dir, &record).await
}

/// Keeps the `keep` most recently started releases (always `current`) and
/// removes the rest with their tags. Returns the kept IDs, newest first.
pub(crate) async fn prune(root: &Path, slug: &str, current: Uuid, keep: usize) -> Vec<Uuid> {
    let mut found = Vec::new();
    let mut entries = match tokio::fs::read_dir(releases_dir(root)).await {
        Ok(entries) => entries,
        Err(_) => return Vec::new(),
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let Some(id) = entry
            .file_name()
            .to_str()
            .and_then(|name| Uuid::parse_str(name).ok())
        else {
            continue;
        };
        let started = tokio::fs::metadata(entry.path().join(RELEASE_FILE))
            .await
            .and_then(|m| m.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        found.push((id, started));
    }
    let (kept, removed) = select_retained(found, current, keep);
    for id in removed {
        let dir = release_dir(root, id);
        if let Ok(body) = tokio::fs::read_to_string(dir.join(RELEASE_FILE)).await {
            if let Ok(record) = serde_json::from_str::<Release>(&body) {
                // Only tags of this release; never a name a record could
                // smuggle in.
                let own: Vec<PinnedImage> = record
                    .images
                    .into_iter()
                    .filter(|image| {
                        deployment_image_tag(slug, &image.service, id).as_deref()
                            == Some(image.tag.as_str())
                    })
                    .collect();
                untag(&own).await;
            }
        }
        tokio::fs::remove_dir_all(&dir).await.ok();
    }
    kept
}

/// Removes every release of a project and its tags (project removal).
pub(crate) async fn remove_all(root: &Path, slug: &str) {
    prune(root, slug, Uuid::nil(), 0).await;
}

/// Splits releases into kept (newest first, `current` always first) and
/// removed. `keep` is clamped to `MAX_RELEASES_RETAINED`; `keep == 0`
/// removes everything (only used for project removal).
fn select_retained(
    mut releases: Vec<(Uuid, SystemTime)>,
    current: Uuid,
    keep: usize,
) -> (Vec<Uuid>, Vec<Uuid>) {
    releases.sort_by_key(|&(id, started)| std::cmp::Reverse((id == current, started, id)));
    let keep = keep.min(MAX_RELEASES_RETAINED).min(releases.len());
    let removed = releases.split_off(keep);
    (
        releases.into_iter().map(|(id, _)| id).collect(),
        removed.into_iter().map(|(id, _)| id).collect(),
    )
}

/// The Compose file as a record: environment and build-arg values, inline
/// secret/config content, and `x-` extension fields are replaced by a mask
/// (comments vanish by re-serializing), then every known secret value left
/// anywhere (a password in a `command`) is masked too.
pub(crate) fn sanitize_compose(compose: &Yaml, secrets: &[String]) -> CoreResult<String> {
    let mut doc = compose.clone();
    if let Some(root) = doc.as_mapping_mut() {
        mask_extensions(root);
    }
    if let Some(services) = doc.get_mut("services").and_then(Yaml::as_mapping_mut) {
        for (_, service) in services.iter_mut() {
            let Some(service) = service.as_mapping_mut() else {
                continue;
            };
            mask_extensions(service);
            if let Some(environment) = service.get_mut("environment") {
                mask_values(environment);
            }
            if let Some(args) = service
                .get_mut("build")
                .and_then(Yaml::as_mapping_mut)
                .and_then(|build| build.get_mut("args"))
            {
                mask_values(args);
            }
        }
    }
    for section in ["secrets", "configs"] {
        if let Some(entries) = doc.get_mut(section).and_then(Yaml::as_mapping_mut) {
            for (_, entry) in entries.iter_mut() {
                if let Some(content) = entry.as_mapping_mut().and_then(|e| e.get_mut("content")) {
                    *content = MASK.into();
                }
            }
        }
    }
    let body = serde_yaml::to_string(&doc)
        .map_err(|e| CoreError::External(format!("rendering Compose snapshot: {e}")))?;
    Ok(redact(&body, secrets))
}

fn mask_extensions(map: &mut serde_yaml::Mapping) {
    for (key, value) in map.iter_mut() {
        if key.as_str().is_some_and(|k| k.starts_with("x-")) {
            *value = MASK.into();
        }
    }
}

/// `KEY: value` → `KEY: ********`, `KEY=value` → `KEY=********`. A bare
/// `KEY` (taken from the environment file) stays as it is.
fn mask_values(values: &mut Yaml) {
    match values {
        Yaml::Mapping(map) => {
            for (_, value) in map.iter_mut() {
                if !value.is_null() {
                    *value = MASK.into();
                }
            }
        }
        Yaml::Sequence(items) => {
            for item in items.iter_mut() {
                match item.as_str().and_then(|entry| entry.split_once('=')) {
                    Some((key, _)) => *item = Yaml::String(format!("{key}={MASK}")),
                    None if item.as_str().is_none() => *item = MASK.into(),
                    None => {}
                }
            }
        }
        other => *other = MASK.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn yaml(body: &str) -> Yaml {
        serde_yaml::from_str(body).unwrap()
    }

    fn image(slug: &str, service: &str, id: Uuid) -> PinnedImage {
        PinnedImage {
            service: service.into(),
            tag: deployment_image_tag(slug, service, id).unwrap(),
            image_id: format!("sha256:{}", "ab".repeat(32)),
        }
    }

    #[test]
    fn snapshots_drop_environment_values_and_known_secrets() {
        let compose = yaml(
            r#"
x-common: &common
  API_TOKEN: anchored-token-xx
services:
  web:
    image: shop/web:1
    # comment with hunter2-xx
    environment:
      DB_PASSWORD: literal-password-xx
      FROM_FILE:
      <<: *common
    command: ["serve", "--password", "hunter2-xx"]
  worker:
    build:
      context: .
      args:
        NPM_TOKEN: npm-token-xx
    environment:
      - SECRET=list-secret-xx
      - FROM_ENV_FILE
    x-meta: { owner: hunter2-xx }
configs:
  app:
    content: "password=inline-config-xx"
secrets:
  key:
    file: ./key.pem
"#,
        );
        let body = sanitize_compose(&compose, &["hunter2-xx".to_string()]).unwrap();
        for secret in [
            "anchored-token-xx",
            "literal-password-xx",
            "hunter2-xx",
            "npm-token-xx",
            "list-secret-xx",
            "inline-config-xx",
        ] {
            assert!(!body.contains(secret), "{secret} leaked:\n{body}");
        }
        // Key names and the structure stay readable.
        for kept in [
            "DB_PASSWORD",
            "FROM_FILE",
            "SECRET=********",
            "FROM_ENV_FILE",
            "NPM_TOKEN",
            "shop/web:1",
            "./key.pem",
            "serve",
        ] {
            assert!(body.contains(kept), "{kept} missing:\n{body}");
        }
    }

    #[test]
    fn rollback_compose_pins_images_and_never_builds_or_pulls() {
        let id = Uuid::new_v4();
        let compose = yaml(
            "services:\n  web:\n    build: { context: ., dockerfile: Dockerfile }\n    image: shop-web\n    ports: ['127.0.0.1:40001:80']\n  db:\n    image: postgres:16\n",
        );
        let images = vec![image("shop", "web", id)];
        let body = render_rollback(&compose, &images).unwrap();
        let doc = yaml(&body);
        let web = &doc["services"]["web"];
        assert_eq!(web["image"].as_str(), Some(images[0].tag.as_str()));
        assert_eq!(web["pull_policy"].as_str(), Some("never"));
        assert!(web.get("build").is_none());
        assert_eq!(web["ports"][0].as_str(), Some("127.0.0.1:40001:80"));
        // Unpinned services keep their Compose image.
        assert_eq!(doc["services"]["db"]["image"].as_str(), Some("postgres:16"));

        let missing = vec![image("shop", "api", id)];
        assert!(render_rollback(&compose, &missing).is_err());
    }

    #[test]
    fn retention_keeps_the_newest_and_always_the_current_release() {
        let base = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        let ids: Vec<Uuid> = (0..5).map(|_| Uuid::new_v4()).collect();
        let releases: Vec<(Uuid, SystemTime)> = ids
            .iter()
            .enumerate()
            .map(|(i, id)| (*id, base + Duration::from_secs(i as u64)))
            .collect();
        let (kept, removed) = select_retained(releases.clone(), ids[4], 3);
        assert_eq!(kept, vec![ids[4], ids[3], ids[2]]);
        assert_eq!(removed.len(), 2);
        // A rolled-back-to release is current even when it is the oldest.
        let (kept, removed) = select_retained(releases.clone(), ids[0], 2);
        assert_eq!(kept, vec![ids[0], ids[4]]);
        assert_eq!(removed, vec![ids[3], ids[2], ids[1]]);
        let (kept, removed) = select_retained(releases, Uuid::nil(), 0);
        assert!(kept.is_empty());
        assert_eq!(removed.len(), 5);
    }

    async fn release_root(record: &Release, compose: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("scontrol-release-{}", Uuid::new_v4()));
        let dir = release_dir(&root, record.deployment_id);
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(dir.join(RELEASE_COMPOSE), compose)
            .await
            .unwrap();
        write_record(&dir, record).await.unwrap();
        root
    }

    #[tokio::test]
    async fn rollback_targets_must_match_the_agent_release_record() {
        let id = Uuid::new_v4();
        let compose = "services:\n  web:\n    image: nginx\n";
        let record = Release {
            deployment_id: id,
            compose_path: "docker-compose.yml".into(),
            compose_sha256: sha256_hex(compose.as_bytes()),
            images: vec![image("shop", "web", id)],
        };
        let root = release_root(&record, compose).await;
        let target = RollbackTarget {
            deployment_id: id,
            compose_sha256: record.compose_sha256.clone(),
            images: record.images.clone(),
        };
        assert_eq!(load_release(&root, "shop", &target).await.unwrap(), compose);

        // A snapshot whose hash or images differ from the agent's record.
        let mut forged = target.clone();
        forged.compose_sha256 = sha256_hex(b"other");
        assert!(matches!(
            load_release(&root, "shop", &forged).await,
            Err(CoreError::Validation(_))
        ));
        let mut forged = target.clone();
        forged.images[0].tag = "library/nginx:latest".into();
        assert!(load_release(&root, "shop", &forged).await.is_err());
        // Another project's slug: the tags are not this project's.
        assert!(load_release(&root, "other", &target).await.is_err());
        // Unknown or pruned release.
        let mut unknown = target.clone();
        unknown.deployment_id = Uuid::new_v4();
        assert!(matches!(
            load_release(&root, "shop", &unknown).await,
            Err(CoreError::NotFound(_))
        ));
        // A release copy edited on disk after it was recorded.
        tokio::fs::write(
            release_dir(&root, id).join(RELEASE_COMPOSE),
            "services:\n  web:\n    image: evil\n",
        )
        .await
        .unwrap();
        assert!(load_release(&root, "shop", &target).await.is_err());
        tokio::fs::remove_dir_all(&root).await.ok();
    }

    #[test]
    fn image_ids_are_strict() {
        assert!(is_image_id(&format!("sha256:{}", "0f".repeat(32))));
        for bad in ["", "sha256:", "abc", "sha256:xyz", "--format"] {
            assert!(!is_image_id(bad), "{bad}");
        }
    }
}
