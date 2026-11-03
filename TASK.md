# scontrol implementation checklist

Durum işaretleri: `[x]` tamamlandı, `[-]` devam ediyor, `[ ]` planlandı.

The Dokploy parity roadmap, research basis, and performance budgets are in `PLAN.md`. Phase labels (D0–D6) below refer to it.

## Foundation

- [x] Rust workspace: `panel-core`, `panel-agent`, `panel-api`, `panel-cli`.
- [x] Shared typed domain model, adapter port, mock adapter, agent Unix-socket RPC protocol/client.
- [x] SQLx PostgreSQL schema and committed offline SQLx metadata (`panel/.sqlx`).
- [x] Root agent with typed `Command::new(..).arg(..)` execution boundary.
- [x] API authentication: Argon2id, AES-GCM session cookies, TOTP enrollment/verification.
- [x] API/CLI site, database, cron, firewall, TLS operations and audit records.
- [x] MCP JSON-RPC `initialize`, `tools/list`, `tools/call` over authenticated HTTP and local stdio.
- [x] Initial unit/integration coverage, including provisioning rollback.

## Docker: Dokploy-inspired deployment workflow

Research basis: Dokploy source `d64ebed` (2026-11-02), inspected 2026-11-03. Earlier notes used `853ca33`. Findings and file paths are in `PLAN.md` §1.

### D0: Durable deployment core

- [x] Replace the product direction “Portainer-lite” with application/Compose deployments.
- [x] Define target model: project/app identity, source type, Compose file, environment, source path, deployment lifecycle/logs, service/domain bindings.
- [x] Add Docker deployment types, DB migration, and typed adapter/RPC operations.
- [x] Agent: validate project IDs and Compose paths; materialize Git/raw source; use `docker compose -p <project> ... up -d --remove-orphans`.
- [x] API endpoints/MCP tools: create/deploy/redeploy, stop/remove, inspect services, deployment history and bounded log retrieval.
- [x] Durable PostgreSQL deployment queue (migration `0003`, `services/deploy_queue.rs`).
  - Deploy endpoints return `202`.
  - One project's deployments run in order; different projects run in parallel (`PANEL_DEPLOY_WORKERS`).
  - Queued jobs survive restarts. A `running` row left behind at startup is marked `failed`.
- [x] Cancel a queued deployment (`POST /:id/deployments/:deployment_id/cancel`).
- [x] Per-deployment streamed logs (`adapters/deploy_log.rs`).
  - Output is written line by line, environment values are masked, each log is capped at 5 MiB, and 20 logs are kept per project.
  - Read by byte offset with `GET /:id/deployments/:deployment_id/log?offset=`, which never splits a UTF-8 character.
- [x] Write-only environment via a separate agent RPC (`PUT /:id/environment`). Stored 0600 outside the source tree and kept across redeploys. Queue rows never hold secrets.
- [x] Domain route integration.
  - A loopback-only Compose override publishes the service on `127.0.0.1:<40000–49999>`.
  - A generated Nginx vhost (`scontrol-compose-<domain>.conf`) proxies to it with WebSocket upgrade, using an atomic write and rolling back if `nginx -t` fails.
  - Domains conflict-checked against sites. Stale vhosts removed on redeploy/project removal.
- [x] TLS for domains: `certbot certonly --webroot` from the shared ACME webroot. The TLS vhost is rendered only once a certificate exists. A failed issuance is a warning. The installer adds the webroot and a renewal hook that reloads Nginx.
- [x] Incremental Git updates: shallow `fetch` + `reset --hard` + `clean` on an existing checkout. Commit SHA recorded per deployment. `GIT_TERMINAL_PROMPT=0` set; timeouts on git (10 min), compose (`deploy_timeout_secs`), and certbot (5 min).
- [x] Compose file symlink escape check (canonicalized path must stay inside `code/`).
- [x] MCP tools `deploy_compose_project` and `compose_deployment_log`.
- [x] Installer installs the Compose v2 plugin (`docker-compose-v2` or `docker-compose-plugin`) and fails clearly without it.
- [ ] Live E2E on Ubuntu 24.04 with real Nginx and certbot: domain vhost, HTTP-01 issuance, renewal hook.

### D1: Operator UX and automation

- [x] UI (`frontend/src/ComposeProjects.tsx`, `api.ts`):
  - sign-in screen;
  - project list and create form (Git or raw, write-only env rows);
  - project detail with deploy (pull images), stop, and delete;
  - deployment history with cancel;
  - live log viewer that polls by offset and stops once the log is complete;
  - service status, domain editor, write-only environment replace, and the webhook panel (secret shown once).
- [x] Structured service status: `docker compose ps --format json` (one object per line or an array) with container, status, health, exit code and `127.0.0.1:40000->80/tcp`-style ports.
- [x] Cancel a running deployment (`POST .../cancel` answers `202`).
  - The agent keeps a cancel signal per deployment and runs each step in its own process group.
  - Cancelling sends SIGINT to that group only, then SIGKILL after 10 s. Timeouts use the same path.
  - The worker records the `cancelled` status.
- [x] Git push webhooks (migration `0004`, `services/webhook.rs`, `routes/webhooks.rs`).
  - Authentication: HMAC `X-Hub-Signature-256` (GitHub, Gitea/Forgejo), `X-Gitea-Signature`, or GitLab `X-Gitlab-Token`, compared in constant time.
  - The secret is derived from the server key, the project ID and a nonce, so it is never stored. Rotating the nonce revokes it.
  - Ping is answered. Only pushes to the deployed branch deploy; tags and deletions are ignored.
  - A push while a job is still queued is coalesced into it.
  - Unknown project, disabled webhook and bad signature all return the same `404`.
- [x] Webhook `watchPaths` (migration `0005`, `PUT /:id/webhook`, UI editor).
  - Repository-relative globs: `*` and `?` within a segment, `**` across segments; a directory covers everything below it. Up to 32 patterns; classes, braces, and negation are refused.
  - Changed files come from the push payload's `added`/`removed`/`modified` lists. A payload that cannot prove the push missed the patterns (force push with no commits, GitLab truncation past 20 commits, commits without file lists) deploys.
- [x] Live container logs over SSE (`GET /:id/logs/stream?service=&tail=&search=&timestamps=`).
  - New streamed agent RPC `follow_compose_logs`: an `Ok` response, then `StreamFrame` lines until `End`. The agent runs `docker compose logs --follow` in its own process group and stops it when the client disconnects (EOF or failed write) or after 1 h. At most 16 followers at once; lines are capped at 16 KiB.
  - Environment values are masked first and `search` (case-insensitive substring) is applied second, so a search cannot probe for a secret.
  - UI: service filter, search, timestamps, follow/stop; the browser keeps the last 2,000 lines and does not auto-reconnect (that would replay the tail).
- [x] Notifications (migration `0006`, `services/notifications.rs`, `routes/notifications.rs`, `frontend/src/Notifications.tsx`).
  - Slack, Discord, Telegram, ntfy, Gotify, generic webhook (`X-Scontrol-Signature: sha256=<HMAC>`), and SMTP (STARTTLS/TLS; plain only to a loopback relay).
  - Per-channel event flags: `deploy_succeeded`, `deploy_failed`, `deploy_cancelled`. Sent in the background after each worker job; a test endpoint sends on demand.
  - The one secret per channel is AES-256-GCM sealed (`services/secret_box.rs`) with the channel ID and kind as associated data, and is write-only in the API. Stored errors drop request URLs (Slack/Discord/Telegram carry the secret there) and are redacted against the secret.
  - HTTP uses reqwest with rustls and no redirects; no OpenSSL is linked.
- [x] Tests: no-shell argv and env-leak coverage.
  - `adapters/exec.rs` refuses any program outside an allow-list (no shells or interpreters) at runtime, and a source scan asserts that only `exec.rs` spawns processes and that every call site names a literal, allow-listed program.
  - Pure argv builders for `git clone`/`fetch`, `certbot`, and `docker compose up`, with tests that hostile values stay single arguments and Git positionals follow `--`.
  - API test: environment values sent through create, replace, and deploy never appear in any response, error, audit row, project row, or queue row, and reach the agent only through `write_compose_environment`.
- [x] Dedicated `PANEL_SECRET_KEY` for encrypted-at-rest secrets. The installer generates it for new installs. Without it (older installs), the key falls back to `PANEL_COOKIE_KEY`, and panel-api logs a warning. Webhook secrets still derive from `PANEL_COOKIE_KEY`, so existing webhook secrets are unchanged.
- [ ] Notifications for other events (backups, Docker cleanup, resource thresholds) as those features land (D3/D4).
- [x] Mail adapter no longer passes the mailbox password to `doveadm pw -p` on argv. It hashes in-process with `sha-crypt` (`{SHA512-CRYPT}$6$...`, random salt), and `doveadm` is off the exec allow-list. Found and fixed 2026-11-03.
- [x] Legacy `adapters/docker.rs` passes container env through a 0600 `--env-file` (created with `create_new`, removed after `docker run`) instead of `-e KEY=VALUE`. It is still not a product surface. Fixed 2026-11-03.

### D2: Applications and builds

- [ ] Application model (single service) rendered into a generated Compose project, so a single deploy engine serves both applications and Compose projects.
- [x] Private Git (migration `0007`, `adapters/git_auth.rs`, `GET /:id/git-access`, UI "Git access" panel). Done 2026-11-03.
  - Git URLs: HTTPS without a user part, `ssh://[user@]host[:port]/path`, and `user@host:path`. Hosts and users never start with `-`.
  - SSH deploy key: `POST/DELETE /:id/git-access/deploy-key`. The agent runs `ssh-keygen -t ed25519`; only the public key leaves the host (stored in `compose_projects.deploy_public_key`). SSH remotes only.
  - Git reaches `ssh` through `GIT_SSH` = the agent binary in helper mode, which `exec`s `ssh -F /dev/null -o BatchMode=yes -o IdentitiesOnly=yes -o IdentityAgent=none -o StrictHostKeyChecking=accept-new -o UserKnownHostsFile=<project>/known_hosts -i <project>/deploy_key`. `GIT_SSH_COMMAND` is not used because Git runs it with `sh -c`. Host keys are trusted on first use, per project.
  - HTTPS token: `PUT/DELETE /:id/git-access/token`, or `git_credentials` on create. Write-only; the agent keeps host, username, and token in a 0600 `git-credentials` file. Git asks for it through `GIT_ASKPASS` = the agent binary, which answers only prompts for the remote's host (taken from the project, not the request). The token is never on argv, in Git's environment, in `.git/config`, or in a credential helper (`credential.helper` is reset through `GIT_CONFIG_*`), and it is masked in deployment logs. PostgreSQL stores only `git_token_host`.
  - Removing a never-deployed project now deletes its directory too (environment file, keys), so a reused slug cannot inherit them.
  - The installer adds `openssh-client` (`ssh`, `ssh-keygen`) with Docker support, since `git` is installed without recommends.
- [ ] Docker image source (`docker pull` of a validated reference).
- [ ] GitHub App installation tokens minted in panel-api and passed per deployment.
- [ ] Build types: Dockerfile (core). Nixpacks, Railpack, and buildpacks as opt-in pinned binaries. Static apps served by Nginx from a release directory, with no container.
- [ ] Registries with a per-project `DOCKER_CONFIG`.
- [ ] Projects → environments → services with env layering.
- [ ] AES-GCM encryption at rest for every secret panel-api stores (registry, S3, GitHub App keys). Notification credentials are done; Git deploy keys and tokens stay on the agent instead (see `MEMORY.md`).

### D3: Data

- [ ] Database services (Postgres/MySQL/MariaDB/Mongo/Redis) bound to `127.0.0.1`, with an external port only via an explicit firewall rule.
- [ ] Streaming S3 backups with credentials in env, retention by count, and restore.
- [ ] Volume backups (optional stop, then streamed tar).
- [ ] Scheduled jobs via generated systemd timers, with runs recorded and bounded logs.

### D4: Observability

- [ ] Agent metrics sampler reading `/proc` and cgroup v2 (no `docker stats` spawning), downsampled retention in PostgreSQL, threshold alerts.
- [ ] Request stats from an Nginx JSON access log, read incrementally.
- [ ] Scheduled Docker image/builder cleanup.

### D5 and D6

- [ ] Rollbacks (keep the last N deploy image tags plus a credential-free config snapshot).
- [ ] Preview deployments for pull requests.
- [ ] Vetted template catalogue.
- [ ] Redirects, basic auth, path prefix, and custom certificates.
- [ ] Web terminal (`docker exec` PTY only, admin + audit).
- [ ] Remote agents (mTLS or SSH-forwarded socket), RBAC, and API keys.

### Security fixes found during D1

- [x] Database passwords no longer ride on argv: `CREATE ROLE ... PASSWORD` (psql) and `CREATE USER ... IDENTIFIED BY` (mysql) are sent on stdin via `exec::run_with_stdin`. Found and fixed 2026-11-03.

### Performance

- [-] Benchmark against the `PLAN.md` §4 budgets.
  - First data point (2026-11-03, macOS arm64, release build, 2 deploy workers, after 50 health requests): panel-agent 6.1 MB RSS, panel-api 7.8 MB RSS, idle CPU 0.0%.
  - Still to do: measure on Ubuntu 24.04, and measure enqueue→compose-start latency.

## Installation and operations

- [x] Ubuntu/Debian installer with explicit Docker/Mail/Antivirus prompts and Cargo feature selection.
- [x] Install GitHub CLI and require interactive `gh auth login` before enabling automation.
- [x] systemd API/agent units and non-secret managed-config PR watcher.
- [x] Auto-commit/PR script only stages allow-listed agent config snapshots and rejects secret-looking keys.
- [x] Wire frontend build and Nginx static/API reverse proxy into installer.
- [ ] Clean-VM Ubuntu 24.04 installer E2E test.

## Web frontend

- [x] Vite + React 19 dashboard scaffold with loading/error and optional-capability states.
- [x] Fix list requests that used trailing slashes (`/sites/`, `/compose-projects/`); the API returns 404 for those paths.
- [x] Implement authenticated create/deploy/log workflows (D1).
- [x] SPA health check called `/api/health/agent`, which did not exist (health was only mounted at `/health`). It is now mounted under both.
- [x] Vite dev proxy (`SCONTROL_API`, default `http://127.0.0.1:8080`) for local UI work.
- [ ] Hosted sites screens (create/delete site, TLS) are still read-only.

## Validation

Latest run, 2026-11-03, D2 private Git plus the mail and legacy-container argv fixes:

- [x] `cargo test --workspace` with all agent features against temporary PostgreSQL: 65 tests passed (agent 32, api 24, core 9). Default-feature agent build: 9 tests passed.
  - New tests: Git URL acceptance and rejection (option-looking hosts, credentials in HTTPS URLs, `ext::`, `file://`), askpass answers only for the stored host, ssh helper argv, 0600 credential file and token absent from Git's environment, deploy key generation and rotation with real `ssh-keygen`, in-process SHA512-CRYPT, and an API test that tokens never reach responses, audit rows, or project rows.
- [x] `cargo sqlx prepare --workspace` regenerated offline metadata (migration `0007`).
- [x] `SQLX_OFFLINE=true cargo clippy --workspace --all-targets -- -D warnings`, with default and with all optional agent features. `cargo fmt --all -- --check` clean.
- [x] `SQLX_OFFLINE=true cargo build --release --workspace` with all agent features. Binaries: agent 6.3 MB, api 16.6 MB, cli 6.0 MB.
- [x] Frontend `npm run build`.
- [x] Local E2E (macOS, release agent over its socket, real GitHub):
  - HTTPS: a stored token for `github.com` was offered through the askpass helper (`GIT_TRACE` shows Git running the agent binary directly, no shell) and the clone succeeded; the token appears in neither the deployment log, the agent log, nor `.git/config`;
  - SSH: the generated deploy key reached GitHub through the `GIT_SSH` helper, the host key landed in the project's `known_hosts`, and the unregistered key was refused (`Permission denied (publickey)`);
  - removing both never-deployed projects deleted their directories.
- [ ] Not run: a clone of a real private repository with a registered deploy key or a valid token (no test repository credentials here); the UI panel in a browser.

Previous run, 2026-11-03, D1 completion (watch paths, SSE logs, notifications, argv/env tests):

- [x] `cargo test --workspace` with all agent features against temporary PostgreSQL: 57 tests passed (agent 26, api 23, core 8). Default-feature agent build: 9 tests passed.
- [x] `cargo sqlx prepare --workspace` regenerated offline metadata (migrations `0005`, `0006`).
- [x] `SQLX_OFFLINE=true cargo clippy --workspace --all-targets -- -D warnings`, with default and with all optional agent features. `cargo fmt --all -- --check` clean.
- [x] `SQLX_OFFLINE=true cargo build --release --workspace` with all agent features. Binaries: agent 5.8 MB, api 15.7 MB (was 10.5 MB; reqwest, rustls, and lettre), cli 5.7 MB. Idle RSS was not re-measured.
- [x] Frontend `npm run build`.
- [x] Local E2E (macOS, real Docker, agent + API from this change):
  - live logs: a service printing its env secret every second streamed as `token=********`; a search for the secret returned no lines; `search`, `service`, and `timestamps` applied; the `docker compose logs --follow` process was gone within 1 s of the client disconnecting;
  - notifications: a real deploy reached a local webhook receiver as `deploy_succeeded` through the worker, and the channel recorded `last_sent_at`;
  - the secret values appear in neither agent nor API logs.
- [ ] Not run: database `run_with_stdin` change against real psql/mysql (no local servers); SMTP, Slack, Discord, Telegram, ntfy, and Gotify against real services (only the generic webhook was exercised end to end).

Previous run, 2026-11-03, D1 change set:

- [x] `cargo test --workspace` with all agent features against temporary PostgreSQL: 40 tests passed (agent 21, api 11, core 8).
  - New D1 tests cover ps JSON parsing, cancellation and timeout of a real process group, cancelled status recording, webhook signature/token authentication, push interpretation, secret derivation and rotation, and the agent error prefix not being doubled.
- [x] `cargo sqlx prepare --workspace` regenerated offline query metadata (migrations `0003`, `0004`).
- [x] `SQLX_OFFLINE=true cargo clippy --workspace --all-targets -- -D warnings`, with default and with all optional agent features. `cargo fmt --all` applied.
- [x] `SQLX_OFFLINE=true cargo build --release --workspace` with all optional agent features. Binaries: agent 5.9 MB, api 10.5 MB, cli 6.0 MB.
- [x] Frontend `npm run build` (`tsc --noEmit` + Vite).
- [x] Browser E2E (Chrome, Vite dev server → API → agent → Docker):
  - sign in;
  - create a raw project with a write-only env row;
  - deployment queued and log streamed live;
  - services table shows running, uptime and ports.
- [x] API E2E:
  - webhook: unsigned or wrong-secret requests get `404`, ping `200`, other branch ignored, signed push queued, GitLab token coalesced, old secret rejected after rotate;
  - cancelling a running deployment during an image pull reached `cancelled` in 0.5 s, and the log ends with `==> deployment cancelled`;
  - cancelling a finished deployment returns `409`.
- [x] Earlier D0 E2E (still valid): secret absent from the log, environment kept across redeploy, loopback publish, Nginx rollback, project cleanup.
