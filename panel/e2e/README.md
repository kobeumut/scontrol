# End-to-end tests

Playwright drives the real frontend and `panel-api` in a browser. Nothing here needs root, Nginx, or Docker:

- `stack.mjs` starts a throwaway PostgreSQL cluster, `scontrol-mock-agent`, `panel-api`, and the built frontend behind `vite preview` (which proxies `/api`, WebSockets included, as Nginx does), all in a temporary directory, and creates the admin user.
- `scontrol-mock-agent` (`crates/panel-core/src/bin/scontrol-mock-agent.rs`, feature `test-utils`) speaks the agent protocol from `MockAdapter` and keeps a little pretend host state: the services of a deployed project, a deployment log, manual job runs, stored backups, metrics, and an echoing terminal. A raw Compose file containing the comment `# e2e: slow` deploys for up to 30 seconds, so a test can watch and cancel a running deployment.

So these tests cover the UI, the API, the database, and the agent protocol. They do not cover what the real agent does on a host (Nginx, Docker, certbot, systemd); that is the pending installer E2E in `TASK.md`.

## Run

Needs Node 20+, the PostgreSQL server binaries (`initdb`, `postgres`), and Chromium.

```bash
cd panel
SQLX_OFFLINE=true cargo build -p panel-api
cargo build -p panel-core --features test-utils --bin scontrol-mock-agent
(cd frontend && npm install && npm run build)

cd e2e
npm install
npx playwright install chromium   # not on Alpine: install the `chromium` package instead
npm test                          # whole suite, about 7 minutes
npx playwright test tests/sites.spec.ts
```

Rebuild the binaries and the frontend after changing them; the stack runs what is built.

| Variable | Meaning |
|----------|---------|
| `E2E_CHROMIUM` | Path of the Chromium to use. `/usr/bin/chromium` is picked automatically on Alpine, where Playwright's own build does not run. |
| `E2E_PG_BIN` | Directory with `initdb` and `postgres`, when they are not found automatically. |
| `E2E_PG_PORT`, `E2E_API_PORT`, `E2E_WEB_PORT` | Ports (defaults 55432, 18080, 14173). |
| `E2E_REUSE_STACK=1` | Test against a stack that is already running (see below) instead of starting one. |
| `E2E_KEEP_DIR=1` | Keep the temporary directory (database, logs) after the stack stops. |

## Try the panel by hand

```bash
npm run stack
```

It prints the URL (`http://localhost:14173`), the admin login, and the log directory, and runs until Ctrl-C. With it running, `E2E_REUSE_STACK=1 npx playwright test tests/<file>` runs a spec against it without the startup time.

## Writing tests

- One spec per area of the UI; `tests/helpers.ts` has `signIn`, `openTab`, `unique`, and `watchForFailures` (fails a test on a 5xx response or an uncaught page error).
- The whole suite shares one panel and database and runs in one worker. Name everything you create with `unique()`, never assert that a list is empty or has an exact length, and delete what you create.
- Wait for the request an action sends before reloading the page, or the reload aborts it.
- When a screen needs the host to answer with something, teach `scontrol-mock-agent` to answer it; do not change `MockAdapter`'s defaults, which the Rust unit tests rely on.
