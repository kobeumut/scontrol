# PLAN: Dokploy feature parity with a smaller footprint

This plan covers the Docker deployment side of scontrol. The goal is to cover what Dokploy does, using Rust binaries, the host's Nginx and systemd, and PostgreSQL, without Dokploy's runtime stack. Read it together with `TASK.md` (the live checklist) and `.kilo/plans/1789208962783-rust-server-panel-plan.md` (the overall panel plan). Every decision here follows the security rules in `AGENTS.md`.

Dokploy is conceptual input only. No Dokploy code is copied into this repository.

## 1. Research basis

- **Source:** `github.com/dokploy/dokploy` at commit `d64ebed` (2026-11-02), inspected 2026-11-03. Paths are relative to that repository. This supersedes the earlier `853ca33` notes in `TASK.md`.
- **Main files read:**
  - Schema: `packages/server/src/db/schema/*.ts`
  - Pipeline: `services/application.ts`, `services/compose.ts`
  - Builders, providers, Docker: `utils/builders/*`, `utils/providers/*`, `utils/docker/*`
  - Routing: `utils/traefik/*`
  - Backups and schedules: `utils/backups/*`, `utils/volume-backups/*`, `utils/schedules/*`
  - Monitoring: `monitoring/*`, `apps/monitoring` (Go)
  - Queues and sockets: `apps/dokploy/server/queues/*`, `apps/dokploy/server/wss/*`
  - Process execution: `utils/process/execAsync.ts`

### What Dokploy runs

| Area | Dokploy |
|---|---|
| Panel | One Next.js 16 / Node 24 process for UI, tRPC (about 58 routers), WebSockets, crons, and the build queue |
| Default containers | `dokploy`, `dokploy-postgres`, `dokploy-traefik`; optionally `dokploy-monitoring` (Go), Vector, cAdvisor |
| Runtime | Docker Swarm on every install, including single nodes. Each app and database is a Swarm service, and each deploy is a `service.update` followed by task polling |
| Routing | Traefik v3: a file provider with one YAML per app, plus Docker labels injected into Compose files. Certificates come from Traefik's ACME (`acme.json`) |
| Queue | Self-hosted installs use an in-memory per-server queue (`queues/in-memory-queue.ts`). It is lost on restart, and leftover `running` rows are marked `cancelled`. Concurrency is `buildsConcurrency`, effectively 1 when unlicensed. Cancel runs `pkill -f "docker build"`, which kills every build on the host |
| Commands | About 319 `execAsync` shell-string call sites against 5 argv-based ones. Clone, build, backup, and schedule steps are each a single `bash` string redirected into a log file |
| Logs | One unbounded log file per deployment, cleaned by cron. They are streamed with `tail -f` over WebSocket. Container log search is a shell `grep` |
| Metrics | While a stats viewer is open, `docker stats --no-stream` runs per container every 1.3 s. The Go monitor runs `docker stats` every tick (60 s default). History is kept in JSON files and SQLite |
| Secrets | Only `env`, `buildArgs`, and `buildSecrets` are encrypted at rest. DB, registry, S3, and git passwords and tokens, and SSH private keys, are stored as plain text. S3 keys and git auth headers are passed on command lines. Rollback snapshots include registry credentials |

### Where Dokploy spends resources (and where scontrol saves them)

1. **The Node runtime and bundled tools.** Dokploy's own `Dockerfile` notes that a pnpm wrapper alone costs about 100 MB RSS. scontrol is two Rust daemons. The plan's target is about 50 MB RAM for the base panel (see §4).
2. **Traefik as an extra always-on container.** scontrol uses the host Nginx, which the panel already needs for PHP, Node, and Python sites.
3. **Swarm on single hosts.** It adds overlay networking, service-update latency, and 3 s task polling to every deploy. scontrol uses `docker compose up -d` directly.
4. **Process-per-sample metrics.** Spawning `docker stats` for every sample is expensive. scontrol samples `/proc` and cgroup v2 files in-process.
5. **Lost work and serial builds.** Dokploy's queue drops jobs on restart and runs one build at a time. scontrol uses a durable PostgreSQL queue that runs separate projects in parallel.
6. **Shell strings.** Shell strings make Dokploy patch injection risks one by one (regex blocklists, grep escaping). scontrol's typed argv removes the shell entirely, which also means one fewer `bash` process per step.

## 2. Design rules for parity

- **Projects, not containers.** A deployable unit is a Compose project, or later an application, with a stable slug. Raw `docker run` CRUD stays out of the product surface (`MEMORY.md`).
- **The host does the work it already can.** Nginx serves routing and TLS, certbot or an ACME crate handles certificates, systemd timers run schedules, and PostgreSQL holds state and the queue. Containers run user workloads only.
- **Typed argv only, and no secrets on argv.** Pass credentials through environment variables, files with mode 0600, `GIT_ASKPASS`, a per-project `DOCKER_CONFIG`, or stdin. Never put them in arguments, which `ps` can read.
- **Write-only secrets.** Environment values go straight to the agent, which stores them in a 0600 file. Masked values are never returned, and they are redacted in deployment logs. Later secret types, such as registry passwords, S3 keys, and git tokens, are encrypted at rest with AES-GCM.
- **Bounded everything.** Each log has a size cap and retention, each command a timeout, each list a limit, and polling runs only as a fallback to in-process wakeups.
- **Optional by feature flag.** Docker deployments, builders (Nixpacks, Railpack, buildpacks), backups, and monitoring extras are compiled and installed only on opt-in.

## 3. Parity matrix

Status: ✅ done · 🟡 partial · ⬜ planned · ✖ out of scope.

| Dokploy feature | scontrol design | Phase | Status |
|---|---|---|---|
| Compose from Git or raw YAML | Agent materialises source under `/var/lib/scontrol/docker/<slug>/code` and validates the Compose policy (no privileged, host namespaces, devices, or host binds) | D0 | ✅ |
| Deployment queue | `compose_deployments` rows are the queue. Workers claim with `FOR UPDATE SKIP LOCKED` under an advisory lock, one project runs at a time, separate projects run in parallel (`PANEL_DEPLOY_WORKERS`, default half the CPUs, capped at 4), and the queue survives restarts | D0 | ✅ |
| Deployment logs | Streamed line by line into `logs/<deployment>.log`, redacted, capped at 5 MiB, 20 kept per project. Read with byte offsets over HTTP polling, with no long-lived socket | D0 | ✅ |
| Env variables | Write-only, sent to the agent through a separate RPC, stored 0600 outside the source tree, kept across redeploys | D0 | ✅ |
| Compose domains (Traefik labels) | A generated `scontrol.override.yml` publishes `127.0.0.1:<40000-49999>:<container port>`. An Nginx vhost proxies to it with WebSocket upgrade. If `nginx -t` fails, the change is rolled back | D0 | ✅ |
| Let's Encrypt | `certbot certonly --webroot` from a shared ACME webroot. The TLS vhost is rendered only once a certificate exists. A renewal hook reloads Nginx. A certificate failure is reported as a warning and the site serves HTTP | D0 | ✅ (needs live E2E) |
| Fast redeploys | An existing checkout of the same remote gets a shallow `fetch` + `reset --hard` + `clean`, not a re-clone. The commit SHA is recorded per deployment | D0 | ✅ |
| Cancel deployment | Queued jobs are cancelled in the DB. Running jobs: the agent keeps a per-deployment cancel signal; each step runs in its own process group, which gets SIGINT, then SIGKILL after 10 s | D0 / D1 | ✅ |
| Container logs | Bounded `docker compose logs --tail`, plus live SSE from an agent-side `logs --follow` in its own process group (stopped on disconnect or after 1 h, 16 followers max). Env values masked, then search applied in Rust | D1 | ✅ |
| Service status | `docker compose ps --format json`: container, state, status, health, exit code, published ports | D1 | ✅ |
| Webhooks (auto deploy) | HMAC (`X-Hub-Signature-256`, `X-Gitea-Signature`) or GitLab `X-Gitlab-Token`. The secret is derived from the server key and a nonce, never stored. Branch filter; pushes coalesce into a queued job; `trigger = 'webhook'`. `watchPaths` globs match the payload's changed files; an incomplete file list deploys | D1 | ✅ |
| Notifications | `reqwest` (rustls, no redirects) for Slack, Discord, Telegram, ntfy, Gotify, and HMAC-signed generic webhooks; `lettre` for SMTP. Per-channel deploy event flags; one AES-GCM-sealed, write-only secret per channel (`PANEL_SECRET_KEY`) | D1 | ✅ (deploy events only; real-service E2E pending) |
| Deploy UI | React: create/deploy form, write-only env editor, deployment history with cancel, live log viewer (offset polling), service status, domain editor, webhook panel | D1 | ✅ |
| Private Git | Deploy keys per project, generated on the agent (0600, only the public key leaves the host). Git reaches `ssh` through `GIT_SSH` pointing at the agent binary, which `exec`s `ssh` with a fixed argv, a per-project `known_hosts`, and no access to root's keys (`GIT_SSH_COMMAND` would go through `sh -c`). HTTPS tokens are write-only, kept by the agent, and answered by the agent binary as `GIT_ASKPASS` for the remote's host only, never argv. GitHub App installation tokens are minted in panel-api | D2 | ✅ (deploy keys, HTTPS tokens, and GitHub App installation tokens minted per deployment) |
| Applications (single service) | `ComposeSource::Application` (stored in the project's `source` JSONB, no extra table): Dockerfile build from Git (context and Dockerfile validated and symlink-checked inside the checkout, optional target) or a validated image reference; memory/CPU limits, restart policy, exec-form health check, named volumes. Rendered by the agent into a generated Compose file, so one deploy engine serves both. Replicas are not offered (one loopback port per domain) | D2 | ✅ |
| Build types | Dockerfile (core). Nixpacks, Railpack, and buildpacks as opt-in pinned binaries (agent feature `builders`, checksummed installer downloads). Static apps served by Nginx directly from a release directory, with no container (optional exec-form build in a capability-less container; only regular files published) | D2 | ✅ |
| Registries | Per-project `DOCKER_CONFIG` directory; credentials encrypted at rest | D2 | ✅ |
| Projects, environments, env layering | `projects → environments → services` (each owner gets a Default project with `production`; existing services migrate into it). Layers are sealed with `SecretBox` in PostgreSQL, merged project → environment → service (most specific wins) by the queue worker at deploy time, and written to the agent through `write_compose_environment`. Overridden values are masked in logs too; the API returns key names and layer provenance only | D2 | ✅ |
| Databases as services | Postgres, MySQL, MariaDB, Mongo, and Redis containers bound to `127.0.0.1` with persistent named volumes. An external port only through an explicit Nginx `stream` proxy plus UFW rule (Docker-published ports would bypass UFW) | D3 | ✅ |
| Backups to S3 | Spawn the dump tool with argv and credentials in env. Stream through gzip into an in-process S3 multipart upload, or `rclone rcat` with env credentials. Retention by count; restore streams back | D3 | ⬜ |
| Volume backups | Optional stop, then `tar` streamed in-process from the volume mountpoint to S3, then start | D3 | ⬜ |
| Scheduled jobs | Generated systemd timers (`OnCalendar=`, timezone) running `docker compose exec` argv. Runs recorded as job rows with bounded logs | D3 | ⬜ |
| Server and container metrics | Agent samples `/proc` and cgroup v2 every 10–30 s, with no process spawn. Kept as a downsampled ring in PostgreSQL. Threshold alerts fire on edge transitions | D4 | ⬜ |
| Request stats | Nginx JSON `log_format`, read incrementally with offset tracking and aggregated into PostgreSQL | D4 | ⬜ |
| Docker cleanup | Scheduled `docker image prune` / `builder prune` argv with an `until=` filter; freed space reported | D4 | ⬜ |
| Rollbacks | Tag each successful build `<slug>/<service>:deploy-<id>` and keep the last N. Rollback = compose up pinned to that tag plus the stored non-secret config snapshot | D5 | ⬜ |
| Preview deployments | PR webhook → `<slug>-pr-<n>` project on a wildcard subdomain, with limit and TTL cleanup, and a PR comment through the GitHub API | D5 | ⬜ |
| Templates | A vetted catalogue (TOML + Compose) shipped with the panel, using the same validator as user Compose and variable generators in Rust | D5 | ⬜ |
| Redirects, basic auth, path prefix, custom certs | Typed vhost options: `return 301/302`, `auth_basic_user_file`, `location /prefix/`, certificate files stored 0600 | D5 | ⬜ |
| Web terminal | Agent-side PTY running `docker exec` argv only, behind admin RBAC and audit. No host root shell | D5 | ⬜ |
| Multi-server | The same agent on remote hosts over mTLS, or an SSH-forwarded Unix socket with one persistent connection | D6 | ⬜ |
| RBAC and API keys | A resource/action permission table checked in Axum extractors, with project and environment scoping. API keys hashed with Argon2. Nothing paywalled | D6 | ⬜ |
| Swarm cluster management | — | — | ✖ (multi-host by placement, not Swarm) |
| Vault and DNS providers, Vector, enterprise SSO/SCIM | — | — | ✖ for now |

## 4. Performance budgets

These are targets for the D0–D4 work. They are not measured yet. `TASK.md` tracks a benchmark item.

| Metric | Budget |
|---|---|
| Idle RSS, panel-api + panel-agent | ≤ 60 MB combined. The plan's base-panel target is about 50 MB. First measurement: 13.9 MB (6.1 + 7.8, macOS release build, 2026-11-03) |
| Idle CPU | Effectively 0. Each idle queue worker runs one indexed query every 15 s, and an enqueue wakes workers immediately through `tokio::sync::Notify` |
| Time from enqueue to `docker compose up` starting (warm checkout) | < 1 s, plus `git fetch` time |
| Extra always-on containers | 0. No Traefik, Redis, or monitoring container |
| Log memory | Constant. Output is streamed line by line to disk and never buffered whole |
| Metrics sampling | No process spawn per sample. Reads are under 1 ms per container from cgroup files |

## 5. Phases and acceptance criteria

**D0: Durable deployment core (implemented in this change).**
- Async deploys return `202`, and the queue survives a panel-api restart. A `running` row left by a crashed process becomes `failed`.
- Streamed, redacted, bounded deployment logs are readable by offset.
- Domain bindings via loopback ports and Nginx, with rollback when `nginx -t` fails, and certificate issuance.
- Env written separately and kept across redeploys. Incremental git updates and commit SHAs.
- MCP tools `deploy_compose_project` and `compose_deployment_log`.
- Done when: workspace tests pass, including queue-ordering, failure, cancel, and port-allocation tests against PostgreSQL. A local E2E run deploys a real Compose project, checks that the secret is absent from the log and kept across redeploy, and checks the loopback publish.

**D1: Operator UX and automation (implemented; live HTTPS E2E pending with D0).**
- Frontend deploy, env, logs, and domains screens.
- Structured `ps`, cancelling running deployments, and SSE container logs.
- Webhooks with HMAC, and notifications.
- Done when an operator can go from a Git push to a live HTTPS domain with no CLI.

**D2: Applications and builds (in progress).**
- Application model, Dockerfile and image sources, private git, registries, environments, and encrypted secrets at rest.
- Done: application model with image and Dockerfile sources, deploy keys, HTTPS tokens, GitHub App installation tokens, private Docker registries, projects/environments with env layering, Nixpacks/Railpack/buildpacks builders, and static apps.

**D3: Data.**
- Database services, S3 backups and restore, volume backups, and systemd-timer schedules.
- Done: database services (generated sealed credentials, loopback-only ports, opt-in outside access).

**D4: Observability.**
- `/proc` and cgroup sampler, metrics retention, alerts, request stats, and Docker cleanup.

**D5: Delivery extras.**
- Rollbacks, previews, templates, the advanced vhost options, and the web terminal.

**D6: Scale-out.**
- Remote agents, RBAC, and API keys.

## 6. Security differences from Dokploy

These are deliberate, not just parity:
- No user-supplied Compose command override. Dokploy instead allows one, guarded by a regex blocklist.
- Compose files cannot read host files: build contexts, `env_file`, secret and config files, and bind mounts must resolve inside the checkout (symlinks included), and `include`, cross-file `extends`, `label_file`, and volume `driver_opts` are refused.
- Git and S3 credentials never go on argv.
- Rollback snapshots never contain credentials.
- Webhooks require HMAC, not only a URL token.
- Cancelling a deployment kills only that deployment's process group.
- The web terminal never opens a root shell on the host.
