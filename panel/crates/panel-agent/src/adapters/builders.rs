//! Opt-in image builders for applications without a Dockerfile (Cargo
//! feature `builders`): Nixpacks, Railpack, and Cloud Native Buildpacks.
//!
//! The binaries are pinned and installed by the installer; this module only
//! runs them, with typed argv through `exec`. The source directory has
//! already been checked to resolve inside the checkout. Anything a builder
//! writes besides the image (Railpack's plan) goes to the agent-owned
//! `build/` directory next to `code/`, never into the checkout, so a
//! symlink committed to the repository cannot redirect a root-owned write.
//!
//! Project environment values are not passed to builds: the builders take
//! build variables only as argv (`--env KEY=VALUE`), which `ps` shows to
//! every local user. The running container still gets them via `env_file`.

use super::deploy_log::DeployLog;
use super::exec::run_streamed;
use crate::config::AgentConfig;
use panel_core::error::{CoreError, CoreResult};
use panel_core::types::BuildTool;
use std::path::Path;
use std::time::Duration;

/// Builders whose program is on the agent's `PATH`.
pub fn available() -> Vec<BuildTool> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let dirs: Vec<_> = std::env::split_paths(&path).collect();
    BuildTool::ALL
        .into_iter()
        .filter(|tool| {
            dirs.iter()
                .any(|dir| is_executable(&dir.join(tool.program())))
        })
        .collect()
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// Builds `source` into the local image `image` with `tool`, streaming the
/// builder's output into the deployment log.
pub async fn build_image(
    cfg: &AgentConfig,
    tool: BuildTool,
    source: &Path,
    build_dir: &Path,
    image: &str,
    log: &mut DeployLog,
) -> CoreResult<()> {
    if !available().contains(&tool) {
        return Err(CoreError::Validation(format!(
            "the {} builder is not installed on this server; re-run the installer and enable application builders",
            tool.as_str()
        )));
    }
    let timeout = Duration::from_secs(cfg.deploy_timeout_secs.max(60));
    let source = source.to_string_lossy().into_owned();
    log.step(&format!("building image {image} with {}", tool.as_str()))
        .await?;
    let code = match tool {
        BuildTool::Nixpacks => {
            let args = nixpacks_args(&source, image);
            run_streamed("nixpacks", &args, &[], log, timeout).await?
        }
        BuildTool::Railpack => {
            create_build_dir(build_dir).await?;
            let plan = build_dir.join("railpack-plan.json");
            let info = build_dir.join("railpack-info.json");
            for stale in [&plan, &info] {
                let _ = tokio::fs::remove_file(stale).await;
            }
            let (plan, info) = (
                plan.to_string_lossy().into_owned(),
                info.to_string_lossy().into_owned(),
            );
            let prepare = railpack_prepare_args(&source, &plan, &info);
            let code = run_streamed("railpack", &prepare, &[], log, timeout).await?;
            if code != 0 {
                return Err(CoreError::External(format!(
                    "railpack prepare failed (exit code {code}); see the deployment log"
                )));
            }
            let output = format!("type=docker,name={image}");
            let syntax = format!("BUILDKIT_SYNTAX={}", cfg.railpack_frontend);
            let build = railpack_build_args(&syntax, &plan, &output, &source);
            run_streamed("docker", &build, &[], log, timeout).await?
        }
        BuildTool::Buildpacks => {
            // `pack` keeps its configuration under PACK_HOME; root's home
            // may be unset under systemd.
            let pack_home = cfg.docker_projects_root.join(".pack-home");
            create_build_dir(&pack_home).await?;
            let pack_home = pack_home.to_string_lossy().into_owned();
            let args = pack_args(image, &source, &cfg.buildpacks_builder);
            run_streamed(
                "pack",
                &args,
                &[("PACK_HOME", pack_home.as_str())],
                log,
                timeout,
            )
            .await?
        }
    };
    if code != 0 {
        return Err(CoreError::External(format!(
            "{} build failed (exit code {code}); see the deployment log",
            tool.as_str()
        )));
    }
    Ok(())
}

async fn create_build_dir(dir: &Path) -> CoreResult<()> {
    tokio::fs::create_dir_all(dir)
        .await
        .map_err(|e| CoreError::Io(format!("creating build directory: {e}")))?;
    tokio::fs::set_permissions(dir, std::os::unix::fs::PermissionsExt::from_mode(0o700))
        .await
        .map_err(|e| CoreError::Io(format!("protecting build directory: {e}")))
}

// Argv builders. Paths are agent-built absolute paths and the image tag is
// derived from a validated slug; each is still one discrete argument.

fn nixpacks_args<'a>(source: &'a str, image: &'a str) -> [&'a str; 5] {
    ["build", "--name", image, "--", source]
}

fn railpack_prepare_args<'a>(source: &'a str, plan: &'a str, info: &'a str) -> [&'a str; 6] {
    ["prepare", "--plan-out", plan, "--info-out", info, source]
}

fn railpack_build_args<'a>(
    syntax: &'a str,
    plan: &'a str,
    output: &'a str,
    source: &'a str,
) -> [&'a str; 9] {
    [
        "buildx",
        "build",
        "--build-arg",
        syntax,
        "-f",
        plan,
        "--output",
        output,
        source,
    ]
}

fn pack_args<'a>(image: &'a str, source: &'a str, builder: &'a str) -> [&'a str; 6] {
    ["build", image, "--path", source, "--builder", builder]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builder_argv_keeps_values_discrete_and_has_no_shell_or_env() {
        let hostile = "/srv/x; rm -rf / $(id) --env=SECRET=1";
        let image = "scontrol-app/shop:latest";

        let nix = nixpacks_args(hostile, image);
        assert_eq!(nix[0], "build");
        assert_eq!(&nix[nix.len() - 2..], &["--", hostile]);
        assert_eq!(
            nix[nix.iter().position(|a| *a == "--name").unwrap() + 1],
            image
        );

        let prepare = railpack_prepare_args(hostile, "/p/build/plan.json", "/p/build/info.json");
        assert_eq!(prepare[0], "prepare");
        assert_eq!(prepare[prepare.len() - 1], hostile);
        assert!(prepare
            .iter()
            .all(|a| !a.starts_with("/srv/x") || *a == hostile));

        let build = railpack_build_args(
            "BUILDKIT_SYNTAX=ghcr.io/railwayapp/railpack-frontend:v0.43.0",
            "/p/build/plan.json",
            "type=docker,name=scontrol-app/shop:latest",
            hostile,
        );
        assert_eq!(&build[..2], &["buildx", "build"]);
        assert_eq!(build[build.len() - 1], hostile);

        let pack = pack_args(image, hostile, "paketobuildpacks/builder-jammy-base");
        assert_eq!(
            pack[pack.iter().position(|a| *a == "--path").unwrap() + 1],
            hostile
        );

        for args in [
            nix.to_vec(),
            prepare.to_vec(),
            build.to_vec(),
            pack.to_vec(),
        ] {
            assert!(!args
                .iter()
                .any(|a| *a == "sh" || *a == "-c" || *a == "bash"));
            // Build variables would be argv-visible; none are ever passed.
            assert!(!args.iter().any(|a| *a == "--env" || *a == "-e"));
        }
    }

    #[test]
    fn every_builder_program_is_allowed_to_spawn() {
        for tool in BuildTool::ALL {
            assert!(
                super::super::exec::is_allowed_program(tool.program()),
                "{}",
                tool.program()
            );
        }
    }
}
