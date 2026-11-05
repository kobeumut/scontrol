# scontrol

**scontrol** is a lightweight, modern, Rust-based server control panel for Ubuntu and Debian, designed as a minimal, secure, and resource-friendly alternative to HestiaCP and CloudPanel.

It provides native multi-runtime site hosting (PHP-FPM, Node.js, Python, static sites, and reverse proxy) and an optional Dokploy-inspired PaaS deployment workflow for Docker and Docker Compose applications. It also features a built-in Model Context Protocol (MCP) server for AI coding assistants and automated Git configuration tracking with GitHub pull requests.

---

## Table of Contents

- [Key Features](#key-features)
- [System Architecture](#system-architecture)
- [Prerequisites](#prerequisites)
- [Production Installation](#production-installation)
  - [1. Automated Installer](#1-automated-installer)
  - [2. Post-Install Bootstrap (First Admin)](#2-post-install-bootstrap-first-admin)
  - [3. Systemd Services](#3-systemd-services)
- [Docker Setup & Deployment](#docker-setup--deployment)
  - [1. Using Docker for Local Development](#1-using-docker-for-local-development)
  - [2. Docker Support in scontrol (PaaS Application Deployments)](#2-docker-support-in-scontrol-paas-application-deployments)
- [Local Development Guide](#local-development-guide)
  - [1. Environment Setup](#1-environment-setup)
  - [2. Running the Backend (API & Agent)](#2-running-the-backend-api--agent)
  - [3. Running the Frontend Dashboard](#3-running-the-frontend-dashboard)
  - [4. Administration CLI (panel-cli)](#4-administration-cli-panel-cli)
  - [5. Testing, Linting & Type-Checking](#5-testing-linting--type-checking)
  - [6. Updating SQLx Query Metadata](#6-updating-sqlx-query-metadata)
- [Model Context Protocol (MCP) Integration](#model-context-protocol-mcp-integration)
- [Security Model](#security-model)
- [License & Attribution](#license--attribution)

---

## Key Features

- **Multi-Runtime Hosting**: Manage PHP (via PHP-FPM), Node.js, Python, static assets, and reverse proxies out of the box.
- **Dokploy-Style Docker PaaS**: Deploy Git repositories or raw Docker Compose projects with an asynchronous deployment queue, bounded log retention, live log streaming, zero-downtime redeploys, and write-only environment variables.
- **Hardened Security Architecture**:
  - `#![forbid(unsafe_code)]` enforced across every Rust crate.
  - Zero shell invocations: all host commands use typed argument vectors (`Command::new(..).arg(..)`) restricted to an allow-list (`ALLOWED_PROGRAMS`).
  - Separation of privilege: unprivileged API service (`panel-api`) communicates with a root-level daemon (`panel-agent`) strictly over a local Unix domain socket.
  - Industry-standard cryptography: Argon2id password hashing, AES-256-GCM encrypted session cookies, and at-rest envelope encryption for credentials (`SecretBox`).
- **Model Context Protocol (MCP)**: Native stdio and HTTP endpoints allowing AI coding agents to inspect status, manage sites, and trigger deployments.
- **GitOps & Auto-PRs**: Managed host configurations are tracked by systemd path watchers, automatically opening GitHub pull requests for human review.
- **Modular & Opt-In**: Docker, Mail (Postfix/Dovecot/DKIM), and Antivirus (ClamAV) are compile-time Cargo features and opt-in installer selections, keeping the base system minimal and lightweight (~50 MB RAM baseline).

---

## System Architecture

```text
Browser / Client
      │
      ▼
   Nginx (Host Reverse Proxy)
      ├────────► /api/*  ──► panel-api (unprivileged user: scontrol)
      │                           │
      │                           ├─ (Unix Domain Socket RPC: /run/scontrol/agent.sock)
      │                           ▼
      │                      panel-agent (root daemon) ──► Host (systemd, Nginx, Certbot, Docker)
      │                           ▲
      │                           │
      │                      PostgreSQL (panel state & durable deployment queue)
      │
      └────────► /*      ──► Frontend Dashboard (React 19 + TypeScript static build)
```

### Workspace Structure

| Crate / Directory | Path | Purpose |
|---|---|---|
| `panel-api` | `panel/crates/panel-api` | Axum HTTP server, authentication, deployment queue workers, webhook triggers, notifications, and MCP server (`/api/mcp`). Runs unprivileged. |
| `panel-agent` | `panel/crates/panel-agent` | Root daemon executing host operations over Unix socket RPC (Nginx, systemd, PHP-FPM, Docker Compose, Certbot, UFW, Cron). |
| `panel-core` | `panel/crates/panel-core` | Shared domain types, input validators, Unix socket protocol, RPC client, and `AdapterPort` / `MockAdapter`. |
| `panel-cli` | `panel/crates/panel-cli` | Command-line management tool for bootstrap, site operations, and capability discovery. |
| `frontend` | `panel/frontend` | React 19 + TypeScript SPA built with Vite (package: `scontrol-web`). |
| `install/` | `install/` | Production installer (`install.sh`), Nginx site configuration, and auto-PR script. |
| `systemd/` | `systemd/` | Systemd service and path units for `panel-api`, `panel-agent`, and Git configuration sync. |

---

## Prerequisites

### For Production
- **Operating System**: Clean install of Ubuntu 22.04 / 24.04 LTS or Debian 12 (systemd required).
- **Access**: Root permissions (`sudo`).
- **Network**: Ports 80 and 443 open for web traffic, port 22 for SSH.

### For Local Development
- **Rust Toolchain**: Stable 1.80+ (Rust Edition 2021).
- **Node.js & npm**: Node.js v20+ and npm v10+.
- **Database**: PostgreSQL 15+ (can be launched with Docker via `docker compose up -d`).
- **System Libraries**: `pkg-config`, `libssl-dev` (Linux) or `openssl` (macOS).
- **Git** & optional **Docker** engine for containerized workflows.

---

## Production Installation

### 1. Automated Installer

scontrol provides an interactive installer that configures all necessary system dependencies, builds release binaries, sets up PostgreSQL and Nginx, and registers systemd services.

Clone the repository and run the installer as root:

```bash
git clone https://github.com/<owner>/scontrol.git /opt/scontrol
cd /opt/scontrol

# SCONTROL_REPO_URL is used by the Git auto-PR sync service
sudo SCONTROL_REPO_URL="https://github.com/<owner>/scontrol.git" ./install/install.sh
```

During installation, the script will prompt whether to enable optional components:
- **Docker Support**: Docker Engine, Docker Compose v2, ACME webroot for container domains.
- **Mail Support**: Postfix, Dovecot, OpenDKIM.
- **Antivirus Support**: ClamAV daemon.

The installer then:
1. Installs base packages (PostgreSQL, Nginx, Certbot, UFW, Cron, build essentials).
2. Sets up the non-privileged system user and group `scontrol`.
3. Creates required directories (`/etc/scontrol`, `/var/lib/scontrol`, `/run/scontrol`).
4. Generates cryptographic keys (`PANEL_COOKIE_KEY`, `PANEL_SECRET_KEY`) stored with `0640` permissions.
5. Builds release binaries (`panel-agent`, `panel-api`, `panel-cli`) using offline SQLx query caches.
6. Builds and deploys the frontend dashboard to `/var/www/scontrol`.
7. Configures Nginx virtual hosts and reloads the server.
8. Registers and starts systemd units (`panel-agent.service`, `panel-api.service`, `scontrol-git-sync.path`).

### 2. Post-Install Bootstrap (First Admin)

After the installer completes, create your initial administrator account using `panel-cli`:

```bash
sudo -u scontrol panel-cli user create \
  --username admin \
  --email admin@example.com \
  --password "YourStrongPasswordHere" \
  --admin
```

You can now log into the web dashboard by navigating to your server's IP address:
```text
http://<server-ip>/
```
*(The API health endpoint is available at `http://<server-ip>/health/`).*

### 3. Systemd Services

The panel runs as systemd background daemons:

```bash
# Check status of the unprivileged API server
sudo systemctl status panel-api.service

# Check status of the root agent daemon
sudo systemctl status panel-agent.service

# View API logs
sudo journalctl -u panel-api.service -f

# View agent logs
sudo journalctl -u panel-agent.service -f
```

---

## Docker Setup & Deployment

Docker in scontrol serves two distinct roles:
1. **Local Development**: Running either PostgreSQL alone or the frontend, API, and PostgreSQL together without installing development toolchains on your host.
2. **PaaS Application Workflows**: Managing containerized projects and Compose stacks on your production server.

---

### 1. Using Docker for Local Development

#### Full development stack (frontend + API + PostgreSQL)

Use the separate `docker-compose.dev.yml` with Docker Engine / Docker Desktop and Compose v2 or newer. Rust and Node.js run inside unprivileged containers; no host toolchain installation is required. Allow at least 10 GB of free disk space for toolchain images and build/dependency caches. Run these commands from the repository root:

```bash
# Generate persistent development-only keys ONCE (requires openssl).
# Refuse to overwrite an existing file; docker-dev.env is git-ignored.
(umask 077; set -C; printf 'SCONTROL_DEV_COOKIE_KEY=%s\nSCONTROL_DEV_SECRET_KEY=%s\n' \
  "$(openssl rand -base64 32)" "$(openssl rand -base64 32)" > docker-dev.env)

# Build toolchain images and start the separate scontrol-dev project.
docker compose --env-file docker-dev.env -f docker-compose.dev.yml up -d --build

# First launch downloads dependencies and compiles Rust; wait for API startup.
docker compose --env-file docker-dev.env -f docker-compose.dev.yml logs -f api frontend
```

Open **http://localhost:5173**. Vite proxies `/api` to the API container; the API is also reachable at **http://localhost:8080/api/health**. Only loopback ports are published; PostgreSQL is private to this stack. Do not expose this development setup publicly or use production credentials. Use `localhost`, not a LAN IP: authentication cookies require a secure context (browsers generally permit localhost over HTTP).

After the API reports `panel-api listening` (migrations run automatically), create a disposable development administrator:

```bash
docker compose --env-file docker-dev.env -f docker-compose.dev.yml exec api \
  target/debug/panel-cli user create \
  --username devadmin --email dev@example.com \
  --password 'devpassword123' --admin
```

The password is visible in command history/process arguments; use it only for disposable local development. Bootstrap before exposing any access: the first-admin HTTP endpoint is unauthenticated while no users exist.

Source files are bind-mounted read-only. Frontend edits use Vite hot reload; after Rust edits, restart the API to rebuild. Linux dependencies/build artifacts stay in named volumes, separate from host `node_modules` and `target`. Rust debug symbols and incremental compilation are disabled in this stack to reduce disk use.

Verify both the frontend and its API proxy after startup:

```bash
curl --fail http://localhost:5173/ > /dev/null
curl --fail http://localhost:5173/api/health
```

Development lifecycle commands:

```bash
# Rebuild changed Rust code and restart the API.
docker compose --env-file docker-dev.env -f docker-compose.dev.yml restart api

# After frontend dependency/lockfile changes, reinstall via container restart.
docker compose --env-file docker-dev.env -f docker-compose.dev.yml restart frontend

# Stop containers; retain database, keys, and dependency caches.
docker compose --env-file docker-dev.env -f docker-compose.dev.yml down

# DESTRUCTIVE: also delete this stack's database and dependency/build caches.
docker compose --env-file docker-dev.env -f docker-compose.dev.yml down -v
```

Keep `docker-dev.env` across restarts: changing its keys invalidates sessions and makes existing encrypted credentials unreadable. If ports are occupied, add `SCONTROL_DEV_WEB_PORT=15173` and/or `SCONTROL_DEV_API_PORT=18080` to that file and rerun `up -d`. The existing PostgreSQL-only stack is independent and unchanged.

**Host operations are intentionally unavailable by default.** This stack does not run `panel-agent`, mount the Docker socket, or grant privileged mode. UI/API authentication and database-backed development work, but agent capability checks report unreachable and deployments, Nginx, TLS, firewall, and other host operations fail; this is not a mock-agent mode.

For real host operations, keep the root agent on a disposable Ubuntu/Debian host or VM. With native Linux Docker Engine, an explicit local Compose override can bind-mount the host's `/run/scontrol` directory into the API container and add the socket's numeric group ID via `group_add`; both directory traversal and socket permissions must permit access. This grants privileged host RPC access, even with a read-only mount. Docker Desktop on macOS/Windows does not generally support this host Unix-socket arrangement; use a Linux VM or the native development workflow below instead. Never substitute a Docker daemon socket mount.

#### PostgreSQL only (native API/frontend development)

To spin up a local PostgreSQL database configured for scontrol development, use the provided `docker-compose.yml`:

```bash
# Start PostgreSQL on port 55432
docker compose up -d

# Verify the container is running and healthy
docker compose ps
```

Alternatively, run PostgreSQL with a one-line Docker command:

```bash
docker run -d \
  --name scontrol-test-pg \
  -p 55432:5432 \
  -e POSTGRES_USER=scontrol \
  -e POSTGRES_PASSWORD=scontrol \
  -e POSTGRES_DB=scontrol \
  postgres:16-alpine
```

This starts PostgreSQL accessible at:
```text
postgres://scontrol:scontrol@127.0.0.1:55432/scontrol
```
This is the default database connection string used by scontrol's integration test suite.

To stop the development database:
```bash
docker compose down
```

---

### 2. Docker Support in scontrol (PaaS Application Deployments)

Unlike traditional panels that provide Portainer-style raw container managers, scontrol treats Docker as a **declarative PaaS deployment system** inspired by Dokploy:

- **Deployment Model**: Manage applications via Git repositories or raw Docker Compose files. Deployments are executed asynchronously through a durable PostgreSQL queue (`compose_deployments` table).
- **Security Sandboxing**:
  - Compose files are strictly validated before execution.
  - Privileged containers (`privileged: true`), host namespace sharing (`network_mode: host`, `pid: host`), devices, and arbitrary host bind mounts are prohibited.
  - Workloads are contained within `/var/lib/scontrol/docker/<project-slug>/`.
  - Environment variables are write-only, stored with `0600` permissions outside the source tree, and masked in deployment logs.
- **Domain & SSL Automation**:
  - Each Compose project can bind a container port to a public domain.
  - `panel-agent` generates a loopback override (`127.0.0.1:<40000-49999>`) and provisions an atomic Nginx reverse proxy configuration (`scontrol-compose-<domain>.conf`) with WebSocket support.
  - Automated SSL issuance uses `certbot certonly --webroot` against `/var/lib/scontrol/acme`.
- **Private Git Support**:
  - Private repositories are supported using SSH deploy keys or HTTPS tokens.
  - The private key is generated on the host agent and never stored in the database.
  - Tokens and keys are injected through the agent binary acting as a custom `GIT_SSH` / `GIT_ASKPASS` helper—preventing credential exposure on process argv or shell commands.

#### Enabling Docker on the Host
If you opted out during the initial install and wish to enable Docker later:

1. Install Docker Engine and the Compose v2 CLI plugin:
   ```bash
   sudo apt-get install -y docker.io docker-compose-v2 openssh-client
   ```
2. Enable Docker runtime support in `/etc/scontrol/agent.toml`:
   ```toml
   [runtime_features]
   docker = true
   ```
3. Rebuild `panel-agent` with the `docker` feature flag:
   ```bash
   cd /opt/scontrol/panel
   SQLX_OFFLINE=true cargo build --release -p panel-agent --features docker
   sudo cp target/release/panel-agent /usr/local/bin/panel-agent
   sudo systemctl restart panel-agent
   ```

---

## Local Development Guide

### 1. Environment Setup

Clone the repository and set up your environment:

```bash
git clone https://github.com/<owner>/scontrol.git
cd scontrol

# Create a local environment file from the template
cp .env.example panel/.env
```

Ensure your PostgreSQL container is running:
```bash
docker compose up -d
```

### 2. Running the Backend (API & Agent)

#### Running `panel-api`
The API compiles with compile-time checked SQLx queries. Because SQLx metadata is committed in `panel/.sqlx`, you can run without connecting to a live DB during compilation, but running the service requires PostgreSQL:

```bash
cd panel

# Generate random 32-byte base64 keys if not using .env
export PANEL_COOKIE_KEY="$(openssl rand -base64 32)"
export PANEL_SECRET_KEY="$(openssl rand -base64 32)"
export DATABASE_URL="postgres://scontrol:scontrol@127.0.0.1:55432/scontrol"
export PANEL_API_BIND="127.0.0.1:8080"
export PANEL_AGENT_SOCKET="/tmp/scontrol-agent.sock"

# Run the API (automatically runs database migrations on startup)
cargo run --bin panel-api
```

#### Running `panel-agent` (Optional in Development)
`panel-agent` manages host-level resources (systemd, Nginx, Docker) and requires root privileges on Linux. For unit and integration tests, `MockAdapter` is used instead, so running `panel-agent` locally is not required for backend API or frontend development.

If testing host operations on Linux:
```bash
sudo cargo run --bin panel-agent -- --config /etc/scontrol/agent.toml
```

### 3. Running the Frontend Dashboard

The frontend is a React 19 + TypeScript application built using Vite.

```bash
cd panel/frontend

# Install dependencies
npm install

# Start development server
npm run dev
```

The Vite dev server will start at `http://localhost:5173`. Any requests to `/api` are automatically proxied to the backend running at `http://127.0.0.1:8080`.

To build the static distribution:
```bash
npm run build
```

### 4. Administration CLI (`panel-cli`)

`panel-cli` interacts directly with PostgreSQL and the agent socket. You can use it during development:

```bash
cd panel

# List existing users
DATABASE_URL="postgres://scontrol:scontrol@127.0.0.1:55432/scontrol" \
  cargo run --bin panel-cli -- user list

# Create a development admin user
DATABASE_URL="postgres://scontrol:scontrol@127.0.0.1:55432/scontrol" \
  cargo run --bin panel-cli -- user create \
    --username devadmin \
    --email dev@example.com \
    --password "devpassword123" \
    --admin
```

### 5. Testing, Linting & Type-Checking

Run the test suite against the local development database:

```bash
cd panel

# Run all workspace unit and integration tests
DATABASE_URL="postgres://scontrol:scontrol@127.0.0.1:55432/scontrol" cargo test --workspace

# Run linter with strict warning denials
SQLX_OFFLINE=true cargo clippy --workspace --all-targets -- -D warnings

# Check optional feature builds (Docker, Mail, ClamAV)
cargo check -p panel-agent --features docker,mail,antivirus

# Check frontend TypeScript types
cd frontend && npm run build
```

### 6. Updating SQLx Query Metadata

`scontrol` uses SQLx compile-time query verification with committed offline metadata in `panel/.sqlx`. When you add or modify any SQL queries in `panel-api`:

1. Ensure your local PostgreSQL is running: `docker compose up -d`
2. Prepare the offline metadata:
   ```bash
   cd panel
   DATABASE_URL="postgres://scontrol:scontrol@127.0.0.1:55432/scontrol" cargo sqlx prepare --workspace
   ```
3. Commit the updated JSON files under `panel/.sqlx`.

---

## Model Context Protocol (MCP) Integration

scontrol exposes a Model Context Protocol (MCP) server that enables AI tools and agents (such as Claude Code or custom agents) to inspect and manage server resources.

### 1. Stdio Mode (Command Line & Local Agent Tools)
Run `panel-api` directly in stdio mode to communicate via JSON-RPC 2.0 over standard input/output:

```bash
cd panel
cargo run --bin panel-api -- --mcp-stdio
```
In stdio mode, standard output is kept clean for JSON-RPC messages; all logging goes to stderr.

### 2. HTTP Mode
When running the HTTP API daemon, the MCP endpoint is available at:
```text
POST /api/mcp
```
Requests require authentication using a valid session cookie or token.

Supported tools include:
- `sites_list`, `site_create`, `site_delete`
- `compose_projects_list`, `compose_project_create`, `compose_project_deploy`, `compose_project_redeploy`
- `compose_project_get_log`, `compose_project_services`
- `system_capabilities`

---

## Security Model

1. **No Shell Executions**: Commands are never evaluated through `sh -c`, `bash`, or string formatting. Every process execution is constructed via typed arguments and validated against a strict program whitelist.
2. **Strict File Permissions**:
   - Environment and secrets: `0600` / `0640` permissions.
   - Unix socket: restricted to root and group `scontrol` (`0660`).
3. **Data Protection at Rest**: Sensitive integration credentials (notification webhooks, private tokens) are sealed using AES-256-GCM authenticated encryption (`SecretBox`) before database storage.
4. **Isolated Queue Execution**: Compose deployments run asynchronously outside the HTTP request lifecycle with per-project serialized execution and strict runtime timeouts.

---

## License & Attribution

### English Summary
Copyright (c) 2026 **Umut ADALI (kobeumut)**.

scontrol is licensed under the **scontrol Attribution License 1.0**. Full legal terms are in the [LICENSE](LICENSE) file.

- **Commercial & Non-Commercial Use**: You are free to use, modify, redistribute, and sell copies or host commercial services based on scontrol.
- **Required Attribution**: If you include scontrol or substantial portions of it in a commercial product or hosted service, you must display the following attribution in the user-accessible About, Credits, Legal, or Licenses section:

  > Based on scontrol by Umut ADALI(kobeumut).

- **Modified Versions**: Any modified or updated releases must preserve the original attribution and include clear notice of changes, author, and date.
- **Hosted Applications**: Hosting independent websites with scontrol does not require attribution on those websites; attribution is only required on the management panel or service itself.

---

### Türkçe Lisans Özeti

Telif Hakkı (c) 2026 **Umut ADALI (kobeumut)**.

Proje, özel **scontrol Attribution License 1.0** ile lisanslanmıştır. Bağlayıcı koşulların tamamı İngilizce [LICENSE](LICENSE) dosyasındadır:

- Ticari kullanım, değiştirme ve dağıtım serbesttir; lisans ve telif bildirimleri korunmalıdır.
- Ticari ürün/hizmetin kullanıcıların erişebildiği Hakkında, Katkılar, Yasal veya Lisanslar bölümünde şu atıf bulunmalıdır (arayüz yoksa kullanıcı belgelerinde):

  > Based on scontrol by Umut ADALI(kobeumut).

- Değiştirilmiş veya güncellenmiş sürümler dağıtılırken ya da kullanıma sunulurken bu atıf korunmalıdır. Sürüm notları veya değişiklik günlüğü; yapılan değişiklikleri, tarihlerini ve sorumlu kişi/kuruluşu belirtmelidir. Özel kaynak kodunun yayımlanması zorunlu değildir.
- Yalnızca scontrol ile barındırılan bağımsız sitelere atıf eklemek gerekmez; scontrol tabanlı yönetim ürününde/hizmetinde atıf yeterlidir.
- Yazılım garantisiz sunulur; yazarın ve katkıda bulunanların sorumluluğu kanunun izin verdiği azami ölçüde reddedilir.
- Bağımlılıklar kendi lisanslarına tabidir. Bu özel lisans, MIT veya Apache-2.0 olarak etiketlenmemelidir.
