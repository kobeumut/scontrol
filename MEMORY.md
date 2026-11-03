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

## Open Items

The full list is in `TASK.md`. The largest open items as of 2026-11-03:

- Docker project domains: the agent does not yet generate Nginx reverse-proxy vhosts for declared project services.
- Docker UI: deploy form, write-only environment editor, service health, and deployment log viewer.
- Clean-VM E2E test of the installer on Ubuntu 24.04.
