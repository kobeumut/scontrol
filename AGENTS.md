# AGENTS.md

Guidance for AI coding agents working in this repository. `CLAUDE.md` imports this file, so Claude Code follows the same rules as every other tool.

## Project Overview

scontrol is a lightweight, Rust-based server control panel for Ubuntu/Debian, meant as an alternative to HestiaCP and CloudPanel. It manages PHP, Node.js, and Python sites. Docker (application and Compose deployments), Mail, and Antivirus are optional and are selected at install time. The panel exposes an MCP (Model Context Protocol) server so AI tools can call panel operations, and it opens GitHub PRs when managed configuration changes.

## Architecture

```
Browser → Nginx ─┬─ /api/*  → panel-api ──(Unix socket RPC)──► panel-agent (root) → host
                 └─ /*      → frontend (static)                 │
                                    └──► PostgreSQL (panel state) ┘
```

| Component | Path | Role |
|-----------|------|------|
| panel-api | `panel/crates/panel-api` | Axum HTTP API, auth, business logic, MCP server (`/api/mcp`). Runs as the unprivileged `scontrol` user. |
| panel-agent | `panel/crates/panel-agent` | Root-level host operations behind a Unix-socket RPC. Runs as root. |
| panel-core | `panel/crates/panel-core` | Shared types, errors, security helpers, agent RPC protocol and client, `AdapterPort` trait, `MockAdapter`. |
| panel-cli | `panel/crates/panel-cli` | Administration CLI. Talks to PostgreSQL and the agent socket directly, so it works during install. |
| Frontend | `panel/frontend` | React 19 + Vite + TypeScript dashboard (npm package `scontrol-web`). |

Supporting directories: `install/` (installer, auto-PR script, Nginx config), `systemd/` (unit files), `.kilo/plans/` (architecture plan), `TASK.md` (live checklist).

## Key Conventions

### Security
- Build every host command with `Command::new(..).arg(..)`. Never build a command with `format!` or string concatenation, never use `sh -c`, and never pass a user-controlled command string. This turns unescaped-input bugs from remote-code-execution risks into type and validation errors.
- Spawn processes only through `panel-agent/src/adapters/exec.rs`, whose `ALLOWED_PROGRAMS` list excludes every shell. A source-scan test enforces this; add a new tool to the list in the same change that uses it.
- Every adapter takes pre-validated typed arguments. Validate user input with allow-lists before it reaches an adapter: project IDs, service names, hostnames, paths, branches, images, ports.
- panel-agent runs as root, so keep privileged logic there, typed and validated. panel-api stays unprivileged.
- Password hashing is Argon2id. Session cookies are AES-GCM encrypted. TOTP 2FA is part of core.
- Never log or return secret values. Redact configured secrets from logs.
- Never expose Docker's TCP socket.
- `.env` files are git-ignored. Keep real secrets out of commits.

### Rust
- Edition 2021+. Every crate root has `#![forbid(unsafe_code)]`. Keep it.
- `thiserror` for error types, `serde` for serialization, async handlers on Tokio, Axum 0.7 for HTTP.
- Shared dependency versions live in `[workspace.dependencies]` in `panel/Cargo.toml`.
- Database access is PostgreSQL through SQLx with compile-time checked queries. Migrations live in `panel/crates/panel-api/migrations/`.

### Adapters
Each host operation is a method on `AdapterPort` (`panel/crates/panel-core/src/adapters.rs`). The API calls it, the agent implements it, and tests use `MockAdapter` (`panel_core::adapters::mock`).

```rust
#[async_trait]
pub trait AdapterPort: Send + Sync {
    async fn create_site(&self, params: CreateSiteParams) -> CoreResult<()>;
    async fn delete_site(&self, params: DeleteSiteParams) -> CoreResult<()>;
    // ...
}
```

Mock adapters let unit tests fail a step on purpose, so rollback paths get exercised.

### Optional Features
Docker, mail, and antivirus are Cargo features of `panel-agent` (`docker`, `mail`, `antivirus`), plus `builders` (Nixpacks, Railpack, Buildpacks; implies `docker`). They are off by default and gated with `#[cfg(feature = "...")]`. An installation that did not opt in must not install their packages or compile their code.

## Docker Deployment (Dokploy-inspired)

Docker is **not** a Portainer-like container dashboard. It is a lightweight PaaS-style deployment workflow inspired by Dokploy's application, Compose, and deployment models.

- The primary unit is a named project/application or Compose project, not an arbitrary `docker run` command.
- Supported sources are Compose from Git, raw Compose, single-service applications (a validated image reference, a Dockerfile in Git, or a Git checkout built by Nixpacks, Railpack, or Cloud Native Buildpacks), and static sites served by the host Nginx. Panel-managed inputs are a validated source/Compose path, a write-only environment file, and a stable project name. Deployments keep status, history, commit SHA, and bounded logs.
- Deploy with typed argv only, for example `docker compose -p <project> -f <file> up -d --remove-orphans`. Never put credentials on argv; pass them through env, 0600 files, or stdin.
- Deployments are asynchronous. The `compose_deployments` table is a durable queue: endpoints insert a `queued` row and return `202`, and the workers in `services/deploy_queue.rs` claim rows with `FOR UPDATE SKIP LOCKED`. One project deploys at a time; separate projects run in parallel. Do not run a deployment inside an HTTP handler.
- Compose files are application-controlled code, but they are still validated (`compose_policy` in `adapters/compose.rs`). Reject privileged mode, host namespaces, devices, dangerous capabilities, and forbidden host bind mounts. Every host path a Compose file names (binds, `env_file`, build context and Dockerfile, secret/config files) must resolve inside the source tree, symlinks included. `include`, cross-file `extends`, `label_file`, and volume `driver_opts` are refused, as is a Compose path that resolves outside the source tree. Write all files under the project's isolated directory (installer default `/var/lib/scontrol/docker/<slug>/`).
- Applications (`ComposeSource::Application`, image or Dockerfile) are rendered by the agent into a generated Compose file (`code/docker-compose.yml` for images, `code/.scontrol-app.yml` in the checkout for Dockerfiles), so they share the Compose deploy engine. Render with `serde_yaml` values, never string templates; the shared validation is `ApplicationSpec::validate` in panel-core.
- Builders (`ApplicationBuild::Builder`) are behind the agent Cargo feature `builders` (implies `docker`) and pinned, checksummed binaries from the installer. They run through `adapters/builders.rs` with typed argv, get the canonicalized context, never receive environment values (their only channel is argv), and write side outputs to the agent-owned `build/` directory, never into the checkout. The image `scontrol-app/<slug>:latest` is used with `pull_policy: never`.
- Static sites (`ComposeSource::Static`) run no container. An optional build runs exec-form commands in `docker run --cap-drop ALL --security-opt no-new-privileges` with the checkout mounted. The publish directory must canonicalize inside the checkout, and `publish_tree` copies only regular files and directories (lstat, no symlinks, no `.git`, mode 0644/0755) into `<static_sites_root>/<slug>/releases/<deployment>/`. The vhost serves that directory with `disable_symlinks on`.
- Services (Compose projects) belong to an environment of a project: `projects → environments → compose_projects`. Variables are layered project → environment → service, and on a shared key the most specific layer wins.
- Environment values are write-only. panel-api stores each layer only sealed with `SecretBox` in `env_variables` (AAD = layer + scope id + key) and returns key names only, with the layer that wins and the layers it overrides. Right before a deployment the queue worker merges the layers (`services/env_layers.rs`) and writes the result to the agent through `write_compose_environment`, never through a queue row, response, audit entry, or log. Overridden inherited values travel as `masked_values` only so the agent can mask them too. The agent keeps the merged set in a 0600 `env` file outside `code/` (overridden values in 0600 `env.masked`) and masks all of them in deployment and container logs.
- A service from before layering has `env_layered = false`: the panel never saw its own variables, so deployments leave its agent `env` file alone (no inheritance) until its variables are saved once through the panel.
- A project domain is an explicit binding to a declared service and container port. The agent publishes that port on `127.0.0.1:<40000–49999>` through a generated `scontrol.override.yml`, writes an Nginx vhost `scontrol-compose-<domain>.conf`, and rolls back if `nginx -t` fails. HTTPS uses `certbot certonly --webroot`. No Traefik.
- Code map:
  - `panel-agent`: `adapters/compose.rs` (including static sites), `adapters/builders.rs`, `adapters/deploy_log.rs`, `adapters/docker_config.rs`, `adapters/db_exposure.rs` (database outside access), `adapters/git_auth.rs` (deploy keys, HTTPS tokens, per-deployment tokens, the `GIT_SSH`/`GIT_ASKPASS` helper mode), and `adapters/nginx.rs` (Compose and static vhosts). `adapters/docker.rs` is legacy container CRUD and is not a product surface.
  - `panel-api`: `services/deploy_queue.rs`, `services/database_services.rs` (credential generation and sealing), `services/env_layers.rs` (sealing, merging, key-only views), `services/github_app.rs`, `services/registries.rs`, `services/webhook.rs` (signatures, watch paths), `services/notifications.rs`, `services/secret_box.rs`, `routes/compose_projects.rs` (including the SSE `logs/stream` and `/:id/variables`), `routes/database_services.rs`, `routes/github_apps.rs`, `routes/projects.rs` (projects, environments, and their variables), `routes/registries.rs`, `routes/webhooks.rs` (public, signature-authenticated), `routes/notifications.rs`, `db/compose_projects.rs`, `db/database_services.rs`, `db/github_apps.rs`, `db/projects.rs`, `db/registries.rs`, `db/notifications.rs`.
  - `panel-core`: `merge_env_layers` and `describe_env_layers` in `types.rs` (pure layering rules).
  - `panel-core`: `database_services.rs` (engine templates, credentials, exposure validation).
  - Frontend: `frontend/src/ComposeProjects.tsx`, `frontend/src/Databases.tsx`, `frontend/src/GitHubApps.tsx`, `frontend/src/Projects.tsx`, `frontend/src/Registries.tsx`, `frontend/src/Variables.tsx` (layered write-only editor), `frontend/src/Notifications.tsx`, and `frontend/src/api.ts`.
  - Migrations: `0002_compose_projects.sql`, `0003_deploy_queue_and_domains.sql`, `0004_compose_webhooks.sql`, `0005_webhook_watch_paths.sql`, `0006_notification_channels.sql`, `0007_compose_git_access.sql`, `0008_projects_environments.sql`, `0009_docker_registries.sql`, `0010_github_apps.sql`, and `0011_database_services.sql`.
- Database services (`ComposeSource::Database`: PostgreSQL, MySQL, MariaDB, MongoDB, Redis) are created only through `/api/database-services`, which allocates a loopback port (`25000–29999`) and generated credentials sealed in `database_services`. The agent renders them from the templates in `panel_core::database_services`: port on `127.0.0.1` only, named volume, credentials from its 0600 `db.env` (Redis: 0400 config as a Compose secret), and a password-free exec health check. Outside access is a host Nginx `stream` server plus UFW rules (`adapters/db_exposure.rs`); never publish a database on `0.0.0.0`, because Docker-published ports bypass UFW. Database services get no domains.
- Live container logs use the streamed agent RPC `follow_compose_logs` (see `panel_core::agent_protocol::StreamFrame`). Mask secrets before filtering, and stop the follower when the client goes away.
- Secrets panel-api must store (notification credentials, layered environment variables, registry credentials, GitHub App private keys, and database service passwords; S3 later) are sealed with `SecretBox` under `PANEL_SECRET_KEY`, are write-only in the API, and are stripped from stored delivery errors.
- GitHub App installation tokens (`services/github_app.rs`) are minted by the queue worker when a job starts, scoped to the project's repository with `contents: read`, and sent only in that job's `DeployComposeProjectParams::git_credentials`. The agent keeps them in a 0600 file for the askpass helper, masks them, and deletes the file when the deployment ends. Never put them in a queue row, project row, audit entry, or log.
- Private Git credentials stay on the agent. Deploy keys are generated there (only the public key leaves the host), and HTTPS tokens are write-only, sent through `write_git_credentials`, and stored 0600. Git gets them through the agent binary acting as `GIT_SSH` or `GIT_ASKPASS` (`SCONTROL_GIT_HELPER`). Never use `GIT_SSH_COMMAND` or a credential helper string, because Git runs those through `sh -c`.
- Push webhooks must be authenticated by a body HMAC or by GitLab's token, never by a URL secret alone. The webhook secret is derived (server key + project ID + nonce) and never stored.
- Cancellation and timeouts stop a step's own process group (`exec::run_streamed`). Never use a host-wide `pkill`.
- Dokploy is conceptual guidance only. Do not copy its code, and do not treat it as a license source. The research basis and parity roadmap are in `PLAN.md`.

## Work Tracking

- `TASK.md` is the live implementation checklist. Update it in the same change set whenever work is completed, reprioritized, or newly discovered.
- The architecture plan is `.kilo/plans/1789208962783-rust-server-panel-plan.md`. Read it together with `TASK.md`.
- `PLAN.md` is the Dokploy-parity roadmap: research basis, parity matrix, performance budgets, and phases D0–D6. Update its status column when a parity item lands.
- `MEMORY.md` records durable decisions and their reasons. Update it when a decision changes.

## Common Commands

The Cargo workspace is in `panel/`. SQLx checks queries at compile time, so a build needs either a live `DATABASE_URL` or `SQLX_OFFLINE=true`. Offline metadata is committed in `panel/.sqlx`.

```bash
cd panel

# Build without a database (uses committed offline metadata)
SQLX_OFFLINE=true cargo build --release --workspace

# Build with compile-time SQL checks against a live database
DATABASE_URL=postgres://user:pass@localhost/scontrol cargo build --workspace

# Test (database-backed tests read DATABASE_URL)
cargo test --workspace

# Lint
SQLX_OFFLINE=true cargo clippy --workspace --all-targets -- -D warnings

# Check optional agent code (not compiled by default)
cargo check -p panel-agent --features docker,builders,mail,antivirus

# Regenerate offline SQLx metadata after changing any SQL query
cargo sqlx prepare --workspace

# Run the API (development)
cargo run --bin panel-api

# Run the agent (requires root)
sudo cargo run --bin panel-agent

# Frontend
cd frontend && npm install && npm run build
```

Before you report work as done, run the build, lint, and test commands that apply, and say which ones you could not run.

## Testing

- Unit tests for every adapter, using `MockAdapter`, including rollback paths.
- Integration tests for the full site provisioning flow.
- Compose validation tests, including rejection of privileged mode and host bind mounts.
- MCP tool-call validation tests.
- Pending: E2E test of the installer on a clean Ubuntu 24.04 VM (tracked in `TASK.md`).

## Frontend

- React 19 + Vite + TypeScript. Styling is plain CSS in `panel/frontend/src/styles.css`. Tailwind is not installed.
- New screens need loading, error, and optional-capability states (for example, Docker not installed).

## Installation and Operations

- `install/install.sh` targets Ubuntu/Debian. It asks explicitly about Docker, Mail, and Antivirus, installs the GitHub CLI, and requires interactive `gh auth login` before automation is enabled.
- `systemd/`: `panel-api` runs as `scontrol`. `panel-agent` runs as root with group `scontrol`. `scontrol-git-sync.path` and `.service` watch managed config and run the auto-PR script as root.
- `install/auto-pr.sh` stages exactly one allow-listed file (`/etc/scontrol/agent.toml`) and rejects config that looks like it contains secrets. Keep both guards when you change what gets committed.
- `install/nginx-scontrol.conf` serves the frontend and proxies `/api/` and `/health/` to the API on `127.0.0.1:8080`.

## Module Structure

```
panel/
├── Cargo.toml              # workspace root (members: crates/*)
├── .sqlx/                  # committed offline SQLx metadata
├── crates/
│   ├── panel-core/         # shared types, errors, security, agent RPC, AdapterPort + MockAdapter
│   ├── panel-agent/        # root host operations; adapters/: nginx, tls, firewall, users, cron,
│   │                       #   php_fpm, systemd_app, database, exec, compose, docker, mail, antivirus
│   ├── panel-api/          # Axum API: auth/, db/, routes/, services/, mcp/, migrations/
│   └── panel-cli/          # administration CLI
└── frontend/               # React 19 + Vite dashboard (package: scontrol-web)
```
