//! Private Git access for Compose projects.
//!
//! Two mechanisms, chosen by the remote's transport:
//!
//! - **SSH deploy key.** The agent generates an ed25519 key per project with
//!   `ssh-keygen`; only the public half ever leaves the host. Git reaches
//!   `ssh` through `GIT_SSH` pointing at this binary in helper mode, which
//!   `exec`s `ssh` with a fixed argv. `GIT_SSH_COMMAND` is not used because
//!   Git runs it through `sh -c`.
//! - **HTTPS token.** Stored write-only, like the environment. Git asks for
//!   it through `GIT_ASKPASS`, again this binary in helper mode, which
//!   answers only prompts for the host the token was saved for. The token
//!   is never on argv or in a Git config file.
//!
//! Files in the project root (mode 0700):
//!
//! ```text
//! deploy_key        0600 ed25519 private key
//! deploy_key.pub    public half, added by the operator on the Git host
//! known_hosts       per-project SSH host keys, trusted on first use
//! git-credentials   0600 HTTPS credentials: host, username, token (one per line)
//! git-ephemeral-credentials
//!                   0600 per-deployment credentials (a GitHub App installation
//!                   token), same format; deleted when the deployment ends
//! ```

use super::exec::{exec_replace, run};
use crate::config::AgentConfig;
use panel_core::adapters::{
    ComposeProjectActionParams, GitHttpCredentials, WriteGitCredentialsParams,
};
use panel_core::error::{CoreError, CoreResult};
use panel_core::types::validate::{self, GitTransport};
use std::path::{Path, PathBuf};

const DEPLOY_KEY: &str = "deploy_key";
const KNOWN_HOSTS: &str = "known_hosts";
const CREDENTIALS: &str = "git-credentials";
const EPHEMERAL_CREDENTIALS: &str = "git-ephemeral-credentials";
const MAX_TOKEN_LEN: usize = 4096;

/// Set by the agent on Git's environment; selects the helper this binary
/// acts as when Git runs it (`askpass` or `ssh`).
const HELPER_ENV: &str = "SCONTROL_GIT_HELPER";
const CREDENTIALS_ENV: &str = "SCONTROL_GIT_CREDENTIALS";
const SSH_KEY_ENV: &str = "SCONTROL_GIT_SSH_KEY";
const KNOWN_HOSTS_ENV: &str = "SCONTROL_GIT_KNOWN_HOSTS";

pub async fn generate_deploy_key(
    cfg: &AgentConfig,
    params: ComposeProjectActionParams,
) -> CoreResult<String> {
    let root = checked_root(cfg, &params.project_slug)?;
    create_private_dir(&root).await?;
    let key = root.join(DEPLOY_KEY);
    let staged = root.join("deploy_key.new");
    let staged_pub = root.join("deploy_key.new.pub");
    for stale in [&staged, &staged_pub] {
        remove_if_present(stale).await?;
    }
    let staged_str = staged.to_string_lossy().into_owned();
    let comment = format!("scontrol-{}", params.project_slug);
    run(
        "ssh-keygen",
        &[
            "-q",
            "-t",
            "ed25519",
            "-N",
            "",
            "-C",
            &comment,
            "-f",
            &staged_str,
        ],
    )
    .await?;
    set_mode(&staged, 0o600).await?;
    let public = tokio::fs::read_to_string(&staged_pub)
        .await
        .map_err(|e| CoreError::Io(format!("reading generated public key: {e}")))?;
    tokio::fs::rename(&staged, &key)
        .await
        .map_err(|e| CoreError::Io(format!("installing deploy key: {e}")))?;
    tokio::fs::rename(&staged_pub, root.join("deploy_key.pub"))
        .await
        .map_err(|e| CoreError::Io(format!("installing deploy key: {e}")))?;
    Ok(public.trim().to_string())
}

pub async fn remove_deploy_key(
    cfg: &AgentConfig,
    params: ComposeProjectActionParams,
) -> CoreResult<()> {
    let root = checked_root(cfg, &params.project_slug)?;
    remove_if_present(&root.join(DEPLOY_KEY)).await?;
    remove_if_present(&root.join("deploy_key.pub")).await
}

pub async fn write_credentials(
    cfg: &AgentConfig,
    params: WriteGitCredentialsParams,
) -> CoreResult<()> {
    let root = checked_root(cfg, &params.project_slug)?;
    let path = root.join(CREDENTIALS);
    let Some(credentials) = params.credentials else {
        return remove_if_present(&path).await;
    };
    if !validate::is_valid_git_host(&params.host) {
        return Err(CoreError::Validation("invalid Git host".into()));
    }
    if !validate::is_valid_git_username(&credentials.username) {
        return Err(CoreError::Validation("invalid Git username".into()));
    }
    if !is_valid_token(&credentials.token) {
        return Err(CoreError::Validation(
            "Git token must be 1..=4096 printable characters".into(),
        ));
    }
    create_private_dir(&root).await?;
    let body = format!(
        "{}\n{}\n{}\n",
        params.host.to_ascii_lowercase(),
        credentials.username,
        credentials.token
    );
    write_private(&path, body.as_bytes()).await
}

/// Values a deployment log must mask: the stored HTTPS token, if any.
pub async fn stored_secrets(root: &Path) -> Vec<String> {
    match tokio::fs::read_to_string(root.join(CREDENTIALS)).await {
        Ok(body) => body
            .lines()
            .nth(2)
            .map(str::to_string)
            .into_iter()
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// Environment for Git commands that may reach the project's remote.
pub struct GitAuth {
    env: Vec<(&'static str, String)>,
    /// One line for the deployment log; never contains a secret.
    pub description: String,
}

impl GitAuth {
    pub fn env(&self) -> Vec<(&str, &str)> {
        self.env.iter().map(|(k, v)| (*k, v.as_str())).collect()
    }
}

/// Picks the deploy key or the HTTPS token for `repository_url`, whichever
/// matches its transport, and returns the environment that makes Git use
/// it through this binary's helper mode. `ephemeral` credentials (minted
/// for this deployment) win over a stored token for HTTPS remotes; they are
/// written to a 0600 file that [`clear_ephemeral`] removes afterwards.
pub async fn for_remote(
    root: &Path,
    repository_url: &str,
    ephemeral: Option<&GitHttpCredentials>,
) -> CoreResult<GitAuth> {
    let (transport, host) = validate::git_remote(repository_url)
        .ok_or_else(|| CoreError::Validation("invalid Git repository URL".into()))?;
    let mut env = base_env();
    let description = match transport {
        GitTransport::Ssh => {
            let key = root.join(DEPLOY_KEY);
            let has_key = key.is_file();
            env.push(("GIT_SSH", helper_program()?));
            // Git would otherwise probe the helper to guess its option syntax.
            env.push(("GIT_SSH_VARIANT", "ssh".into()));
            env.push((HELPER_ENV, "ssh".into()));
            env.push((KNOWN_HOSTS_ENV, path_string(&root.join(KNOWN_HOSTS))));
            if has_key {
                env.push((SSH_KEY_ENV, path_string(&key)));
                "using the project deploy key".to_string()
            } else {
                "no deploy key; connecting without an SSH identity".to_string()
            }
        }
        GitTransport::Https if ephemeral.is_some() => {
            let credentials = ephemeral.expect("checked by the guard");
            if !validate::is_valid_git_username(&credentials.username)
                || !is_valid_token(&credentials.token)
            {
                return Err(CoreError::Validation(
                    "invalid per-deployment Git credentials".into(),
                ));
            }
            let path = root.join(EPHEMERAL_CREDENTIALS);
            let body = format!("{host}\n{}\n{}\n", credentials.username, credentials.token);
            write_private(&path, body.as_bytes()).await?;
            env.push(("GIT_ASKPASS", helper_program()?));
            env.push((HELPER_ENV, "askpass".into()));
            env.push((CREDENTIALS_ENV, path_string(&path)));
            format!("using a short-lived GitHub App installation token for {host}")
        }
        GitTransport::Https => {
            let path = root.join(CREDENTIALS);
            let stored_host = tokio::fs::read_to_string(&path)
                .await
                .ok()
                .and_then(|body| body.lines().next().map(str::to_string));
            match stored_host {
                Some(stored) if stored == host => {
                    env.push(("GIT_ASKPASS", helper_program()?));
                    env.push((HELPER_ENV, "askpass".into()));
                    env.push((CREDENTIALS_ENV, path_string(&path)));
                    format!("using the stored HTTPS token for {host}")
                }
                Some(stored) => format!(
                    "the stored HTTPS token is for {stored}, not {host}; cloning without it"
                ),
                None => "no HTTPS token; cloning anonymously".to_string(),
            }
        }
    };
    Ok(GitAuth { env, description })
}

/// Deletes per-deployment credentials written by [`for_remote`].
pub async fn clear_ephemeral(root: &Path) {
    remove_if_present(&root.join(EPHEMERAL_CREDENTIALS))
        .await
        .ok();
}

/// Environment for every Git command: never prompt on a terminal, and
/// never hand credentials to a credential helper configured on the host,
/// which could store the token in plain text under root's home.
fn base_env() -> Vec<(&'static str, String)> {
    vec![
        ("GIT_TERMINAL_PROMPT", "0".into()),
        ("GIT_CONFIG_COUNT", "1".into()),
        ("GIT_CONFIG_KEY_0", "credential.helper".into()),
        ("GIT_CONFIG_VALUE_0", String::new()),
    ]
}

/// This binary, which Git runs back in helper mode.
fn helper_program() -> CoreResult<String> {
    let exe = std::env::current_exe()
        .map_err(|e| CoreError::Io(format!("locating the agent binary: {e}")))?;
    if !exe.is_file() {
        // The binary was replaced by an upgrade; a restart picks it up.
        return Err(CoreError::External(
            "the agent binary changed on disk; restart panel-agent".into(),
        ));
    }
    Ok(path_string(&exe))
}

/// Runs this process as a Git helper when Git started it as one, and
/// returns `None` otherwise. Called before anything writes to stdout,
/// because an askpass helper answers on stdout.
pub fn run_as_git_helper() -> Option<anyhow::Result<()>> {
    let helper = std::env::var(HELPER_ENV).ok()?;
    let args: Vec<String> = std::env::args().skip(1).collect();
    Some(match helper.as_str() {
        "askpass" => askpass(&args),
        "ssh" => ssh(args),
        other => Err(anyhow::anyhow!("unknown Git helper {other:?}")),
    })
}

fn askpass(args: &[String]) -> anyhow::Result<()> {
    let path = std::env::var(CREDENTIALS_ENV)?;
    let body = std::fs::read_to_string(path)?;
    let prompt = args.first().map(String::as_str).unwrap_or_default();
    let answer = askpass_answer(prompt, &body)
        .ok_or_else(|| anyhow::anyhow!("no stored credential answers this prompt"))?;
    println!("{answer}");
    Ok(())
}

/// Answers Git's `Username for 'https://host': ` and
/// `Password for 'https://user@host': ` prompts, but only for the host the
/// credentials were stored for.
fn askpass_answer<'a>(prompt: &str, credentials: &'a str) -> Option<&'a str> {
    let mut lines = credentials.lines();
    let (host, username, token) = (lines.next()?, lines.next()?, lines.next()?);
    let target = prompt.split('\'').nth(1)?.strip_prefix("https://")?;
    let authority = target.split('/').next()?;
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let prompt_host = host_port.split(':').next()?;
    if !prompt_host.eq_ignore_ascii_case(host) {
        return None;
    }
    if prompt.starts_with("Username") {
        Some(username)
    } else if prompt.starts_with("Password") {
        Some(token)
    } else {
        None
    }
}

fn ssh(git_args: Vec<String>) -> anyhow::Result<()> {
    let known_hosts = std::env::var(KNOWN_HOSTS_ENV)?;
    let key = std::env::var(SSH_KEY_ENV).ok();
    let args = ssh_args(key.as_deref(), &known_hosts, git_args);
    Err(exec_replace("ssh", &args).into())
}

/// `ssh` options ahead of Git's own arguments (`[-p port] host command`).
/// `-F /dev/null` ignores root's SSH config, and identities are limited to
/// the project key, so a project can never borrow root's keys or agent.
fn ssh_args(key: Option<&str>, known_hosts: &str, git_args: Vec<String>) -> Vec<String> {
    let mut args: Vec<String> = [
        "-F",
        "/dev/null",
        "-o",
        "BatchMode=yes",
        "-o",
        "IdentitiesOnly=yes",
        "-o",
        "IdentityAgent=none",
        "-o",
        "StrictHostKeyChecking=accept-new",
        "-o",
        "ConnectTimeout=30",
    ]
    .map(String::from)
    .into();
    args.extend(["-o".into(), format!("UserKnownHostsFile={known_hosts}")]);
    match key {
        Some(key) => args.extend(["-i".into(), key.to_string()]),
        None => args.extend(["-o".into(), "IdentityFile=none".into()]),
    }
    args.extend(git_args);
    args
}

fn is_valid_token(token: &str) -> bool {
    !token.is_empty()
        && token.len() <= MAX_TOKEN_LEN
        && !token.chars().any(|c| c.is_control() || c.is_whitespace())
}

fn checked_root(cfg: &AgentConfig, slug: &str) -> CoreResult<PathBuf> {
    if !validate::is_valid_project_slug(slug) {
        return Err(CoreError::Validation(format!(
            "invalid project slug: {slug}"
        )));
    }
    Ok(cfg.docker_projects_root.join(slug))
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

async fn create_private_dir(dir: &Path) -> CoreResult<()> {
    tokio::fs::create_dir_all(dir)
        .await
        .map_err(|e| CoreError::Io(format!("creating project root: {e}")))?;
    set_mode(dir, 0o700).await
}

async fn set_mode(path: &Path, mode: u32) -> CoreResult<()> {
    #[cfg(unix)]
    tokio::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(mode))
        .await
        .map_err(|e| CoreError::Io(format!("protecting {}: {e}", path.display())))?;
    let _ = (path, mode);
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
        .map_err(|e| CoreError::Io(format!("writing Git credentials: {e}")))?;
    tokio::io::AsyncWriteExt::write_all(&mut file, body)
        .await
        .map_err(|e| CoreError::Io(format!("writing Git credentials: {e}")))?;
    set_mode(path, 0o600).await
}

async fn remove_if_present(path: &Path) -> CoreResult<()> {
    match tokio::fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(CoreError::Io(format!("removing {}: {e}", path.display()))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use panel_core::adapters::GitHttpCredentials;

    fn config(root: &Path) -> AgentConfig {
        AgentConfig {
            docker_projects_root: root.to_path_buf(),
            ..Default::default()
        }
    }

    #[test]
    fn askpass_answers_only_for_the_stored_host() {
        let stored = "github.com\nx-access-token\nghp_secret\n";
        assert_eq!(
            askpass_answer("Username for 'https://github.com': ", stored),
            Some("x-access-token")
        );
        assert_eq!(
            askpass_answer("Password for 'https://x-access-token@github.com': ", stored),
            Some("ghp_secret")
        );
        assert_eq!(
            askpass_answer(
                "Password for 'https://GitHub.com:443/org/repo.git': ",
                stored
            ),
            Some("ghp_secret")
        );
        for prompt in [
            "Password for 'https://evil.example': ",
            "Password for 'https://github.com.evil.example': ",
            "Password for 'https://github.com@evil.example': ",
            "Password for 'http://github.com': ",
            "Enter passphrase for key: ",
            "",
        ] {
            assert_eq!(askpass_answer(prompt, stored), None, "{prompt}");
        }
        assert_eq!(
            askpass_answer("Password for 'https://github.com': ", "github.com\n"),
            None
        );
    }

    #[test]
    fn ssh_helper_pins_identity_and_known_hosts_before_git_args() {
        let git = vec![
            "-p".to_string(),
            "2222".into(),
            "git@example.com".into(),
            "git-upload-pack 'org/repo.git'".into(),
        ];
        let args = ssh_args(Some("/p/deploy_key"), "/p/known_hosts", git.clone());
        assert_eq!(&args[args.len() - git.len()..], git.as_slice());
        let joined = args.join(" ");
        for expected in [
            "-F /dev/null",
            "BatchMode=yes",
            "IdentitiesOnly=yes",
            "IdentityAgent=none",
            "StrictHostKeyChecking=accept-new",
            "-o UserKnownHostsFile=/p/known_hosts",
            "-i /p/deploy_key",
        ] {
            assert!(joined.contains(expected), "{expected} missing: {joined}");
        }
        let anonymous = ssh_args(None, "/p/known_hosts", vec![]).join(" ");
        assert!(anonymous.contains("IdentityFile=none"));
        assert!(!anonymous.contains("-i "));
    }

    #[tokio::test]
    async fn credentials_are_private_and_used_only_for_their_host() {
        let dir = std::env::temp_dir().join(format!("scontrol-git-{}", uuid::Uuid::new_v4()));
        let cfg = config(&dir);
        let slug = "private-app";
        let root = dir.join(slug);
        write_credentials(
            &cfg,
            WriteGitCredentialsParams {
                project_slug: slug.into(),
                host: "GitHub.com".into(),
                credentials: Some(GitHttpCredentials {
                    username: "x-access-token".into(),
                    token: "ghp_s3cret".into(),
                }),
            },
        )
        .await
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(root.join(CREDENTIALS))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        assert_eq!(stored_secrets(&root).await, ["ghp_s3cret"]);

        let auth = for_remote(&root, "https://github.com/org/app.git", None)
            .await
            .unwrap();
        let env = auth.env();
        assert!(env.contains(&("GIT_TERMINAL_PROMPT", "0")));
        assert!(env.contains(&("GIT_CONFIG_KEY_0", "credential.helper")));
        assert!(env.iter().any(|(k, _)| *k == "GIT_ASKPASS"));
        assert!(env.contains(&(HELPER_ENV, "askpass")));
        // The token itself is never in Git's environment, only its file.
        assert!(env.iter().all(|(_, v)| !v.contains("ghp_s3cret")));
        assert!(!auth.description.contains("ghp_s3cret"));

        let other = for_remote(&root, "https://gitlab.com/org/app.git", None)
            .await
            .unwrap();
        assert!(other.env().iter().all(|(k, _)| *k != "GIT_ASKPASS"));

        for bad in [
            ("x-access-token", "two words"),
            ("x-access-token", "line\nbreak"),
            ("x-access-token", ""),
            ("-u", "token"),
        ] {
            let result = write_credentials(
                &cfg,
                WriteGitCredentialsParams {
                    project_slug: slug.into(),
                    host: "github.com".into(),
                    credentials: Some(GitHttpCredentials {
                        username: bad.0.into(),
                        token: bad.1.into(),
                    }),
                },
            )
            .await;
            assert!(matches!(result, Err(CoreError::Validation(_))), "{bad:?}");
        }

        write_credentials(
            &cfg,
            WriteGitCredentialsParams {
                project_slug: slug.into(),
                host: "github.com".into(),
                credentials: None,
            },
        )
        .await
        .unwrap();
        assert!(!root.join(CREDENTIALS).exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn ssh_remotes_use_the_helper_and_the_deploy_key_when_present() {
        let dir = std::env::temp_dir().join(format!("scontrol-git-{}", uuid::Uuid::new_v4()));
        let root = dir.join("app");
        std::fs::create_dir_all(&root).unwrap();
        let url = "git@github.com:org/app.git";

        let auth = for_remote(&root, url, None).await.unwrap();
        let env = auth.env();
        assert!(env.contains(&(HELPER_ENV, "ssh")));
        assert!(env.contains(&("GIT_SSH_VARIANT", "ssh")));
        assert!(env.iter().all(|(k, _)| *k != SSH_KEY_ENV));
        assert!(env.iter().all(|(k, _)| *k != "GIT_SSH_COMMAND"));

        std::fs::write(root.join(DEPLOY_KEY), "key").unwrap();
        let auth = for_remote(&root, url, None).await.unwrap();
        let key = root.join(DEPLOY_KEY).to_string_lossy().into_owned();
        assert!(auth.env().contains(&(SSH_KEY_ENV, key.as_str())));
        assert!(for_remote(&root, "file:///etc", None).await.is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn ephemeral_credentials_win_and_are_cleared() {
        let dir = std::env::temp_dir().join(format!("scontrol-git-{}", uuid::Uuid::new_v4()));
        let root = dir.join("app");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join(CREDENTIALS),
            "github.com\nx-access-token\nghp_stored\n",
        )
        .unwrap();
        let minted = GitHttpCredentials {
            username: "x-access-token".into(),
            token: "ghs_minted".into(),
        };
        let auth = for_remote(&root, "https://github.com/org/app.git", Some(&minted))
            .await
            .unwrap();
        let path = root.join(EPHEMERAL_CREDENTIALS);
        let path_str = path.to_string_lossy().into_owned();
        assert!(auth.env().contains(&(CREDENTIALS_ENV, path_str.as_str())));
        assert!(auth.env().iter().all(|(_, v)| !v.contains("ghs_minted")));
        assert!(!auth.description.contains("ghs_minted"));
        let body = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            askpass_answer("Password for 'https://x-access-token@github.com': ", &body),
            Some("ghs_minted")
        );
        assert_eq!(
            askpass_answer("Password for 'https://evil.example': ", &body),
            None
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        clear_ephemeral(&root).await;
        assert!(!path.exists());
        // A hostile token is refused before anything is written.
        let bad = GitHttpCredentials {
            username: "x-access-token".into(),
            token: "two words".into(),
        };
        assert!(
            for_remote(&root, "https://github.com/org/app.git", Some(&bad))
                .await
                .is_err()
        );
        assert!(!path.exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn deploy_key_is_generated_private_and_replaceable() {
        let dir = std::env::temp_dir().join(format!("scontrol-git-{}", uuid::Uuid::new_v4()));
        let cfg = config(&dir);
        let params = || ComposeProjectActionParams {
            project_slug: "keyed-app".into(),
        };
        let first = generate_deploy_key(&cfg, params()).await.unwrap();
        assert!(first.starts_with("ssh-ed25519 "), "{first}");
        assert!(first.ends_with(" scontrol-keyed-app"), "{first}");
        let key = dir.join("keyed-app").join(DEPLOY_KEY);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&key).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let second = generate_deploy_key(&cfg, params()).await.unwrap();
        assert_ne!(first, second, "rotation must produce a new key");
        remove_deploy_key(&cfg, params()).await.unwrap();
        assert!(!key.exists());
        std::fs::remove_dir_all(&dir).ok();
    }
}
