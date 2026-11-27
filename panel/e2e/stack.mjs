// Starts the whole panel for end-to-end tests without root, Nginx or Docker:
// a throwaway PostgreSQL cluster, the mock agent (`scontrol-mock-agent`),
// panel-api, and the built frontend behind `vite preview` (which proxies
// `/api` like Nginx does). Everything lives in a temporary directory.
//
// `node stack.mjs` keeps the stack running until Ctrl-C (for manual browser
// testing); Playwright uses `startStack` through global-setup.
import { spawn, execFileSync } from 'node:child_process';
import { existsSync, mkdtempSync, rmSync, writeFileSync, openSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import net from 'node:net';

const here = dirname(fileURLToPath(import.meta.url));
const panel = resolve(here, '..');

export const PORTS = {
  postgres: Number(process.env.E2E_PG_PORT ?? 55432),
  api: Number(process.env.E2E_API_PORT ?? 18080),
  web: Number(process.env.E2E_WEB_PORT ?? 14173),
};
export const BASE_URL = `http://localhost:${PORTS.web}`;
export const ADMIN = { username: 'admin', email: 'admin@example.test', password: 'correct-horse-battery' };

function findBinary(name) {
  for (const profile of ['debug', 'release']) {
    const candidate = join(panel, 'target', profile, name);
    if (existsSync(candidate)) return candidate;
  }
  throw new Error(
    `${name} is not built. Run in panel/: SQLX_OFFLINE=true cargo build -p panel-api && ` +
    'cargo build -p panel-core --features test-utils --bin scontrol-mock-agent',
  );
}

function postgresBin(name) {
  if (process.env.E2E_PG_BIN) return join(process.env.E2E_PG_BIN, name);
  for (const dir of ['/usr/bin', '/usr/libexec/postgresql', '/usr/local/bin']) {
    if (existsSync(join(dir, name))) return join(dir, name);
  }
  // Debian/Ubuntu keep the server binaries out of PATH.
  try {
    const found = execFileSync('sh', ['-c', `ls /usr/lib/postgresql/*/bin/${name} | tail -1`]).toString().trim();
    if (found) return found;
  } catch { /* fall through */ }
  return name;
}

function waitForPort(port, label, timeoutMs = 60_000) {
  const deadline = Date.now() + timeoutMs;
  return new Promise((resolvePort, reject) => {
    const attempt = () => {
      const socket = net.connect(port, '127.0.0.1');
      socket.once('connect', () => { socket.destroy(); resolvePort(); });
      socket.once('error', () => {
        socket.destroy();
        if (Date.now() > deadline) reject(new Error(`${label} did not start on port ${port}`));
        else setTimeout(attempt, 200);
      });
    };
    attempt();
  });
}

export async function startStack() {
  const dir = mkdtempSync(join(tmpdir(), 'scontrol-e2e-'));
  const children = [];
  const run = (label, command, args, env = {}, cwd = panel) => {
    const log = openSync(join(dir, `${label}.log`), 'a');
    const child = spawn(command, args, { cwd, env: { ...process.env, ...env }, stdio: ['ignore', log, log] });
    child.on('exit', (code) => { if (code) console.error(`[e2e] ${label} exited with ${code}; see ${join(dir, `${label}.log`)}`); });
    children.push(child);
    return child;
  };
  const stop = async () => {
    for (const child of children.reverse()) child.kill('SIGTERM');
    await new Promise((done) => setTimeout(done, 500));
    for (const child of children) child.kill('SIGKILL');
    if (!process.env.E2E_KEEP_DIR) rmSync(dir, { recursive: true, force: true });
  };

  try {
    // E2E_DATABASE_URL: use an existing, empty PostgreSQL database (for
    // example one in Docker) on a machine without the server binaries.
    let databaseUrl = process.env.E2E_DATABASE_URL;
    if (!databaseUrl) {
      const data = join(dir, 'pg');
      const passwordFile = join(dir, 'pgpass');
      writeFileSync(passwordFile, 'scontrol\n', { mode: 0o600 });
      execFileSync(postgresBin('initdb'), ['-D', data, '-U', 'scontrol', '-A', 'scram-sha-256', '--pwfile', passwordFile, '-E', 'UTF8'], { stdio: 'ignore' });
      run('postgres', postgresBin('postgres'), ['-D', data, '-p', String(PORTS.postgres), '-c', 'listen_addresses=127.0.0.1', '-k', dir, '-c', 'fsync=off']);
      await waitForPort(PORTS.postgres, 'PostgreSQL');
      databaseUrl = `postgres://scontrol:scontrol@127.0.0.1:${PORTS.postgres}/postgres`;
    }

    const socket = join(dir, 'agent.sock');
    run('agent', findBinary('scontrol-mock-agent'), [socket]);

    // Fixed keys keep sessions valid if panel-api is restarted by hand.
    const key = Buffer.alloc(32, 7).toString('base64');
    run('api', findBinary('panel-api'), [], {
      DATABASE_URL: databaseUrl,
      PANEL_API_BIND: `127.0.0.1:${PORTS.api}`,
      PANEL_AGENT_SOCKET: socket,
      PANEL_COOKIE_KEY: key,
      PANEL_SECRET_KEY: Buffer.alloc(32, 9).toString('base64'),
    });
    await waitForPort(PORTS.api, 'panel-api');

    if (!existsSync(join(panel, 'frontend', 'dist', 'index.html'))) {
      throw new Error('frontend is not built. Run in panel/frontend: npm install && npm run build');
    }
    run('web', join(panel, 'frontend', 'node_modules', '.bin', 'vite'),
      ['preview', '--port', String(PORTS.web), '--strictPort', '--host', '127.0.0.1'],
      { SCONTROL_API: `http://127.0.0.1:${PORTS.api}` }, join(panel, 'frontend'));
    await waitForPort(PORTS.web, 'frontend');

    const bootstrap = await fetch(`http://127.0.0.1:${PORTS.api}/api/auth/bootstrap-admin`, {
      method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify(ADMIN),
    });
    if (!bootstrap.ok) throw new Error(`bootstrap-admin failed: ${bootstrap.status} ${await bootstrap.text()}`);
    return { dir, stop, databaseUrl };
  } catch (error) {
    await stop();
    throw error;
  }
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  const stack = await startStack();
  console.log(`scontrol is running at ${BASE_URL}`);
  console.log(`  admin login: ${ADMIN.username} / ${ADMIN.password}`);
  console.log(`  API: http://127.0.0.1:${PORTS.api}   logs: ${stack.dir}`);
  const shutdown = async () => { await stack.stop(); process.exit(0); };
  process.on('SIGINT', shutdown);
  process.on('SIGTERM', shutdown);
}
