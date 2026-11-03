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
Docker, mail, and antivirus are Cargo features of `panel-agent` (`docker`, `mail`, `antivirus`). They are off by default and gated with `#[cfg(feature = "...")]`. An installation that did not opt in must not install their packages or compile their code.

## Docker Deployment (Dokploy-inspired)

Docker is **not** a Portainer-like container dashboard. It is a lightweight PaaS-style deployment workflow inspired by Dokploy's application, Compose, and deployment models.

- The primary unit is a named project/application or Compose project, not an arbitrary `docker run` command.
- Supported sources are explicit Git or raw Compose. Panel-managed inputs are a validated source/Compose path, a write-only environment file, and a stable project name. Deployments keep status, history, and bounded logs.
- Deploy with typed argv only, for example `docker compose -p <project> -f <file> up -d --remove-orphans`.
- Compose files are application-controlled code, but they are still validated. Reject privileged mode and forbidden host bind mounts. Write all files under the application's isolated directory (installer default `/var/lib/scontrol/docker`).
- Environment values are write-only. Do not persist or return secret values after creation. Store them encrypted or in a dedicated secret store, and redact them from logs.
- A project domain is an explicit binding to a declared HTTP service and port. The agent generates an Nginx reverse-proxy vhost, and TLS goes through the standard certificate workflow. This binding is not implemented yet (see `TASK.md`).
- Code map: `panel-agent` `adapters/docker.rs` and `adapters/compose.rs`; `panel-api` `routes/compose_projects.rs` and `db/compose_projects.rs`; migration `0002_compose_projects.sql`.
- Dokploy is conceptual guidance only. Do not copy its code, and do not treat it as a license source. The research basis is recorded in `TASK.md`.

## Work Tracking

- `TASK.md` is the live implementation checklist. Update it in the same change set whenever work is completed, reprioritized, or newly discovered.
- The architecture plan is `.kilo/plans/1789208962783-rust-server-panel-plan.md`. Read it together with `TASK.md`.
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
cargo check -p panel-agent --features docker,mail,antivirus

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
