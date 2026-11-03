# MEMORY.md

Durable project context that is not obvious from the code. Read it together with `AGENTS.md`. Status and checklists live in `TASK.md`; this file records decisions and why they were made. Last reviewed: 2026-11-03.

## Decisions

- **Docker is a Dokploy-style deployment workflow, not a container dashboard.** Per the plan's research summary, HestiaCP and CloudPanel both lack Docker support. A Portainer-style dashboard was rejected in favour of application and Compose deployments (`TASK.md`, Docker section).
  - How to apply: do not add `docker run` or raw container CRUD endpoints. Model work as projects and Compose deployments.
- **Docker, Mail, and Antivirus are opt-in at install time and compile-time features.** They are off by default. The base install stays small (the plan targets ~50 MB RAM) and installs no packages nobody asked for.
  - How to apply: new optional code goes behind a `panel-agent` feature. Do not make it unconditional.
- **Host commands are typed argv only.** This is the plan's core security requirement: no shell, no string-built commands, least privilege.
- **Auto-PR commits only allow-listed, non-secret config.** The watcher runs as root and pushes to GitHub, so the allow-list and the secret-key rejection in `install/auto-pr.sh` form a security boundary.
- **Offline SQLx metadata is committed on purpose (`panel/.sqlx`).** Builds with `SQLX_OFFLINE=true` need no database.
  - How to apply: after any SQL change, run `cargo sqlx prepare --workspace` and commit the updated `.sqlx` files, or offline builds break.
- **Dokploy is research input, not a code source.** Use its conceptual model only. Do not copy its code.
- **The deployment queue is PostgreSQL, not memory or Redis.** Dokploy's self-hosted queue is in-memory: it loses jobs on restart and runs effectively one build at a time. Here `compose_deployments` rows are the jobs (decided 2026-11-03, `PLAN.md` §3).
  - How to apply: new deploy-like work (webhooks, previews, rollbacks, backups) enqueues rows and lets the workers run them. Keep per-project serialisation in the claim query.
- **Secrets never ride in queue rows.** Environment values are written to the agent through a separate RPC before a job is queued, so a durable queue does not mean secrets at rest in PostgreSQL.
- **Webhook secrets are derived, not stored.** The secret is HMAC(server key, project ID + nonce), and only the nonce is in PostgreSQL, so a database dump does not leak deploy triggers. The server key comes from `PANEL_COOKIE_KEY`, so rotating that key invalidates every webhook secret (decided 2026-11-03).
- **Stored secrets are sealed with `PANEL_SECRET_KEY`, separate from the cookie key (decided 2026-11-03).** Notification credentials are the first secrets panel-api must read back, so they are AES-256-GCM sealed (`services/secret_box.rs`) with the row ID and kind as associated data. A separate key lets operators rotate `PANEL_COOKIE_KEY` (logging everyone out) without losing stored secrets. Installs from before the key existed fall back to the cookie key; the installer never adds the key to an existing env file, because that would make already-stored secrets unreadable.
  - How to apply: seal every new stored secret that panel-api itself must read back (registry, S3, GitHub App keys) with `SecretBox` and row-bound AAD. Never return it; report `has_secret` instead.
- **Git deploy keys and HTTPS tokens live on the agent, not in PostgreSQL (decided 2026-11-03).** Only the agent ever uses them, so they follow the environment model: written once through a dedicated RPC, kept in 0600 files in the project directory, and absent from the database (which stores the public key and the token's host). The private key is generated on the agent and never leaves it.
  - How to apply: a credential that only host commands need goes to the agent this way. Seal it in PostgreSQL only when panel-api has to use it (for example to mint a GitHub App token).
- **Git helpers are the agent binary itself, not shell snippets (decided 2026-11-03).** `GIT_SSH_COMMAND`, `core.sshCommand`, and `!`-style credential helpers are run by Git through `sh -c`. Instead the agent sets `GIT_SSH` and `GIT_ASKPASS` to its own executable plus `SCONTROL_GIT_HELPER=ssh|askpass`; `main` detects that before logging starts. The ssh helper `exec`s `ssh` through `exec::exec_replace` (allow-listed, covered by the source scan). The askpass helper answers only for the host the token was saved for, so a redirect to another host gets nothing.
  - How to apply: a new Git or SSH integration adds a helper mode or typed argv, never a command string for Git to interpret.
- **Processes are spawned only through an allow-list in `adapters/exec.rs`.** A shell or interpreter is never on the list, and a source-scan test fails the build if code elsewhere spawns processes or names a program that is not a literal on the list (2026-11-03).
  - How to apply: add a new host tool to `ALLOWED_PROGRAMS` in the same change that calls it. Pass secrets through stdin (`run_with_stdin`), env, or 0600 files, never argv.
- **Live logs are a streamed RPC, not polling.** `follow_compose_logs` is the one agent request that answers with many lines; closing the connection is the stop signal. Redaction runs before search so search cannot reveal masked values.
- **Applications are a Compose source, not a separate table (decided 2026-11-03).** An application is `ComposeSource::Application`, which the agent renders into a generated Compose file. That reuses the queue, logs, environment, domains, webhooks, and Git access with no migration, and keeps one deploy engine. The project environment reaches the container through `env_file` pointing at the 0600 `env` file.
  - How to apply: new application options become typed `ApplicationSpec` fields rendered through `serde_yaml` values, validated in `ApplicationSpec::validate` (shared by API and agent). Never template YAML strings. Health checks stay exec-form (`CMD`).
- **The Compose policy treats every host path as hostile (decided 2026-11-03).** `docker compose` runs as root, so `env_file`, build contexts, secret/config files, `include`, `extends.file`, `label_file`, and volume `driver_opts` can all read host files. Relative paths are allowed only when they resolve, symlinks included, inside `code/`.
  - How to apply: when Compose gains a key that names a host path, add it to `compose_policy` and the symlink resolution in the same change.
- **Routing uses the host Nginx, not Traefik.** Compose domains publish the service on a loopback port (40000–49999) and get a generated vhost. That avoids an extra always-on proxy container and Docker-label rewriting of user Compose files.
  - How to apply: do not inject labels into, or rewrite, user Compose files. Add routing features (redirects, basic auth, path prefixes) as typed vhost options.

## Open Items

The full list is in `TASK.md`. The largest open items as of 2026-11-03 (after D0):

- Docker domains/TLS are implemented but unverified against real Nginx and certbot on Ubuntu 24.04.
- D1 is implemented. Notification delivery is verified end to end only for the generic webhook; Slack, Discord, Telegram, ntfy, Gotify, and SMTP still need a real-service check.
- Clean-VM E2E test of the installer on Ubuntu 24.04.
