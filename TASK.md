# scontrol implementation checklist

Durum işaretleri: `[x]` tamamlandı, `[-]` devam ediyor, `[ ]` planlandı.

The Dokploy parity roadmap, research basis, and performance budgets are in `PLAN.md`. Phase labels (D0–D6) below refer to it.

## Stage 4 integration handoff (2026-11-05)

- [x] Merge GENE-78 / PR #13 as the remote-agent/RBAC foundation.
- [x] Integrate PR #12, then PR #11, preserving both features when resolving conflicts; combine named wildcard certificates with advanced vhost options and cover their rendering in a regression check.
- [ ] GENE-82: fix cross-feature remote routing/RBAC, unprivileged chown, macOS terminal compilation, YAML merge-key isolation, and full TLS-file rollback; validate combined migrations and Ubuntu/remote-host flows. Stage 4 merge acceptance is not production-readiness approval.

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
- [x] Write-only environment via a separate agent RPC (`PUT /:id/environment`). Stored 0600 outside the source tree and kept across redeploys. Queue rows never hold secrets. (Since D2 env layering, panel-api keeps the layers sealed and the worker writes the merged set before each deployment; see D2.)
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
  - Hardening (GENE-85, 2026-11-05): every one-shot agent command (`exec::run`, `run_with_stdin`, `run_allow_nonzero`) now runs in its own process group with a hard limit (10 min by default; `run_allow_nonzero_within` for explicit limits). On timeout the whole group gets SIGTERM, then SIGKILL after 5 s, and a dropped future SIGKILLs the group, so a hung `docker compose logs` cannot leave orphaned `docker`/plugin processes. The one-shot `compose_project_logs` read is limited to 30 s; ClamAV scans to 6 h. With `search`, `tail` now counts matching lines: the history up to an instant `T` is scanned (`--until T`, up to 20,000 lines per container, 30 s) and the last matches per container are sent, then the follower continues at `--since T` without repeating or skipping lines.
  - Environment values are masked first and `search` (case-insensitive substring) is applied second, so a search cannot probe for a secret.
  - UI: service filter, search, timestamps, follow/stop; the browser keeps the last 2,000 lines and does not auto-reconnect (that would replay the tail).
- [x] Notifications (migration `0006`, `services/notifications.rs`, `routes/notifications.rs`, `frontend/src/Notifications.tsx`).
  - Message size limits (2026-11-05): every channel's text is cut on `char` boundaries with a `... (log truncated)` marker so long build errors are never rejected: Telegram 4000 UTF-16 units (API limit 4096), Slack 3000, Discord 2000 characters, ntfy 4000 bytes, Gotify/webhook `message`/email 10 000 characters; titles 256 characters on one line; webhook `error_message` data 4000 characters. Telegram sends plain text (no `parse_mode`), so nothing needs HTML escaping.
  - Slack, Discord, Telegram, ntfy, Gotify, generic webhook (`X-Scontrol-Signature: sha256=<HMAC>`), and SMTP (STARTTLS/TLS; plain only to a loopback relay).
  - Per-channel event flags: `deploy_succeeded`, `deploy_failed`, `deploy_cancelled`. Sent in the background after each worker job; a test endpoint sends on demand.
  - The one secret per channel is AES-256-GCM sealed (`services/secret_box.rs`) with the channel ID and kind as associated data, and is write-only in the API. Stored errors drop request URLs (Slack/Discord/Telegram carry the secret there) and are redacted against the secret.
  - HTTP uses reqwest with rustls and no redirects; no OpenSSL is linked.
- [x] Tests: no-shell argv and env-leak coverage.
  - `adapters/exec.rs` refuses any program outside an allow-list (no shells or interpreters) at runtime, and a source scan asserts that only `exec.rs` spawns processes and that every call site names a literal, allow-listed program.
  - Pure argv builders for `git clone`/`fetch`, `certbot`, and `docker compose up`, with tests that hostile values stay single arguments and Git positionals follow `--`.
  - API test: environment values sent through create, replace, and deploy never appear in any response, error, audit row, project row, or queue row, and reach the agent only through `write_compose_environment`.
- [x] Dedicated `PANEL_SECRET_KEY` for encrypted-at-rest secrets. The installer generates it for new installs. Without it (older installs), the key falls back to `PANEL_COOKIE_KEY`, and panel-api logs a warning. Webhook secrets still derive from `PANEL_COOKIE_KEY`, so existing webhook secrets are unchanged.
- [ ] Notifications for other events (backups, Docker cleanup) as those features land (D3/D4). `job_failed` (scheduled jobs) and `metric_alert`/`metric_resolved` (resource thresholds) are done.
- [x] Mail adapter no longer passes the mailbox password to `doveadm pw -p` on argv. It hashes in-process with `sha-crypt` (`{SHA512-CRYPT}$6$...`, random salt), and `doveadm` is off the exec allow-list. Found and fixed 2026-11-03.
- [x] Legacy `adapters/docker.rs` passes container env through a 0600 `--env-file` (created with `create_new`, removed after `docker run`) instead of `-e KEY=VALUE`. It is still not a product surface. Fixed 2026-11-03.

### D2: Applications and builds

- [x] Application model (single service) rendered into a generated Compose project, so a single deploy engine serves both applications and Compose projects. Done 2026-11-03.
  - `ComposeSource::Application(ApplicationSpec)` (`kind: "application"`), stored in the existing `compose_projects.source` JSONB, so no migration was needed. Queue, logs, write-only environment, domains, webhooks, deploy keys, and HTTPS tokens apply unchanged.
  - Spec: `build` (image or Dockerfile), `service_name` (default `app`), `container_port` (rendered as `expose`), `restart`, `memory_limit_mb` (`mem_limit`), `cpu_limit_millis` (`cpus`), `health_check` (exec-form `CMD` argv, never `CMD-SHELL`), and named `volumes` (no host binds). Validated by `ApplicationSpec::validate` in panel-core, shared by panel-api and the agent.
  - The agent renders the file with `serde_yaml` values (no string templating). The project's 0600 `env` file becomes the container's `env_file`, so environment values reach the container without passing through PostgreSQL, argv, or the rendered file.
  - Replicas are not offered: a domain publishes one loopback port, which only one container can bind.
  - UI: "App: image" and "App: Dockerfile" create modes with application settings; the project header summarises the spec; Git access and webhook panels show for Dockerfile apps.
- [x] Private Git (migration `0007`, `adapters/git_auth.rs`, `GET /:id/git-access`, UI "Git access" panel). Done 2026-11-03.
  - Git URLs: HTTPS without a user part, `ssh://[user@]host[:port]/path`, and `user@host:path`. Hosts and users never start with `-`.
  - SSH deploy key: `POST/DELETE /:id/git-access/deploy-key`. The agent runs `ssh-keygen -t ed25519`; only the public key leaves the host (stored in `compose_projects.deploy_public_key`). SSH remotes only.
  - Git reaches `ssh` through `GIT_SSH` = the agent binary in helper mode, which `exec`s `ssh -F /dev/null -o BatchMode=yes -o IdentitiesOnly=yes -o IdentityAgent=none -o StrictHostKeyChecking=accept-new -o UserKnownHostsFile=<project>/known_hosts -i <project>/deploy_key`. `GIT_SSH_COMMAND` is not used because Git runs it with `sh -c`. Host keys are trusted on first use, per project.
  - HTTPS token: `PUT/DELETE /:id/git-access/token`, or `git_credentials` on create. Write-only; the agent keeps host, username, and token in a 0600 `git-credentials` file. Git asks for it through `GIT_ASKPASS` = the agent binary, which answers only prompts for the remote's host (taken from the project, not the request). The token is never on argv, in Git's environment, in `.git/config`, or in a credential helper (`credential.helper` is reset through `GIT_CONFIG_*`), and it is masked in deployment logs. PostgreSQL stores only `git_token_host`.
  - Removing a never-deployed project now deletes its directory too (environment file, keys), so a reused slug cannot inherit them.
  - The installer adds `openssh-client` (`ssh`, `ssh-keygen`) with Docker support, since `git` is installed without recommends.
- [x] Docker image source. The reference is allow-listed (`validate::is_valid_image_reference`: `[registry[:port]/]path[:tag][@sha256:…]`, lowercase path, nothing option-like) and pulled by `docker compose up` (`--pull always` with "pull images").
- [x] GitHub App installation tokens minted in panel-api and passed per deployment (migration `0008`, `services/github_app.rs`, `routes/github_apps.rs`, UI "GitHub Apps" tab). Done 2026-11-03.
  - `POST/GET/DELETE /api/github-apps`: name, numeric App ID, and the PEM private key (PKCS#1 as GitHub issues it, or PKCS#8). The key is parsed with `ring` before it is stored, sealed with `SecretBox` (AAD `github_app:<id>:private_key`), and never returned. An App still linked to a project cannot be deleted (409 names the projects).
  - `PUT/DELETE /api/compose-projects/:id/git-access/github-app` (or `github_app` on create) links an installation ID. Only `https://github.com/<owner>/<repo>` remotes qualify; `GET /git-access` reports the link.
  - When a job starts, the worker signs a 9-minute RS256 App JWT, calls `POST /app/installations/<id>/access_tokens` (no redirects, 20 s timeout) asking for that one repository with `contents: read`, and sends the token in `DeployComposeProjectParams::git_credentials`. The agent writes it to a 0600 `git-ephemeral-credentials` file for its askpass helper, masks it in the deployment log, and deletes it when the deployment ends. It overrides a stored HTTPS token. It is never in a queue row, project row, audit entry, or `Debug` output. A refused installation fails the job before the agent runs, with GitHub's message (no token or JWT) as the error.
- [-] Build types.
  - [x] Dockerfile (core): Git checkout (same clone/fetch, deploy key, and token path as Compose), `context_path` and `dockerfile_path` relative to the repository root, optional multi-stage `target`. Both must canonicalize inside the checkout (symlink escape check). The generated `.scontrol-app.yml` is written with `create_new` after removing whatever the repository put at that path, so a committed symlink cannot redirect the write.
  - [x] Nixpacks, Railpack, and buildpacks as opt-in pinned binaries. Done 2026-11-03.
    - `ApplicationBuild::Builder { builder, repository_url, branch, context_path }` (`type: "builder"`, `builder: nixpacks | railpack | buildpacks`). The context must canonicalize inside the checkout and the builder gets the resolved path.
    - Agent Cargo feature `builders` (implies `docker`), module `adapters/builders.rs`. Argv: `nixpacks build --name <img> -- <dir>`; `railpack prepare <dir> --plan-out build/railpack-plan.json --info-out build/railpack-info.json` then `docker buildx build --build-arg BUILDKIT_SYNTAX=<pinned frontend> -f <plan> --output type=docker,name=<img> <dir>`; `pack build <img> --path <dir> --builder <builder>` with `PACK_HOME` under the projects root. Railpack's plan is written to the agent-owned `build/` directory, never into the checkout. `nixpacks`, `railpack`, and `pack` are on the exec allow-list.
    - The image is `scontrol-app/<slug>:latest`; the generated Compose file uses it with `pull_policy: never`, and `--pull always` is not passed for builder apps.
    - Project environment values are not passed to builds, because the builders take build variables only on argv. They still reach the container through `env_file`.
    - Installer: asked only with Docker. Downloads Nixpacks 1.41.0, Railpack 0.43.0, and pack 0.40.10 for amd64/arm64, checks each against a SHA-256 pinned in `install.sh`, installs `docker-buildx`, and writes `railpack_frontend = ghcr.io/railwayapp/railpack-frontend:v0.43.0` and `buildpacks_builder` to `agent.toml`.
    - `AgentCapabilities::build_tools` lists builders that are compiled in and on `PATH` (no process spawned); the UI marks missing ones.
  - [x] Static apps served by Nginx from a release directory, with no container. Done 2026-11-03.
    - `ComposeSource::Static { repository_url, branch, publish_path, build?, spa }` (`kind: "static"`). Optional `build { image, commands }`: up to 8 exec-form commands, each run as `docker run --rm --cap-drop ALL --security-opt no-new-privileges --env-file <project env> --mount type=bind,source=<code>,target=/workspace <image> <argv>`. A failed or cancelled step removes the named build container.
    - The publish directory must canonicalize inside the checkout. It is copied into `<static_sites_root>/<slug>/releases/<deployment>/` (default `/srv/scontrol/static`, 0755, so the Nginx worker can traverse it) by `publish_tree`: lstat-based, regular files and directories only, symlinks/devices/FIFOs skipped and listed in the log, `.git` skipped, every opened file re-checked against the lstat inode, files 0644 (setuid/exec bits dropped), at most 100,000 files / 2 GiB / depth 64.
    - Domain vhosts use the same writer, rollback, and certbot path as Compose domains, with `root <release>; disable_symlinks on;`, dotfiles answered with 404, and `try_files $uri $uri/ =404` (or `/index.html` with `spa`). The previous release keeps serving until the new vhosts pass `nginx -t`; three releases are kept.
    - `services` reports one synthetic `static` service with the served release; `stop` and live logs are refused; `remove` deletes the vhosts, releases, and project directory.
    - UI: "Static site" create mode (publish directory, optional build image and commands, SPA), and a domain form without service/port for static sites.
- [x] Registries with a per-project `DOCKER_CONFIG` (migration `0009_docker_registries.sql`). `docker_registries` holds the sealed password; the queue sends it to the agent right before a deployment (`write_docker_config`), which writes a 0600 `<project>/.docker/config.json`, sets `DOCKER_CONFIG` for `docker compose up`, masks it in the log, and removes it afterwards. `docker login` runs only for connectivity tests, password on stdin.
- [x] Projects → environments → services with env layering (migration `0008_projects_environments.sql`).
  - `projects` (unique slug per owner) and `environments` (unique slug per project). Every owner of existing Compose projects gets a Default project with a `production` environment holding them. A new Compose project takes `environment_id` (default: the owner's Default/production, created on first use). Deleting an environment or project that still has services is refused (409).
  - `env_variables`: one sealed row per variable of a project, environment, or service (exactly one scope column set). Values are AES-256-GCM sealed with `SecretBox`, AAD = layer + scope id + key, so a value moved to another scope or key fails to open.
  - Merge rule (`panel_core::types::merge_env_layers`): project → environment → service, the most specific layer wins. The queue worker merges right before `deploy_compose_project` and calls `write_compose_environment` with the result plus the overridden values (`masked_values`, kept 0600 in `env.masked` and masked in deployment and live logs). Up to 256 variables per layer, 768 merged.
  - API: `/api/projects` (list, create, rename, delete), `/api/projects/:id/variables`, `/api/projects/:id/environments` (create, delete), `/api/projects/:id/environments/:eid/variables`, and `/api/compose-projects/:id/variables`. GET returns key names per layer and, per key, the winning layer and the layers it overrides. PUT replaces a layer; an entry without `value` keeps the stored value. `PUT /:id/environment` and `environment` on create/deploy now store the service layer instead of writing to the agent.
  - Services from before layering (`env_layered = false`) keep their agent env file untouched and inherit nothing until their variables are saved once; the UI says so.
  - UI: "Projects & environments" tab (projects, environments, project and environment variables) and a layered variables editor on each Compose project showing inherited and overridden keys.
- [x] AES-GCM encryption at rest for every secret panel-api stores (registry, S3, GitHub App keys). Notification credentials, GitHub App private keys, and registry credentials are done; Git deploy keys and tokens stay on the agent instead (see `MEMORY.md`).
- [ ] Found 2026-11-03, not fixed: the installer creates `/var/lib/scontrol` as `0750 root:scontrol`, so the Nginx worker (`www-data`) cannot traverse it to the ACME webroot `/var/lib/scontrol/acme`. HTTP-01 challenges for Compose domains would then 404. Static releases avoid this by living under `/srv/scontrol/static`; the ACME webroot needs the same treatment (or `o+x` on the data root), verified on Ubuntu 24.04.

### D3: Data

- [x] Database services: PostgreSQL, MySQL, MariaDB, MongoDB, Redis (migration `0011`, `panel_core::database_services`, `routes/database_services.rs`, `frontend/src/Databases.tsx`).
  - A `ComposeSource::Database` the agent renders from a vetted per-engine template (`render_database`): pinned version list, `127.0.0.1:<25000–29999>` port only, named volume `data`, exec-form health check without a password, `no-new-privileges`. Shares the queue, logs, variables, and history with other services.
  - Credentials are generated (OS RNG, 32 alphanumeric characters; MySQL/MariaDB also get a root password), sealed with `SecretBox` in `database_services`, and sent only in that deployment's `DeployComposeProjectParams::database`. The agent writes them to a 0600 `db.env` (the last `env_file`, so user variables cannot replace them) and, for Redis, a 0400 config owned by the container user and mounted as a Compose secret. Never on argv; masked in deployment and container logs.
  - `POST /:id/credentials` shows user, password(s), and URIs (`Cache-Control: no-store`, audited without values). Start (queued `up -d`), stop, restart (new `restart_compose_project` RPC), delete. The data volume survives deletion.
  - Outside access is opt-in (`PUT/DELETE /:id/exposure`): a generated Nginx `stream` server on the public port proxies to the loopback port with `allow`/`deny`, and UFW allows the port only from the listed addresses. Docker never publishes on `0.0.0.0` (that would bypass UFW). The agent checks with `nginx -T` that the stream include is loaded, rolls back on failure, and removes the exposure with the project. The installer adds `libnginx-mod-stream` and the `stream {}` include.
  - Templates smoke-tested with real images (all five healthy and authenticating over loopback). MongoDB 8.0 builds before 8.0.17 refuse to start on Linux 6.19+ kernels, so 8.2 is the default.
- [ ] Database services: live E2E on Ubuntu 24.04 of outside access (stream module include, UFW rules) and a shared network so containers of other services can reach a database by name (today they use the host's loopback port only from host processes).
- [x] Streaming S3 backups with credentials in env, retention by count, and restore (migration `0012_s3_backups.sql`). `backup_destinations` holds the sealed access/secret key pair; `backup_runs` records each run. The agent (`adapters/s3.rs`: in-process SigV4 + multipart over reqwest; `adapters/backup.rs`) spawns `pg_dump`/`mysqldump`/`mongodump` through `exec::spawn_piped`, gzips stdout in-process and uploads parts of 8 MiB, with no dump file on disk. Passwords go through `PGPASSWORD`/`MYSQL_PWD` or a 0600 config file (MongoDB), never argv, and are not stored. A failed dump deletes its partial object. Retention keeps the newest N per database. Restore streams S3 → gunzip → `psql`/`mysql`/`mongorestore`. API runs are asynchronous (202 + polled run row). UI: `Backups.tsx`. Not done: scheduled (systemd-timer) backups.
- [x] Backup series isolation and MySQL events (GENE-87, after Dokploy #4914/#5611). A database backup key is `<prefix>/<engine>-<namespace>-<database>-<timestamp>.<ext>`: the namespace is `svc-<slug>` for a named Compose service or a managed database service found by its loopback port, else `host-<host>-<port>` (`srv-<server id>-host-…` on a remote server). Retention counts only exact series members (`is_backup_of_series`), so `app` never prunes `app-staging-…` (database or volume). `mysqldump` also takes `--events`. Backups from before this change keep their old name and are no longer pruned automatically; delete them by hand once newer backups exist.
- [x] Volume backups (optional `docker compose stop <service>`, `tar -cf -` of `<docker_volumes_root>/<project>_<volume>/_data` streamed through gzip to S3, service started again even on failure) and restore (`tar -xpf -`).
- [x] Volume backup/restore service lifecycle safety (GENE-84): before a service is stopped, an S3 preflight checks the destination (list + zero-byte probe write/delete for backups, exact key lookup for restores), so a bad destination costs no downtime. A drop guard starts the service again if the operation is cancelled or panics, a failed stop is followed by a start attempt, and a failed restart is never swallowed (combined "X failed; additionally, starting service Y again failed" error).
- [x] Scheduled jobs via generated systemd timers, with runs recorded and bounded logs (migration `0013_scheduled_jobs.sql`). Done 2026-11-04.
  - Model: `job_schedules` (Compose project, name, service, argv `command`, schedule kind + expression, time zone, generated `on_calendar`, timeout, enabled, last run) and `job_runs` (trigger, status, times, exit code, error, log size, truncated). At most 32 jobs per project; static sites have none.
  - Schedules (`panel_core::schedule`): five-field cron (names, ranges, steps, lists, `@hourly`…`@yearly`) converted to `OnCalendar=`, or a systemd calendar expression limited to calendar characters. Like Vixie cron, restricting both day fields means either matches, which becomes two `OnCalendar=` lines. The agent checks the zone in `/usr/share/zoneinfo` and every expression with `systemd-analyze calendar` (added to the exec allow-list). Conversions were checked against systemd 255.
  - Commands are argv (1..=64 single-line arguments, ≤16 KiB, no leading `-`); a shell given a command string (`sh -c`, `bash -lc`, `--command`, also behind `env`/`busybox`/other wrappers) is refused in panel-api and again in the agent.
  - Agent (`adapters/scheduled_jobs.rs`, feature `docker`): `scontrol-job-<id>.service` (`Type=oneshot`, `ExecStart=<agent> --config <agent.toml> run-scheduled-job <slug> <id>`, `TimeoutStartSec` = timeout + 60 s) and `.timer` (`AccuracySec=1s`), written 0644 through a temp file and rename; a 0600 `jobs/<id>/job.json` holds the argv. Disabled jobs keep their units with the timer stopped. Removing a job, or its Compose project, stops and removes the units and the job directory.
  - Helper mode runs `docker compose … exec -T -e SCONTROL_JOB=<id> -e SCONTROL_JOB_RUN=<run> <service> <argv…>` in its own process group, streams output masked with the project's environment values into `runs/<run>.log` (5 MiB cap), writes `runs/<run>.json`, keeps 50 runs, and exits non-zero on failure. Killing the exec client does not stop the process inside the container, so on timeout, before each run, and on removal the agent stops container processes whose `/proc/<pid>/environ` carries the job marker (SIGTERM, SIGKILL after 10 s).
  - "Run now" starts the same service (`systemctl start --no-block`), so runs never overlap; `409` while one is active. A record left `running` by a killed helper is closed as failed.
  - panel-api (`services/scheduled_jobs.rs`) polls run records every 10 s (2 s while a run is active, and right after "run now"), upserts them so each finished run is stored and notified once, and prunes to 50 runs per job. Output stays on the agent and is read by offset.
  - API: `GET/POST /api/compose-projects/:id/jobs`, `GET/PUT/DELETE …/jobs/:job_id`, `POST …/run` (202), `GET …/runs`, `GET …/runs/:run_id/log`, and `POST /api/scheduled-jobs/preview` (generated `OnCalendar=`). The database row is rolled back (create) or restored (update) when the agent refuses the units. Audit rows name the program, not its arguments.
  - UI: "Scheduled jobs" on each non-static Compose project: list with last run, run now, pause/resume, edit, delete; editor with quote-aware command splitting and argv preview, cron/calendar presets, time zone, timeout, live `OnCalendar=` preview; run history dialog with the streamed log.
  - Verified: unit tests for conversion, validation, unit rendering (`systemd-analyze verify` passes on Ubuntu 24.04), atomic unit writes, record listing/pruning, marker matching; API tests for validation, rollback, scoping, audit, and once-only `job_failed` delivery; a local run of the helper against Docker (masking, literal arguments, exit codes, timeout). Not yet run on a real Ubuntu host with systemd timers firing (part of the pending installer E2E).

### D4: Observability

- [x] Agent metrics sampler reading `/proc` and cgroup v2 (no `docker stats` spawning), downsampled retention in PostgreSQL, threshold alerts (GENE-73).
  - Agent (`adapters/metrics.rs`): in-process sampler every `metrics_interval_secs` (10–30 s, default 15) on the blocking pool. Host: `/proc/stat`, `meminfo`, `loadavg`, `diskstats` (whole disks from `/sys/block`, no partitions/loop/dm double counting), `self/mounts` + `statvfs` (rustix, df-style percentage). Containers (Docker installs): cgroup v2 `cpu.stat`, `memory.current` − `inactive_file`, `memory.max`, `io.stat` for `docker-<id>.scope` and `docker/<id>`; Compose labels from `config.v2.json`, cached per container. RPC `metrics_snapshot` returns the latest snapshot. Release build: ~0.4 ms per host sample.
  - panel-core `metrics.rs`: pure parsers, rate math, snapshot types, and the `AlertTracker` state machine (consecutive-sample trigger/clear plus hysteresis band).
  - API: migration `0015_metrics.sql` (`server_metrics`, `container_metrics`, `metric_alert_settings`, `metric_alert_state`); `services/metrics.rs` collects every 30 s, stores per Compose service (replicas summed, panel projects only), rolls up 30 s → 5 min → 1 h every 5 min, prunes at 24 h / 7 d / 30 d, and sends `metric_alert` / `metric_resolved` only on edges. Routes `/api/metrics/current`, `/server?range=`, `/projects/:id?range=`, `/alerts`, `PUT /alerts/settings`.
  - UI: "Monitoring" tab with stat tiles, CPU/memory/disk I/O/disk-used charts (crosshair tooltip), per-service charts, mounts table, firing alerts, and the threshold form.
  - Verified: parser tests on synthetic `/proc`/cgroup files, sampler tests (rates, label filtering, vanished containers, fd-leak loop of 2000 samples, real-host budget), state-machine tests (single spike, hovering in the band, alternating edges), PostgreSQL tests for storage, dedup, rollup idempotence, retention, disk/service alerts, stale-state cleanup, and the routes. Local E2E: unprivileged agent + panel-api + UI; the host's real 98 % root disk fired `disk:/` once and kept it firing without repeats. Not yet checked with real Docker containers on cgroup v2 (part of the installer E2E).
- [ ] Container CPU/memory alerts per service beyond memory-of-limit (for example sustained CPU above N cores), if operators ask for them.
- [x] Request stats from an Nginx JSON access log, read incrementally (GENE-74).
  - `install/nginx-scontrol-log.conf` (`log_format scontrol_json escape=json`, http-level `access_log`, no headers/query string) and logrotate. `panel_core::observability` parses lines and aggregates per host and UTC minute (hosts capped at 200 per chunk, odd `Host` values counted as `_other`). The agent's `read_nginx_stats` is stateless: panel-api stores the inode and byte offset (`nginx_log_cursor`) and updates it in the same transaction as the buckets (`request_stats_minute`, 30 days kept). Partial last lines wait for the next poll; truncation or a new inode restarts at 0.
  - `GET /api/observability/requests?hours=&host=` (any signed-in user); UI "Observability" tab with a status-class chart and a per-domain table.
- [x] Scheduled Docker image/builder cleanup (GENE-74).
  - Agent `prune_docker`: fixed argv `docker image prune -a -f --filter until=<n>h` and `docker builder prune -a -f ...`; only the validated age (1–8760 h) varies. Freed bytes are parsed from Docker's "Total reclaimed space" / "Total:" line. Runs and settings live in `docker_cleanup_runs` / `docker_cleanup_settings` (migration `0014`); one run at a time; the scheduler (every 5 min) honours the interval; admin-only API and UI (one-click run, interval, age).
  - Verified: core parser/aggregation tests, agent log-reader tests (offsets, partial lines, rotation, giant line), API tests (accumulation, cursor, cleanup validation, overlap, admin-only). Not yet run against a real Nginx and Docker host (part of the pending installer E2E). Notifications for cleanup results are not added.
- [x] Docker cleanup vs. deployments (GENE-83, Dokploy #5714/#5698). Cleanup and deployments exclude each other per server under the claim lock (409 for a manual run, deferral for a scheduled one); the run is owned by a spawned task with a 30 min limit, and a `running` row older than 1 h no longer blocks deployments and is failed by the next start. `image prune -a` is replaced by dangling prune + per-reference `docker image rm` of unused, old, unpinned tagged images (rollback pins and images of stopped containers kept), every command killed at its timeout, 25 min agent budget.
  - Verified: panel-core selection/parse tests; PostgreSQL tests for both exclusion directions, other servers unaffected, queue flowing after the cleanup, dropped callers, agent timeout, stale rows; route 409. Output formats and image-ID consistency checked read-only against Docker 29.8 with the containerd snapshotter; the removal path itself not yet run on a real host (installer E2E).

### D5 and D6

- [x] Rollbacks: pinned `<slug>/<service>:deploy-<id>` image tags plus a credential-free release snapshot (migration `0016_deployment_rollbacks.sql`). Done 2026-11-04.
  - Agent (`adapters/releases.rs`): after a successful Compose deployment it tags the image of every running service by image ID, keeps a 0600 copy of the deployed Compose file and a `release.json` (SHA-256, pinned images) under `releases/<deployment>/`, and prunes all but `rollback_releases_retained` (default 5, max 20) releases with their tags. A built service without a running container, or a service name that is not a lowercase image path component, leaves the deployment without a release (warning) instead of a partial pin. Project removal drops every tag.
  - Snapshot (`DeploymentSnapshot`, `compose_deployments.snapshot`): Compose file with environment/build-arg values, inline secret/config content, `x-` fields, and comments removed, every known secret masked (again by the worker with every value it sent), the override, environment key names, and the pinned images. Never a value.
  - Rollback: `POST /api/compose-projects/:id/rollback/:deployment_id` (and MCP `rollback_compose_project`) queues a `rollback` deployment with `rollback_of`. The worker writes the current merged environment, skips registry login, GitHub App token, Git, build, and pull, and sends a `RollbackTarget`; the agent checks it against its own record (hash, images, tags of that deployment, unchanged copy, tag still at the recorded image ID), renders `code/.scontrol-rollback.yml` (image → tag, `build` removed, `pull_policy: never`), and runs `up -d --remove-orphans` with current domains. Static sites and database services are refused. `GET …/deployments/:id/snapshot` shows the record; audit `rollback_compose_project`; notifications say "Rollback succeeded/failed".
  - UI: "Rollback to this version" on retained deployments in the history (confirmation lists the pinned images), rollback rows show their target.
  - Verified: agent unit tests (sanitizing, rendering, retention, release/target matching, argv), API tests against PostgreSQL (snapshot storage and scrubbing, rollback request shape, no registry/Git calls, conflict/not-found/bad-request cases, route, audit), and a local Docker check that tagging by image ID plus `pull_policy: never` without `--build` brings the older build back. Not yet run end to end through the agent on Ubuntu (part of the pending E2E).
  - Follow-ups: static-site rollback by re-pointing the vhost at a kept release; a configurable keep count per project.
- [x] Preview deployments for pull requests (migration `0019_preview_deployments.sql`). Done 2026-11-04.
  - Lifecycle (`services/previews.rs`, pure rules in `panel_core::previews`): the push webhook also accepts GitHub/Gitea `pull_request` and GitLab `Merge Request Hook` events (same HMAC/token authentication). Opened/reopened and new commits create `<slug>-pr-<n>` (a Compose project on the PR head branch inheriting the parent's owner, environment, GitHub App link and registry) with the domain `<slug>-pr-<n>.<base domain>` and queue a `preview` deployment, or record the new head and redeploy (coalescing with a queued job); closed/merged marks the preview `closing` and tears it down in the background. Transitions run under a per-project advisory lock. Requests from forks are never previewed. Number, head branch and SHA are allow-list validated before they name anything.
  - Limits: at most `max_active` (default 5, 1..=50) active previews per project; above it the request gets a comment instead of a preview. Every container gets `mem_limit`/`memswap_limit`, `cpus`, `pids_limit: 1024` and the same `deploy.resources.limits` through the generated override (default 512 MiB / 0.5 CPU). Previews are ephemeral: the agent removes their named volumes with them (`.scontrol-ephemeral` marker).
  - TTL: the sweeper (every 5 min) marks previews idle for longer than `ttl_hours` (default 48) closing and retries every unfinished teardown (a running deployment is cancelled first). Disabling previews or deleting the parent removes all of them.
  - Isolation (PR #12 review): a preview's Compose file may not name volumes or networks explicitly, use `external` ones, `volumes_from`, `external_links`, or `container:` namespaces; every service's `ports` is replaced (`!override` / `!reset`) in the override, and the effective `docker compose config` must publish only the managed loopback ports. Teardown never uses `down --volumes`; it removes only volumes labelled with the preview's Compose project and named `<slug>_…`. Queueing happens in the transition's transaction under the parent lock, teardown cancels/inspects jobs under the claim lock, and the claim skips closing previews (regression test `closing_previews_never_start_a_deployment`).
  - TLS: per-host certbot certificates, or an existing wildcard certificate lineage (`ComposeDomainBinding::certificate`, certbot skipped).
  - PR comments: after each preview deployment the link (or "failed"/"cancelled", never an error message) is posted and then edited in place; a "removed" note follows teardown. GitHub projects with a GitHub App mint a `pull_requests: write` token; otherwise a write-only sealed API token per project (GitHub/Gitea/GitLab APIs).
  - API: `GET/PUT /api/compose-projects/:id/previews`, `PUT/DELETE …/previews/comment-token`, `DELETE …/previews/:number`. UI: "Pull request previews" on Git-sourced projects.
  - Verified: core unit tests (state machine, TTL, limits, slugs, validation), agent tests (limit override, named certificates), API tests against PostgreSQL (open → deploy with parent variables, limits and ephemeral flag → synchronize/coalesce → fork refusal → limit → close/teardown → slot reuse; TTL sweep; settings validation and write-only token; comment request shape). Not yet run against real GitHub/GitLab webhooks or a wildcard certificate on Ubuntu (part of the pending E2E).
  - Follow-ups: private repositories over deploy keys or stored HTTPS tokens (those live in the parent's agent directory; previews currently get Git access only through a GitHub App or a public remote); a per-preview environment override layer.
- [x] Remote agents (mTLS or SSH-forwarded socket), RBAC, and API keys (GENE-78, migration `0017_rbac_api_keys_servers.sql`). Done 2026-11-04.
  - PKI (`panel_core::pki`): ECDSA P-384 CA sealed in `pki_material`; one panel client certificate (`clientAuth` only, 30 days, renewed with 10 left); agent server certificates (`serverAuth` only, 90 days) signed from a CSR made by `panel-agent remote-init`, with the panel's host as the only SAN. TLS 1.3 only. The client pins the agent leaf fingerprint (current, plus the previous one for 7 days or until the agent presents the new one).
  - Agent: optional `[remote]` listener in `agent.toml` (`remote.rs`), key must be 0600, certificate files reloaded on change, 64 concurrent connections, 10 s handshake timeout. Same line protocol and dispatch as the Unix socket.
  - Servers (`/api/servers`, `services/agent_pool.rs`, `services/placement.rs`): `local` (nil UUID), `mtls`, and `ssh_socket` (`/run/scontrol/remote/<name>.sock`) transports; health check every 60 s; draining and `max_services`; new services go to the least loaded eligible server unless one is chosen. Compose routes, MCP log tools, and deploy workers use the agent of the service's server.
  - RBAC (`panel_core::rbac`, `auth/mod.rs`, `/api/access`): 15 `resource:action` permissions, built-in roles `admin`, `project-admin`, `developer`, `deployer`, `viewer`, custom roles, bindings at global/project/environment scope. Extractors `Authenticated`, `SessionUser`, `AuthUser`, `AdminUser` (needs `system:admin`), `Require<P>`, `ProjectAccess<P>`, `ServiceAccess<P>`. Projects and Compose services are fully RBAC-scoped; `users.is_admin` stays the superuser. Bindings and roles can only hand out what their author holds; project creators get `project-admin` on the project.
  - API keys (`/api/api-keys`): `sck_<prefix>_<secret>`, Argon2id hash only, token shown once, optional expiry, revocation applies immediately; a key's grants are checked together with its owner's current grants; `access:manage` and `system:admin` cannot be granted to keys; key management needs a session. Audit entries of key requests have `actor_kind = 'api_key'`.
  - UI: Servers (enroll from CSR, certificate bundle, rotate, drain, capacity, check), Access (bindings and roles), API keys (scoped grants, one-time token), and a server picker on new services.
  - Verified: panel-core unit tests (RBAC rules, PKI, mTLS client against a mock TLS agent: round trip, foreign CA, agent-as-client EKU, pin mismatch, wrong host), agent tests (key permissions, reload, `remote-init`), API integration tests against PostgreSQL through the router (no-binding, project, and environment scoping; API key scope, hashing, forgery, revocation; key ⊆ owner; binding/role escalation; placement and per-server routing; CSR enrollment and rotation). Not yet run against a real remote host.
- [x] PR #13 review fixes (2026-11-04): deploying with an `environment` needs `variables:update`; domain changes that redeploy, webhook-secret rotation, creating with `deploy: true`, and scheduled-job create/edit need `services:deploy` (all checked before any change; regression tests for sessions and narrowed keys). Health checks are bounded (10 s) and run concurrently (a silent agent no longer stalls the others). Migration numbers are unique across the open D5/D6 PRs: #13 keeps `0017_rbac_api_keys_servers.sql`, #12 moved to `0019_preview_deployments.sql`, #11 to `0020_domain_vhost_options.sql`; the three were applied together on a fresh database (see the PR).
- [x] Multi-server follow-ups (2026-11-04): scheduled jobs (routes and per-server run collection with per-server cursors), volume backups (the service's server) and database backups (optional `server_id`), database services (placement, lifecycle, outside access on their server), metrics (`server_metrics.server_id`, per-server alert state and labels, `?server_id=` on the API and a server picker in Monitoring), request statistics (one log cursor per server) and Docker cleanup (per server, one run at a time per server). Persistent mTLS connections (idle pool, read-only retry only). Placement default changed to "this host unless draining or full", with `auto_place` for least-loaded placement. Installer agent-only mode (`SCONTROL_ROLE=agent`: builds only panel-agent, runs `remote-init`, writes `[remote]`, UFW rule for the panel address) and the `scontrol-agent-tunnel@` unit for SSH-forwarded servers; the remote listener waits for its certificate instead of needing a restart.
- [x] RBAC follow-ups (2026-11-04): scheduled jobs and database services are scoped per service (`ServiceAccess`; revealing database credentials needs a session and `variables:update`; outside access also needs `servers:manage`); per-service metrics need `services:read` on the service. MCP over HTTP accepts API keys and checks each Compose tool against the caller's grants (sites, databases, and firewall tools stay session-only). Registries, S3 destinations, notifications, GitHub Apps, firewall, host metrics, alert settings, and Docker cleanup stay `system:admin` (server-wide by nature).
- [ ] Remote agents end to end on real hosts (Ubuntu 24.04 panel + agent over mTLS and over the SSH tunnel), as part of the pending installer E2E.
- [x] Vetted template catalogue (GENE-77). `panel-core/src/templates.rs` + `panel-core/templates/*.toml|yml` (WordPress, Ghost, Directus, Plausible, Uptime Kuma). Compose files hold `${VAR}` references only; generators (`password`, `hex`, `base64`, `uuid`) fill a write-only service environment layer at install, and `input` variables are user supplied (a generated secret cannot be overridden). `GET /api/templates`, `/:id`, `POST /:id/install` creates a raw-Compose project. A test runs every shipped template through the agent's `compose_policy`. Not yet: Next.js (needs a Git build source).
- [x] Redirects, basic auth, path prefix, and custom certificates (GENE-77). `panel-core/src/vhost.rs` (`VhostOptions`, strict redirect-target and prefix allow-lists), migration `0020_domain_vhost_options.sql` (numbered clear of the other open D5/D6 branches' 0017), `PUT /api/compose-projects/:id/domains/:domain_id/options`, agent `adapters/vhost_secrets.rs` (htpasswd with SHA-512 crypt hashes, uploaded cert/key, in `nginx_vhost_secrets_dir/<domain>/` = `/etc/nginx/scontrol-vhost`, directory 0711; key/cert 0600 root, htpasswd 0600 owned by the Nginx worker user from `nginx.conf` because workers read it per request; an uploaded pair is parsed, expiry- and key-match-checked with `openssl` in a `*.new` staging area and swapped in only when valid, so a bad upload never touches the active pair) and `adapters/nginx.rs` rendering. Applied through the existing deploy path, so `nginx -t` failure rolls the vhost back. A path prefix is for container services only; a custom certificate needs HTTPS and replaces certbot for that domain.
- [x] Web terminal (GENE-77). `GET /api/compose-projects/:id/terminal` (WebSocket, admin only, same-origin check). Agent `adapters/terminal.rs` allocates a PTY (rustix, no `unsafe`) and runs only `docker exec -it -e TERM=… <container-id> <sh|bash|ash>`; the container is resolved from the Compose project/service labels, so no host shell and no foreign container. Max 8 sessions, 1 MiB buffer, 15 min idle close. Open/close are audited with byte counts (keystrokes are not recorded). The API pumps output through long-polling agent RPCs (`open/read/write/resize/close_terminal`). The installer's Nginx config now upgrades WebSockets on `/api/`.

### Security fixes found during D1
- [x] Compose policy closed host-file reads (found and fixed 2026-11-03, D2). Compose runs as root, so these let a Compose file read or mount any host file:
  - `build` contexts and Dockerfiles that are absolute, contain `..`, use `~` or `$` interpolation, or are remote URLs; `build.additional_contexts`, `build.secrets`, and `build.ssh`;
  - absolute or escaping `env_file` (string, list, or `path:` form), `label_file`, `extends.file`, and top-level `include`;
  - top-level `secrets`/`configs` with an escaping `file:`; volume `driver_opts` (a `local` volume with `o: bind` mounts a host path);
  - short-form binds with `..` anywhere (`./data/../../..` slipped past the old `../` prefix check), `~`, or `$`; long-form mounts other than `volume`/`tmpfs`.
  - Every remaining relative host path (binds, env files, build contexts, Dockerfiles, secret/config files) is resolved after checkout: the deepest existing ancestor must canonicalize inside `code/`, so a symlink committed to the repository cannot point Docker at a host path.
  - Also refused now: `userns_mode: host`, `device_cgroup_rules`, `apparmor:unconfined` (colon form), `systempaths=unconfined`, `label=disable`, and the capabilities `SYS_MODULE`, `SYS_RAWIO`, `SYS_PTRACE`, `SYS_BOOT`, `DAC_READ_SEARCH`, and `BPF` (with or without a `CAP_` prefix, any case).
- [x] `agent.toml` without a `[runtime_features]` section disabled Docker, Mail, and Antivirus, because a derived `Default` ignored the per-field `default_true`. `RuntimeFeatures` now implements `Default` explicitly. Installer-written configs always had the section, so installs were unaffected. Found during the D2 E2E run.

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

Previous run, 2026-11-03, D2 projects, environments, and env layering (Linux x86_64, Rust 1.99.1 in a container, PostgreSQL 16):

- [x] `cargo test --workspace` with all agent features against PostgreSQL: 86 tests (agent 40, api 32, core 14); 85 passed in the container run. `git_auth::deploy_key_is_generated_private_and_replaceable` failed only because the container's uid 1000 has no passwd entry (`ssh-keygen: No user exists for uid 1000`); it passes when run as root. Default-feature agent build: 10 passed.
  - New tests: merge precedence and overridden values, key provenance without values (core); sealed rows hold no plaintext, update keeps omitted values and rejects unknown keys and duplicates, sealed values bound to scope and key (env_layers); the worker writes the merged layers before deploying, with values absent from the queue row and from `Debug` output; legacy services keep their agent env file; API test that project, environment, and service secrets never reach responses, errors, or audit rows, plus 409 on deleting a non-empty environment or project; agent test that overridden values are kept 0600 and masked.
- [x] Migration 0008 checked against a 0007 database holding projects of two owners: each owner got a Default/production project and every service moved into it with `env_layered = false`.
- [x] `cargo sqlx prepare --workspace` (metadata updated), `SQLX_OFFLINE=true cargo clippy --workspace --all-targets -- -D warnings` with default and all agent features, `cargo fmt --all -- --check`, `SQLX_OFFLINE=true cargo build --release --workspace`.
- [x] Frontend `npm run build`.
- [ ] Not run: the new screens in a browser; a real Docker deployment of a layered service.

Latest run, 2026-11-03, D2 GitHub App tokens, builders, and static sites (macOS arm64, Rust 1.98.1, PostgreSQL 16 in Docker):

- [x] `cargo test --workspace` with all agent features (`docker,builders,mail,antivirus`) against PostgreSQL: 94 tests passed (agent 49, api 32, core 13). Default-feature agent: 10 passed; `docker` only: 46 passed.
  - New tests: static site and builder spec validation; builder argv (discrete values, no shell, no `--env`); every builder program is allow-listed; builder apps render `image` + `pull_policy: never`; ephemeral Git credentials win, are 0600, answer only their host, and are deleted; static vhost (release root, `disable_symlinks on`, dotfile 404, SPA fallback, TLS); `publish_tree` skips symlinks to host files and directories, `.git`, and drops setuid bits; a symlinked publish directory is refused; release pruning keeps the served release; static services/stop/logs/remove; static build argv (cap-drop, no-new-privileges, env file by path, exec form); PEM key parsing (PKCS#1 and PKCS#8, errors never echo key material); the App JWT verifies against the App public key and lives ≤ 10 minutes; only github.com HTTPS remotes qualify; token minting against a local mock GitHub API (repository-scoped, `contents: read`, wrong installation → HTTP 404 message, key sealed for another row refused); the queue sends the minted token with the job and no table holds it; API: the private key is never returned or stored in clear, links need github.com HTTPS remotes, linked Apps cannot be deleted, static/builder sources are validated.
- [x] `cargo sqlx prepare --workspace` regenerated offline metadata (migration `0008`).
- [x] `SQLX_OFFLINE=true cargo clippy --workspace --all-targets -- -D warnings`, with default and with all optional agent features; `cargo clippy -p panel-agent` with `docker` and `docker,builders`. `cargo fmt --all -- --check` clean.
- [x] Frontend `npm run build`.
- [x] Local E2E (macOS, release agent with `docker,builders` over its socket, Docker Desktop):
  - static site without build (`mdn/beginner-html-site`): cloned, 16 files published, `.git` not published, `services` reports the release;
  - static site with an `alpine:3.20` build that plants `dist/leak -> /etc/passwd` and `dist/assets/hostroot -> /`, sets setuid on `index.html`, and prints an env secret: both links skipped and logged, `index.html` published 0644, the secret masked as `********` in the deployment log and absent from the agent log, no build container left;
  - a failing build step fails the deployment and the previous release stays current; a file or `../..` publish path is refused;
  - Nixpacks 1.41.0 build of `heroku/node-js-getting-started` (no Dockerfile): `scontrol-app/nix-app:latest` built and started with `pull_images: true` (no pull attempted), the env reached the container, the secret is in neither log; `build_tools` reported `["nixpacks"]` only once the binary was on `PATH`;
  - all three projects removed through the agent (containers, release directories, project directories).
- [ ] Not run: Railpack and pack builds, static-site domains against real Nginx/certbot, a GitHub App against real GitHub, the new UI in a browser, and the installer's builder download on Ubuntu.

- [x] `cargo test --workspace` with all agent features against PostgreSQL: 75 tests passed (agent 39, api 25, core 11). Default-feature agent build: 10 passed.
  - New tests: image reference, relative path, and container path rules; application spec validation (hostile image, service name, mount target, empty health check, escaping build context); rendered application YAML (exact keys, exec-form health check keeps a hostile argument as one argument, absolute `env_file` only allowed for the project's own file); 25 Compose policy rejections; host-path collection; symlink resolution of host paths; the generated file replaces a planted symlink without writing through it; `RuntimeFeatures` defaults; an API test covering application validation errors, defaults, webhook refusal for image apps, and Git access/webhooks for Dockerfile apps.
- [x] `SQLX_OFFLINE=true cargo clippy --workspace --all-targets -- -D warnings`, with default and with all optional agent features. `cargo fmt --all -- --check` clean. No SQL changed, so `.sqlx` is unchanged.
- [x] `SQLX_OFFLINE=true cargo build --release --workspace` with all agent features. Binaries: agent 7.8 MB, api 19.8 MB, cli 7.4 MB (Linux; the earlier figures were macOS).
- [x] Frontend `npm run build`.
- [x] Local E2E (Ubuntu, real Docker and Compose v5.0.1, release agent over its socket, run as a docker-group user):
  - image app (`redis:alpine`): container healthy through the exec-form health check; `mem_limit` 64 MiB, 0.25 CPU, and restart policy applied; the env value reached the container and appears in neither the deployment log nor the agent log; `env` 0600 and the project directory 0700;
  - Dockerfile app (`github.com/docker/welcome-to-docker`): cloned, built, healthy, commit SHA recorded; the redeploy took the shallow-fetch path and regenerated `.scontrol-app.yml` after `git clean`;
  - raw Compose with `env_file: /etc/shadow` or `build: /` was refused before Docker ran;
  - both projects removed through the agent.
- [ ] Not run: the new UI forms in a browser; a Dockerfile app from a private repository.

Previous run, 2026-11-03, D2 private Git plus the mail and legacy-container argv fixes:

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
