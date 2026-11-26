# Real-Docker template check

`smoke.mjs` installs application templates for real. The browser tests in `panel/e2e` run against `scontrol-mock-agent`, which only pretends to deploy; this check runs panel-api with the real `panel-agent` against the local Docker daemon, installs a template through the panel API, and requires the application to answer HTTP.

## Run it

```bash
cd panel
SQLX_OFFLINE=true cargo build -p panel-api -p panel-agent --features panel-agent/docker
node e2e/real-docker/smoke.mjs                 # WordPress
node e2e/real-docker/smoke.mjs ghost directus  # the named templates
node e2e/real-docker/smoke.mjs --all           # the whole catalogue, one at a time
```

It needs Node 20 or newer, a user that may use Docker (the `docker` group), the Compose plugin, the `postgres:16-alpine` image for the panel database, and Linux (the applications are reached on their container address). It needs no root, Nginx, or systemd. The exit code is 0 only when every requested template passed.

| Option | Meaning |
|--------|---------|
| `--all` | Every template of the catalogue, one after the other. |
| `--keep-images` | Leave the images the run pulled in place (faster next time). |
| `--reserve-mb <n>` | Free disk to keep besides a template's images (default 500). A template that does not fit is reported as failed without being started. |

`E2E_KEEP_DIR=1` keeps the temporary directory with `api.log` and `agent.log`.

## What a run does

1. Starts a throwaway PostgreSQL container, the real `panel-agent` with every path of its configuration pointed at a temporary directory (so it runs unprivileged), and panel-api on a free loopback port. It then creates the admin and a project.
2. Per template: installs it into the project's environment, deploys it with fresh images, and waits for the queued deployment. A failed deployment prints the end of its log.
3. Proves the application works:
   - WordPress: completes the installer (which creates the tables in MariaDB) and reads the front page with the site title it set; after the redeployment the REST index must return that title from the database.
   - Ghost: the admin API reports the version and the front page renders.
   - Directus: `/server/health` is `ok` and a login with a wrong password is refused from the users table.
   - Uptime Kuma: the dashboard is served.
   - Plausible: `/api/health` is 200 and the registration page renders.
4. Reads the container logs through the panel, redeploys, and proves the application again (for WordPress: the data survived).
5. Deletes the service with `?volumes=true` and fails if a container, network, volume, or release image tag of it is left.

## What it leaves alone

Every project slug, the PostgreSQL container, and the temporary directory carry a random per-run id (`rd<id>-…`, `scontrol-rd-<id>-pg`). Only those names are stopped or removed, also when the run fails or is interrupted with Ctrl-C. The script never prunes.

Installing a template pulls its images. Afterwards the image store is put back for exactly the images the template names: a tag the run pulled is removed, and a tag the pull moved to a newer build points at its previous image again. Pass `--keep-images` to skip this.

## What it does not cover

Domains, HTTPS, scheduled jobs, the firewall, and database outside access need the host Nginx, certbot, systemd, and UFW, which an unprivileged agent cannot drive. A Redis database service also needs root: the agent hands its configuration file to the container user.
