#!/usr/bin/env node
// Installs application templates for real: panel-api and the real
// `panel-agent` (built with `--features docker`) run unprivileged against the
// local Docker daemon, a template is installed through the same API calls the
// dashboard makes, and the deployed application has to answer HTTP.
//
//   node smoke.mjs                 # WordPress
//   node smoke.mjs ghost directus  # the named templates
//   node smoke.mjs --all           # the whole catalogue, one at a time
//
// Options: --keep-images (leave the images this run pulled in place),
//          --reserve-mb <n> (free disk to keep besides a template's images,
//          default 500; a template that does not fit is not started).
//
// Everything the run creates carries a random per-run name (`rd<run>-...`),
// and only those names are ever stopped or removed. See README.md.
import { spawn, execFileSync } from 'node:child_process';
import { existsSync, mkdtempSync, mkdirSync, rmSync, writeFileSync, openSync, statfsSync, readFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { randomBytes } from 'node:crypto';
import net from 'node:net';

const here = dirname(fileURLToPath(import.meta.url));
const panel = resolve(here, '..', '..');
const RUN = randomBytes(3).toString('hex');
const ADMIN = { username: 'admin', email: 'admin@example.test', password: 'correct-horse-battery' };
const SITE_TITLE = `Real Docker ${RUN}`;

const args = process.argv.slice(2);
const flag = (name) => args.includes(name);
const option = (name, fallback) => {
  const at = args.indexOf(name);
  return at >= 0 && args[at + 1] ? args[at + 1] : fallback;
};
const keepImages = flag('--keep-images');
const reserveMb = Number(option('--reserve-mb', 500));
const requested = args.filter((a, i) => !a.startsWith('--') && args[i - 1] !== '--reserve-mb');
// Rough disk a template needs for images and first-start data, in MB. An
// installation pulls every image, also one that is already local.
const DISK_MB = { wordpress: 1500, ghost: 1700, directus: 1200, 'uptime-kuma': 600, plausible: 1700 };

const log = (message) => console.log(`[real-docker] ${message}`);
const sleep = (ms) => new Promise((done) => setTimeout(done, ms));

function docker(...argv) {
  return execFileSync('docker', argv, { stdio: ['ignore', 'pipe', 'pipe'] }).toString().trim();
}
function dockerQuiet(...argv) {
  try { return docker(...argv); } catch { return ''; }
}

function findBinary(name) {
  for (const profile of ['debug', 'release']) {
    const candidate = join(panel, 'target', profile, name);
    if (existsSync(candidate)) return candidate;
  }
  throw new Error(
    `${name} is not built. Run in panel/: SQLX_OFFLINE=true cargo build -p panel-api && ` +
    'SQLX_OFFLINE=true cargo build -p panel-agent --features docker',
  );
}

function freePort() {
  return new Promise((done, reject) => {
    const server = net.createServer();
    server.once('error', reject);
    server.listen(0, '127.0.0.1', () => {
      const { port } = server.address();
      server.close(() => done(port));
    });
  });
}

async function until(label, timeoutMs, probe, everyMs = 2000) {
  const deadline = Date.now() + timeoutMs;
  let last = '';
  for (;;) {
    try {
      const value = await probe();
      if (value) return value;
    } catch (error) {
      last = String(error?.cause?.code ?? error?.message ?? error);
    }
    if (Date.now() > deadline) throw new Error(`timed out waiting for ${label}${last ? ` (${last})` : ''}`);
    await sleep(everyMs);
  }
}

function freeMb() {
  let root = '/';
  try { root = docker('info', '--format', '{{.DockerRootDir}}'); } catch { /* reported by the daemon check */ }
  for (const path of [root, '/']) {
    try {
      const stat = statfsSync(path);
      return Math.floor((stat.bavail * stat.bsize) / (1024 * 1024));
    } catch { /* try the next path */ }
  }
  return Infinity;
}

// ---------------------------------------------------------------- the stack

const stack = { dir: null, children: [], postgres: null, api: null, cookie: null, slugs: new Set(), projects: new Map(), images: new Map() };

async function api(method, path, body) {
  const response = await fetch(`${stack.api}${path}`, {
    method,
    headers: { 'content-type': 'application/json', ...(stack.cookie ? { cookie: stack.cookie } : {}) },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await response.text();
  let json;
  try { json = text ? JSON.parse(text) : null; } catch { json = text; }
  if (!response.ok) throw new Error(`${method} ${path} → ${response.status} ${text.slice(0, 400)}`);
  return { json, response };
}

async function startStack() {
  docker('version', '--format', '{{.Server.Version}}');
  docker('compose', 'version');
  const agentBinary = findBinary('panel-agent');
  const apiBinary = findBinary('panel-api');

  const dir = mkdtempSync(join(tmpdir(), `scontrol-rd-${RUN}-`));
  stack.dir = dir;
  for (const sub of ['run', 'docker', 'sites', 'static', 'acme', 'letsencrypt', 'systemd',
    'nginx/sites-available', 'nginx/sites-enabled', 'nginx/stream', 'nginx/vhost']) {
    mkdirSync(join(dir, sub), { recursive: true });
  }
  const socket = join(dir, 'run', 'agent.sock');
  // Every path the agent writes to is private to this run, so it needs no
  // root. Nginx, certbot, systemd and UFW are not available this way.
  const config = join(dir, 'agent.toml');
  writeFileSync(config, [
    `socket_path = "${socket}"`,
    `sites_root = "${join(dir, 'sites')}"`,
    `docker_projects_root = "${join(dir, 'docker')}"`,
    `static_sites_root = "${join(dir, 'static')}"`,
    `acme_webroot = "${join(dir, 'acme')}"`,
    `letsencrypt_live_dir = "${join(dir, 'letsencrypt')}"`,
    `systemd_unit_dir = "${join(dir, 'systemd')}"`,
    `nginx_sites_available = "${join(dir, 'nginx/sites-available')}"`,
    `nginx_sites_enabled = "${join(dir, 'nginx/sites-enabled')}"`,
    `nginx_stream_dir = "${join(dir, 'nginx/stream')}"`,
    `nginx_vhost_secrets_dir = "${join(dir, 'nginx/vhost')}"`,
    `nginx_stats_log = "${join(dir, 'nginx/access.json.log')}"`,
    '',
  ].join('\n'));

  const run = (label, command, argv, env = {}) => {
    const out = openSync(join(dir, `${label}.log`), 'a');
    const child = spawn(command, argv, { cwd: dir, env: { ...process.env, ...env }, stdio: ['ignore', out, out] });
    child.on('exit', (code) => { if (code) console.error(`[real-docker] ${label} exited with ${code}; see ${join(dir, `${label}.log`)}`); });
    stack.children.push(child);
  };

  stack.postgres = `scontrol-rd-${RUN}-pg`;
  log(`starting PostgreSQL container ${stack.postgres}`);
  docker('run', '-d', '--name', stack.postgres, '-e', 'POSTGRES_USER=scontrol', '-e', 'POSTGRES_PASSWORD=scontrol',
    '-p', '127.0.0.1::5432', 'postgres:16-alpine');
  await until('PostgreSQL', 60_000, () => {
    docker('exec', stack.postgres, 'pg_isready', '-h', '127.0.0.1', '-U', 'scontrol');
    return true;
  }, 500);
  const pgPort = docker('port', stack.postgres, '5432/tcp').split('\n')[0].split(':').pop();

  run('agent', agentBinary, ['--config', config]);
  await until('the agent socket', 20_000, () => existsSync(socket), 200);

  const apiPort = await freePort();
  stack.api = `http://127.0.0.1:${apiPort}`;
  run('api', apiBinary, [], {
    DATABASE_URL: `postgres://scontrol:scontrol@127.0.0.1:${pgPort}/postgres`,
    PANEL_API_BIND: `127.0.0.1:${apiPort}`,
    PANEL_AGENT_SOCKET: socket,
    PANEL_COOKIE_KEY: randomBytes(32).toString('base64'),
    PANEL_SECRET_KEY: randomBytes(32).toString('base64'),
  });
  await until('panel-api', 60_000, async () => (await fetch(`${stack.api}/api/health`)).ok, 300);

  await api('POST', '/api/auth/bootstrap-admin', ADMIN);
  const login = await api('POST', '/api/auth/login', { username: ADMIN.username, password: ADMIN.password });
  stack.cookie = login.response.headers.getSetCookie().map((c) => c.split(';')[0]).join('; ');
  log(`panel-api is up at ${stack.api} (logs: ${dir})`);
}

// Local image tags and the image each one points at.
function imageTags() {
  const tags = new Map();
  for (const line of dockerQuiet('image', 'ls', '--no-trunc', '--format', '{{.Repository}}:{{.Tag}} {{.ID}}').split('\n')) {
    const [ref, id] = line.split(' ');
    if (ref && id && !ref.includes('<none>')) tags.set(ref, id);
  }
  return tags;
}

// Puts the image store back as it was for the images a template names: a
// tag this run pulled is removed, and a tag the pull moved to a newer build
// is pointed at the image it had before (which Docker keeps, untagged).
// Images the run did not pull are never touched.
function restoreImages() {
  for (const [ref, before] of stack.images) {
    const now = dockerQuiet('image', 'inspect', '--format', '{{.Id}}', ref);
    if (!now || now === before) continue;
    dockerQuiet('image', 'rm', ref);
    if (before) dockerQuiet('image', 'tag', before, ref);
  }
  stack.images.clear();
}

// Removes what is left of one of this run's projects straight through Docker.
// `slug` always carries this run's random id, so nothing else can match.
function forceRemove(slug) {
  const filter = `label=com.docker.compose.project=${slug}`;
  for (const id of dockerQuiet('ps', '-aq', '--filter', filter).split('\n').filter(Boolean)) dockerQuiet('rm', '-f', '-v', id);
  for (const name of dockerQuiet('volume', 'ls', '-q', '--filter', filter).split('\n').filter(Boolean)) dockerQuiet('volume', 'rm', name);
  for (const id of dockerQuiet('network', 'ls', '-q', '--filter', filter).split('\n').filter(Boolean)) dockerQuiet('network', 'rm', id);
  for (const ref of dockerQuiet('image', 'ls', '--format', '{{.Repository}}:{{.Tag}}', `${slug}/*`).split('\n').filter(Boolean)) dockerQuiet('image', 'rm', ref);
}

function leftovers(slug) {
  const filter = `label=com.docker.compose.project=${slug}`;
  return {
    containers: dockerQuiet('ps', '-aq', '--filter', filter),
    volumes: dockerQuiet('volume', 'ls', '-q', '--filter', filter),
    networks: dockerQuiet('network', 'ls', '-q', '--filter', filter),
    pins: dockerQuiet('image', 'ls', '-q', `${slug}/*`),
  };
}

async function stopStack() {
  for (const [slug, id] of stack.projects) {
    try { await api('DELETE', `/api/compose-projects/${id}?volumes=true`); } catch { /* fall back to Docker below */ }
    forceRemove(slug);
  }
  for (const slug of stack.slugs) forceRemove(slug);
  if (!keepImages) restoreImages();
  for (const child of [...stack.children].reverse()) child.kill('SIGTERM');
  await sleep(500);
  for (const child of stack.children) child.kill('SIGKILL');
  if (stack.postgres) dockerQuiet('rm', '-f', '-v', stack.postgres);
  if (stack.dir && !process.env.E2E_KEEP_DIR) rmSync(stack.dir, { recursive: true, force: true });
}

// ------------------------------------------------------- deploy and observe

async function waitForDeployment(projectId, deploymentId) {
  const done = await until(`deployment ${deploymentId}`, 20 * 60_000, async () => {
    const { json } = await api('GET', `/api/compose-projects/${projectId}/deployments/${deploymentId}`);
    return ['queued', 'running'].includes(json.status) ? null : json;
  });
  if (done.status !== 'succeeded') {
    const { json } = await api('GET', `/api/compose-projects/${projectId}/deployments/${deploymentId}/log`);
    const tail = String(json.content ?? '').split('\n').filter((l) => !/Pulling|Waiting|Download|Extract|Verifying|Pull complete/.test(l)).slice(-40).join('\n');
    throw new Error(`deployment ${done.status}: ${done.error_message}\n--- deployment log ---\n${tail}`);
  }
  return done;
}

async function services(projectId) {
  return (await api('GET', `/api/compose-projects/${projectId}/services`)).json;
}

function containerAddress(container) {
  const address = docker('inspect', '--format', '{{range .NetworkSettings.Networks}}{{.IPAddress}} {{end}}', container).split(' ')[0];
  if (!address) throw new Error(`container ${container} has no address`);
  return address;
}

// The applications are reached on their container address: publishing a
// port is the job of a domain binding, which needs the host Nginx.
async function appUrl(projectId, service, port) {
  const entry = (await services(projectId)).find((s) => s.name === service);
  if (!entry || entry.state !== 'running') throw new Error(`service ${service} is not running`);
  return `http://${containerAddress(entry.container)}:${port}`;
}

async function get(url, init = {}) {
  const response = await fetch(url, { redirect: 'manual', signal: AbortSignal.timeout(15_000), ...init });
  return { status: response.status, location: response.headers.get('location'), text: await response.text() };
}

const title = (html) => (/<title>([^<]*)<\/title>/i.exec(html)?.[1] ?? '').trim();

// One entry per template: `prove` must show that the application really
// works (not merely that a container is up), `afterRedeploy` that its data
// survived a redeployment.
const CHECKS = {
  wordpress: {
    service: 'wordpress', port: 80,
    async prove(url) {
      await until('WordPress to answer', 180_000, async () => (await get(`${url}/wp-admin/install.php`)).status === 200);
      const form = new URLSearchParams({
        weblog_title: SITE_TITLE, user_name: 'admin', admin_password: `Pw-${randomBytes(12).toString('hex')}`,
        admin_email: 'admin@example.test', blog_public: '0', language: '',
      });
      form.set('admin_password2', form.get('admin_password'));
      // Creating the site writes its tables: the database connection works.
      const installed = await get(`${url}/wp-admin/install.php?step=2`, { method: 'POST', body: form });
      if (installed.status !== 200 || !/Success!/.test(installed.text)) throw new Error(`WordPress install returned ${installed.status}`);
      const front = await until('the WordPress front page', 120_000, async () => {
        const page = await get(`${url}/`);
        return page.status === 200 && title(page.text) === SITE_TITLE ? page : null;
      });
      return `installer finished, front page 200 <title>${title(front.text)}</title>`;
    },
    // WordPress remembers the address it was installed on and redirects
    // page requests from any other one, and a redeployment may move the
    // container. The REST index is answered wherever it is asked, and its
    // site name comes from the database.
    async afterRedeploy(url) {
      const site = await until('the WordPress REST API', 120_000, async () => {
        const page = await get(`${url}/?rest_route=/`);
        return page.status === 200 && JSON.parse(page.text).name === SITE_TITLE ? JSON.parse(page.text) : null;
      });
      return `REST index 200, site name "${site.name}" read back from the database`;
    },
  },
  ghost: {
    service: 'ghost', port: 2368,
    async prove(url) {
      const site = await until('the Ghost admin API', 300_000, async () => {
        const page = await get(`${url}/ghost/api/admin/site/`);
        return page.status === 200 ? JSON.parse(page.text).site : null;
      });
      const front = await get(`${url}/`);
      if (front.status !== 200 || !/ghost/i.test(front.text)) throw new Error(`Ghost front page returned ${front.status}`);
      return `admin API 200 (Ghost ${site.version}), front page 200 <title>${title(front.text)}</title>`;
    },
  },
  directus: {
    service: 'directus', port: 8055,
    async prove(url) {
      const health = await until('Directus health', 300_000, async () => {
        const page = await get(`${url}/server/health`);
        return page.status === 200 && JSON.parse(page.text).status === 'ok' ? page : null;
      });
      // A wrong password is answered from the users table: the database is in use.
      const login = await get(`${url}/auth/login`, {
        method: 'POST', headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ email: 'admin@example.com', password: 'definitely-wrong' }),
      });
      if (login.status !== 401) throw new Error(`Directus login probe returned ${login.status}`);
      return `/server/health 200 ${health.text.trim()}, login probe 401 INVALID_CREDENTIALS`;
    },
  },
  'uptime-kuma': {
    service: 'uptime-kuma', port: 3001,
    async prove(url) {
      const page = await until('Uptime Kuma', 180_000, async () => {
        const first = await get(`${url}/dashboard`);
        return first.status === 200 && /Uptime Kuma/.test(first.text) ? first : null;
      });
      return `/dashboard 200 <title>${title(page.text)}</title>`;
    },
  },
  plausible: {
    service: 'plausible', port: 8000,
    async prove(url) {
      const health = await until('Plausible health', 420_000, async () => {
        const page = await get(`${url}/api/health`);
        return page.status === 200 ? page : null;
      });
      const register = await get(`${url}/register`);
      if (register.status !== 200) throw new Error(`Plausible /register returned ${register.status}`);
      return `/api/health 200 ${health.text.trim()}, /register 200 <title>${title(register.text)}</title>`;
    },
  },
};

async function runTemplate(templateId, environmentId) {
  const check = CHECKS[templateId];
  if (!check) throw new Error(`no smoke check for template ${templateId}`);
  const free = freeMb();
  const needed = (DISK_MB[templateId] ?? 1500) + reserveMb;
  if (free < needed) {
    throw new Error(`not started: ${free} MB free where Docker stores images, ${needed} MB wanted ` +
      `(about ${DISK_MB[templateId] ?? 1500} MB for the template, ${reserveMb} MB reserve; see --reserve-mb)`);
  }
  const slug = `rd${RUN}-${templateId}`;
  stack.slugs.add(slug);
  log(`${templateId}: installing as ${slug}`);
  // Created without a deployment first, so the images it names are known
  // (and their current tags recorded) before anything is pulled.
  const { json: installed } = await api('POST', `/api/templates/${templateId}/install`, {
    name: `Real Docker ${templateId}`, slug, environment_id: environmentId, deploy: false,
  });
  const projectId = installed.project.id;
  stack.projects.set(slug, projectId);
  const tags = imageTags();
  for (const [, ref] of String(installed.project.source.compose_yaml).matchAll(/^\s*image:\s*(\S+)\s*$/gm)) {
    if (!stack.images.has(ref)) stack.images.set(ref, tags.get(ref) ?? null);
  }
  // What the dashboard's install does: deploy with fresh images.
  const { json: first } = await api('POST', `/api/compose-projects/${projectId}/deploy`, { pull_images: true });
  await waitForDeployment(projectId, first.id);
  const running = await services(projectId);
  log(`${templateId}: deployed (${running.map((s) => `${s.name}=${s.state}${s.health ? `/${s.health}` : ''}`).join(', ')})`);

  let proof = await check.prove(await appUrl(projectId, check.service, check.port));
  log(`${templateId}: ${proof}`);

  const logs = (await api('GET', `/api/compose-projects/${projectId}/logs?tail=20`)).json.logs;
  if (!String(logs).includes(`${check.service}-1`)) throw new Error('container logs do not name the service');

  const { json: again } = await api('POST', `/api/compose-projects/${projectId}/deploy`, {});
  await waitForDeployment(projectId, again.id);
  const url = await appUrl(projectId, check.service, check.port);
  proof = await (check.afterRedeploy ?? check.prove).call(check, url);
  log(`${templateId}: after a redeployment: ${proof}`);

  await api('DELETE', `/api/compose-projects/${projectId}?volumes=true`);
  stack.projects.delete(slug);
  const left = leftovers(slug);
  const remaining = Object.entries(left).filter(([, value]) => value).map(([kind]) => kind);
  if (remaining.length) throw new Error(`removal left ${remaining.join(', ')} behind`);
  log(`${templateId}: removed (containers, network, volumes and release tags are gone)`);

  if (!keepImages) restoreImages();
  return proof;
}

async function main() {
  await startStack();
  const catalogue = (await api('GET', '/api/templates')).json.map((t) => t.id);
  const wanted = flag('--all') ? catalogue : (requested.length ? requested : ['wordpress']);
  for (const id of wanted) {
    if (!catalogue.includes(id)) throw new Error(`unknown template ${id}; the catalogue has ${catalogue.join(', ')}`);
  }
  const { json: project } = await api('POST', '/api/projects', { name: `Real Docker ${RUN}`, slug: `rd${RUN}` });
  const environmentId = project.environments[0].id;

  const results = [];
  for (const id of wanted) {
    try {
      results.push({ id, ok: true, detail: await runTemplate(id, environmentId) });
    } catch (error) {
      results.push({ id, ok: false, detail: String(error?.message ?? error) });
      log(`${id}: FAILED: ${error?.message ?? error}`);
      const slug = `rd${RUN}-${id}`;
      const projectId = stack.projects.get(slug);
      if (projectId) {
        try { await api('DELETE', `/api/compose-projects/${projectId}?volumes=true`); stack.projects.delete(slug); } catch { /* forced below */ }
      }
      forceRemove(slug);
      if (!keepImages) restoreImages();
    }
  }
  console.log('');
  for (const result of results) console.log(`${result.ok ? 'PASS' : 'FAIL'}  ${result.id}  ${result.detail.split('\n')[0]}`);
  return results.every((r) => r.ok);
}

let stopping = false;
async function finish(code) {
  if (stopping) return;
  stopping = true;
  await stopStack().catch((error) => console.error(`[real-docker] cleanup: ${error.message}`));
  process.exit(code);
}
process.on('SIGINT', () => finish(130));
process.on('SIGTERM', () => finish(143));

try {
  const ok = await main();
  await finish(ok ? 0 : 1);
} catch (error) {
  console.error(`[real-docker] ${error?.message ?? error}`);
  if (stack.dir && existsSync(join(stack.dir, 'api.log'))) {
    console.error(readFileSync(join(stack.dir, 'api.log'), 'utf8').split('\n').slice(-5).join('\n'));
  }
  await finish(1);
}
