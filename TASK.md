# scontrol implementation checklist

Durum işaretleri: `[x]` tamamlandı, `[-]` devam ediyor, `[ ]` planlandı.

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

Research basis: Dokploy source `853ca33` (2026-09-03 local inspection), notably
`packages/server/src/db/schema/application.ts`, `compose.ts`, `deployment.ts`,
`utils/builders/compose.ts`, and `utils/docker/domain.ts`.

- [x] Replace the product direction “Portainer-lite” with application/Compose deployments.
- [x] Define target model: project/app identity, source type, Compose file, environment, source path, deployment lifecycle/logs, service/domain bindings.
- [x] Add Docker deployment types, DB migration, and typed adapter/RPC operations.
- [x] Agent implementation: validate project IDs and Compose paths; materialize Git/raw source and `.env` with restrictive permissions; use `docker compose -p <project> ... up -d --remove-orphans`; capture bounded Docker logs.
- [x] API endpoints/MCP tools: create/deploy/redeploy, stop/remove, inspect services, deployment history and bounded log retrieval.
- [ ] Domain route integration: generate validated Nginx reverse-proxy vhosts for a declared project service port; certificate issuance follows standard TLS workflow.
- [-] UI: dashboard and project list implemented; add deploy form, write-only environment editor, service health and deployment log viewer.
- [-] Tests: invalid Compose privilege/bind-mount rejection and typed RPC coverage added; add failed-deploy status/log, no-shell argv, and secret non-persistence integration tests.

## Installation and operations

- [x] Ubuntu/Debian installer with explicit Docker/Mail/Antivirus prompts and Cargo feature selection.
- [x] Install GitHub CLI and require interactive `gh auth login` before enabling automation.
- [x] systemd API/agent units and non-secret managed-config PR watcher.
- [x] Auto-commit/PR script only stages allow-listed agent config snapshots and rejects secret-looking keys.
- [x] Wire frontend build and Nginx static/API reverse proxy into installer.
- [ ] Clean-VM Ubuntu 24.04 installer E2E test.

## Web frontend

- [x] Vite + React 19 dashboard scaffold with loading/error and optional-capability states.
- [-] Implement authenticated create/deploy/log workflows.

## Validation

- [x] `cargo build --workspace` with `DATABASE_URL` compile-time SQL validation.
- [x] `cargo test --workspace` (17 tests passing against temporary PostgreSQL).
- [x] `cargo sqlx prepare --workspace` generated offline query metadata.
- [x] `SQLX_OFFLINE=true cargo build --release --workspace` verification (all optional agent features).
- [x] `SQLX_OFFLINE=true cargo clippy --workspace --all-targets -- -D warnings` (all optional agent features).
