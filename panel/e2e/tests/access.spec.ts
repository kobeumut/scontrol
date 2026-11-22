import { APIRequestContext, expect, Page, request as pwRequest, test } from '@playwright/test';
import { execFileSync } from 'node:child_process';
import { createHmac } from 'node:crypto';
import { mkdtempSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { ADMIN, PORTS } from '../stack.mjs';
import { openTab, signIn, unique, watchForFailures } from './helpers';

const API = `http://127.0.0.1:${PORTS.api}`;
const PASSWORD = 'a-long-test-password-1';
const AGENT_BIN = resolve(dirname(fileURLToPath(import.meta.url)), '../../target/debug/panel-agent');

/** A logged-in API client (cookie session). */
async function apiAs(user: { username: string; password: string }, extra?: { totp?: string }): Promise<APIRequestContext> {
  const ctx = await pwRequest.newContext({ baseURL: API });
  const res = await ctx.post('/api/auth/login', { data: { username: user.username, password: user.password, totp_code: extra?.totp } });
  expect(res.status(), 'login').toBe(200);
  return ctx;
}

async function createUser(admin: APIRequestContext, prefix: string) {
  const username = unique(prefix).replace(/-/g, '_');
  const res = await admin.post('/api/admin/users', { data: { username, email: `${username}@example.test`, password: PASSWORD, is_admin: false } });
  expect(res.status(), await res.text()).toBe(200);
  return { username, password: PASSWORD, id: (await res.json()).id as string };
}

async function roleId(admin: APIRequestContext, name: string): Promise<string> {
  const roles = await (await admin.get('/api/access/roles')).json();
  return roles.find((r: { name: string }) => r.name === name).id;
}

/** Runs the real `panel-agent remote-init` for a CSR; the key stays in a temp dir. */
function makeCsr(host: string): string {
  const dir = join(mkdtempSync(join(tmpdir(), 'e2e-csr-')), 'remote');
  const out = execFileSync(AGENT_BIN, ['remote-init', '--host', host, '--dir', dir], { encoding: 'utf8' });
  return out.slice(out.indexOf('-----BEGIN CERTIFICATE REQUEST-----'), out.indexOf('-----END CERTIFICATE REQUEST-----') + '-----END CERTIFICATE REQUEST-----'.length);
}

function base32Decode(input: string): Buffer {
  const alphabet = 'ABCDEFGHIJKLMNOPQRSTUVWXYZ234567';
  let bits = '';
  for (const c of input.replace(/=+$/, '').toUpperCase()) bits += alphabet.indexOf(c).toString(2).padStart(5, '0');
  const bytes: number[] = [];
  for (let i = 0; i + 8 <= bits.length; i += 8) bytes.push(parseInt(bits.slice(i, i + 8), 2));
  return Buffer.from(bytes);
}

function totpNow(secretBase32: string): string {
  const counter = Buffer.alloc(8);
  counter.writeBigUInt64BE(BigInt(Math.floor(Date.now() / 1000 / 30)));
  const hmac = createHmac('sha1', base32Decode(secretBase32)).update(counter).digest();
  const offset = hmac[hmac.length - 1] & 0xf;
  const code = (hmac.readUInt32BE(offset) & 0x7fffffff) % 1_000_000;
  return code.toString().padStart(6, '0');
}

let admin: APIRequestContext;
test.beforeAll(async () => { admin = await apiAs(ADMIN); });
test.afterAll(async () => { await admin.dispose(); });

function serverRow(page: Page, name: string) {
  return page.getByRole('row').filter({ has: page.getByText(name, { exact: true }) });
}
function rowByText(page: Page, text: string) {
  return page.getByRole('row').filter({ hasText: text });
}

async function openAdd(page: Page) {
  await openTab(page, 'Servers');
  await page.getByRole('button', { name: 'Add server' }).click();
}

test('adds an SSH-socket server, drains it, sets capacity, checks it and removes it', async ({ page }) => {
  const failures = watchForFailures(page);
  const name = unique('ssh');
  await signIn(page);
  await openAdd(page);
  await page.getByRole('button', { name: 'SSH-forwarded socket' }).click();
  await page.getByLabel('Name').fill(name);
  await page.getByLabel('Forwarded socket').fill(`/run/scontrol/remote/${name}.sock`);
  await page.getByRole('button', { name: 'Add server' }).last().click();

  const row = serverRow(page, name);
  await expect(row).toBeVisible();
  await expect(row).toContainText('SSH-forwarded socket');
  await expect(row).toContainText(`/run/scontrol/remote/${name}.sock`);

  // Nothing listens on the socket, so a check reports the server as unreachable.
  await row.getByRole('button', { name: 'check' }).click();
  await expect(page.getByRole('status')).toContainText(`${name} did not answer`);

  await row.getByRole('button', { name: 'drain' }).click();
  await expect(row.getByText('draining', { exact: true })).toBeVisible();
  await expect(row.getByRole('button', { name: 'undrain' })).toBeVisible();
  await row.getByRole('button', { name: 'undrain' }).click();
  await expect(row.getByText('draining', { exact: true })).toHaveCount(0);

  page.once('dialog', (d) => d.accept('7'));
  await row.getByRole('button', { name: 'capacity' }).click();
  await expect(row.getByRole('cell', { name: '0 / 7' })).toBeVisible();

  page.once('dialog', (d) => d.accept('')); // empty means no limit
  await row.getByRole('button', { name: 'capacity' }).click();
  await expect(row.getByRole('cell', { name: '0 / 7' })).toHaveCount(0);

  page.once('dialog', (d) => d.accept('-3'));
  await row.getByRole('button', { name: 'capacity' }).click();
  await expect(page.getByRole('alert')).toHaveText('Enter a whole number.');

  page.once('dialog', (d) => d.accept());
  await row.getByRole('button', { name: 'remove' }).click();
  await expect(serverRow(page, name)).toHaveCount(0);
  expect(failures).toEqual([]);
});

test('server form validation: bad socket path, reserved name, bad host, bad CSR', async ({ page }) => {
  const name = unique('val');
  await signIn(page);
  await openAdd(page);
  await page.getByRole('button', { name: 'SSH-forwarded socket' }).click();
  await page.getByLabel('Name').fill(name);
  await page.getByLabel('Forwarded socket').fill('/tmp/not-allowed.sock');
  await page.getByRole('button', { name: 'Add server' }).last().click();
  await expect(page.getByRole('alert')).toContainText('the forwarded socket must be');

  await page.getByLabel('Name').fill('local');
  await page.getByLabel('Forwarded socket').fill('/run/scontrol/remote/local.sock');
  await page.getByRole('button', { name: 'Add server' }).last().click();
  await expect(page.getByRole('alert')).toContainText('server names are 2–40 lowercase letters');

  // mTLS with garbage CSR.
  await page.getByRole('button', { name: 'Mutual TLS' }).click();
  await page.getByLabel('Name').fill(name);
  await page.getByLabel('Address (host:port)').fill('not a host:7843');
  await page.getByLabel('Certificate signing request').fill('garbage');
  await page.getByRole('button', { name: 'Sign and add' }).click();
  await expect(page.getByRole('alert')).toContainText('invalid host in address');
  await page.getByLabel('Address (host:port)').fill(`${name}.example.com:7843`);
  await page.getByRole('button', { name: 'Sign and add' }).click();
  await expect(page.getByRole('alert')).toContainText('not a valid certificate signing request');
  // Nothing half-enrolled.
  await page.getByRole('button', { name: 'Cancel' }).click();
  await expect(serverRow(page, name)).toHaveCount(0);

});

test('enrolls an mTLS server from a CSR, shows the certificate, rotates it and removes it', async ({ page }) => {
  const failures = watchForFailures(page);
  const name = unique('tls');
  const host = `${name}.example.com`;
  await signIn(page);
  await openAdd(page);
  await page.getByLabel('Name').fill(name);
  await page.getByLabel('Address (host:port)').fill(`${host}:7843`);
  await expect(page.getByText(`panel-agent remote-init --host ${host}`)).toBeVisible();
  await page.getByLabel('Max services').fill('3');
  await page.getByLabel('Certificate signing request').fill(makeCsr(host));
  await page.getByRole('button', { name: 'Sign and add' }).click();

  // The signed bundle is shown once, ready to install.
  await expect(page.getByText(`Install on ${name}`)).toBeVisible();
  await expect(page.getByLabel('agent.pem')).toHaveValue(/BEGIN CERTIFICATE/);
  await expect(page.getByLabel('ca.pem')).toHaveValue(/BEGIN CERTIFICATE/);
  const firstCert = await page.getByLabel('agent.pem').inputValue();
  await page.getByRole('button', { name: 'Done' }).click();

  const row = serverRow(page, name);
  await expect(row).toContainText('mutual TLS');
  await expect(row).toContainText('0 / 3');
  await expect(row).toContainText('expires');
  await expect(row).not.toContainText('rotation pending');

  // "certificate" shows the stored certificate again.
  await row.getByRole('button', { name: 'certificate' }).click();
  await expect(page.getByLabel('agent.pem')).toHaveValue(firstCert);
  await page.getByRole('button', { name: 'Done' }).click();

  // Rotation: a garbage CSR is refused, a real one is signed and leaves a pending rotation.
  await row.getByRole('button', { name: 'rotate' }).click();
  await page.getByLabel('Certificate signing request').fill('nope');
  await page.getByRole('button', { name: 'Sign new certificate' }).click();
  await expect(page.getByRole('alert')).toContainText('not a valid certificate signing request');
  await page.getByLabel('Certificate signing request').fill(makeCsr(host));
  await page.getByRole('button', { name: 'Sign new certificate' }).click();
  await expect(page.getByText(`Install on ${name} (rotated)`)).toBeVisible();
  const rotated = await page.getByLabel('agent.pem').inputValue();
  expect(rotated).not.toEqual(firstCert);
  await page.getByRole('button', { name: 'Done' }).click();
  await expect(serverRow(page, name)).toContainText('rotation pending');

  page.once('dialog', (d) => d.accept());
  await serverRow(page, name).getByRole('button', { name: 'remove' }).click();
  await expect(serverRow(page, name)).toHaveCount(0);
  expect(failures).toEqual([]);
});

test('the local server has no remove button and the API refuses to remove or rename it', async ({ page }) => {
  await signIn(page);
  await openTab(page, 'Servers');
  const local = page.getByRole('row').filter({ hasText: 'panel host' });
  await expect(local).toBeVisible();
  await expect(local.getByRole('button', { name: 'remove' })).toHaveCount(0);
  await expect(local.getByRole('button', { name: 'certificate' })).toHaveCount(0);
  const servers = await (await admin.get('/api/servers')).json();
  const id = servers.find((s: { transport: string }) => s.transport === 'local').id;
  expect((await admin.delete(`/api/servers/${id}`)).status()).toBe(400);
  expect((await admin.put(`/api/servers/${id}`, { data: { name: 'renamed', draining: false } })).status()).toBe(400);
});

test('creates a custom role, edits it, binds it at global and project scope, then removes both', async ({ page }) => {
  const failures = watchForFailures(page);
  const user = await createUser(admin, 'acc');
  const role = unique('role');
  const slug = unique('proj');
  const created = await admin.post('/api/projects', { data: { name: slug, slug, environment_name: 'staging' } });
  expect(created.status(), await created.text()).toBeLessThan(300);
  const projectId = (await created.json()).id ?? (await created.json()).project?.id;

  await signIn(page);
  await openTab(page, 'Access');
  await page.getByRole('button', { name: 'New role' }).click();
  // A role needs a name and at least one permission.
  await expect(page.getByRole('button', { name: 'Create role' })).toBeDisabled();
  await page.getByRole('textbox', { name: 'Name' }).fill(role);
  await page.getByRole('textbox', { name: 'Description' }).fill('e2e role');
  await page.getByLabel(/projects:read/).check();
  await page.getByRole('button', { name: 'Create role' }).click();
  const roleRow = rowByText(page, role);
  await expect(roleRow).toContainText('projects:read');

  await roleRow.getByRole('button', { name: 'edit' }).click();
  await page.getByLabel(/services:read/).check();
  await page.getByRole('button', { name: 'Save role' }).click();
  await expect(rowByText(page, role)).toContainText('projects:read, services:read');

  // Duplicate role name.
  await page.getByRole('button', { name: 'New role' }).click();
  await page.getByRole('textbox', { name: 'Name' }).fill(role);
  await page.getByLabel(/projects:read/).check();
  await page.getByRole('button', { name: 'Create role' }).click();
  await expect(page.getByRole('alert')).toBeVisible();
  await page.getByRole('button', { name: 'Cancel' }).click();

  // Global binding.
  const bindingForm = page.locator('form.card').first();
  await bindingForm.locator('select').nth(0).selectOption({ label: user.username });
  await bindingForm.locator('select').nth(1).selectOption({ label: role });
  await bindingForm.getByRole('button', { name: 'Add binding' }).click();
  const globalRow = rowByText(page, user.username).filter({ hasText: 'global' });
  await expect(globalRow).toContainText(role);

  // Project-scoped binding.
  await bindingForm.locator('select').nth(0).selectOption({ label: user.username });
  await bindingForm.locator('select').nth(1).selectOption({ label: 'viewer' });
  await bindingForm.locator('select').nth(2).selectOption({ label: slug });
  await bindingForm.getByRole('button', { name: 'Add binding' }).click();
  await expect(rowByText(page, user.username).filter({ hasText: slug })).toContainText('viewer');
  // Whole-project and environment-scoped.
  await bindingForm.locator('select').nth(3).selectOption({ label: 'staging' });
  await bindingForm.locator('select').nth(1).selectOption({ label: 'deployer' });
  await bindingForm.getByRole('button', { name: 'Add binding' }).click();
  await expect(rowByText(page, user.username).filter({ hasText: `${slug} / staging` })).toContainText('deployer');

  // Remove bindings (confirm dialog).
  for (const scope of [`${slug} / staging`, 'viewer', 'global']) {
    page.once('dialog', (d) => d.accept());
    await rowByText(page, user.username).filter({ hasText: scope }).first().getByRole('button', { name: 'remove' }).click();
    await expect(rowByText(page, user.username).filter({ hasText: scope })).toHaveCount(0);
  }

  page.once('dialog', (d) => d.accept());
  await rowByText(page, role).getByRole('button', { name: 'delete' }).click();
  await expect(rowByText(page, role)).toHaveCount(0);
  await admin.delete(`/api/projects/${projectId}`);
  expect(failures).toEqual([]);
});

test('a non-admin user sees only what their role allows and is refused clearly otherwise', async ({ page }) => {
  const failures = watchForFailures(page);
  const user = await createUser(admin, 'viewer');
  const bind = await admin.post('/api/access/bindings', { data: { user_id: user.id, role_id: await roleId(admin, 'viewer'), scope: { kind: 'global' } } });
  expect(bind.status(), await bind.text()).toBeLessThan(300);

  await signIn(page, user);
  const nav = page.getByRole('navigation', { name: 'Primary content' });
  await expect(nav.getByRole('button', { name: 'API keys' })).toBeVisible();
  await expect(nav.getByRole('button', { name: 'Access', exact: true })).toHaveCount(0);
  await expect(nav.getByRole('button', { name: 'Servers', exact: true })).toHaveCount(0);

  // Every visible tab still renders without an error state.
  for (const tab of ['Projects & environments', 'Compose projects', 'API keys']) {
    await openTab(page, tab);
    await expect(page.getByText('Loading…')).toHaveCount(0);
  }

  // The API refuses what the UI hides, with 403 and a message.
  const api = await apiAs(user);
  for (const [method, path] of [['get', '/api/servers'], ['get', '/api/access/bindings'], ['get', '/api/access/users'], ['get', '/api/admin/users']] as const) {
    const res = await api[method](path);
    expect(res.status(), `${method} ${path}`).toBe(403);
    expect((await res.json()).error).toBeTruthy();
  }
  const noCreate = await api.post('/api/access/roles', { data: { name: unique('nope'), description: '', permissions: ['projects:read'] } });
  expect(noCreate.status()).toBe(403);
  // A viewer cannot mint a key with more than they hold.
  const tooMuch = await api.post('/api/api-keys', { data: { name: 'x', grants: [{ permission: 'services:deploy', scope: { kind: 'global' } }], expires_in_days: 1 } });
  expect(tooMuch.status()).toBe(400);
  expect((await tooMuch.json()).error).toContain('you do not hold services:deploy');

  // In the UI the same refusal is shown as a message, not a crash.
  await openTab(page, 'API keys');
  await page.getByRole('button', { name: 'New key' }).click();
  await page.getByRole('textbox', { name: 'Name' }).fill('too-much');
  await page.locator('fieldset select').first().selectOption('services:deploy');
  await page.getByRole('button', { name: 'Add permission' }).click();
  await page.getByRole('button', { name: 'Create key' }).click();
  await expect(page.getByRole('alert')).toContainText('you do not hold services:deploy');
  await api.dispose();
  expect(failures.filter((f) => !f.startsWith('403'))).toEqual([]);
});

test('creates an API key in the UI, uses its token, and loses access when it is revoked', async ({ page }) => {
  const failures = watchForFailures(page);
  const keyName = unique('key');
  await signIn(page);
  await openTab(page, 'API keys');
  await page.getByRole('button', { name: 'New key' }).click();
  await expect(page.getByRole('button', { name: 'Create key' })).toBeDisabled();
  await page.getByRole('textbox', { name: 'Name' }).fill(keyName);
  await page.locator('fieldset select').first().selectOption('projects:read');
  await page.getByRole('button', { name: 'Add permission' }).click();
  await expect(page.locator('fieldset table').getByText('projects:read')).toBeVisible();
  await page.getByRole('button', { name: 'Create key' }).click();

  await expect(page.getByText(`Copy the token for ${keyName} now`)).toBeVisible();
  const token = (await page.locator('.secret').innerText()).trim();
  expect(token).toMatch(/^sck_/);

  const row = rowByText(page, keyName);
  await expect(row).toContainText('projects:read');
  await expect(row).toContainText('global');

  const bearer = await pwRequest.newContext({ baseURL: API, extraHTTPHeaders: { Authorization: `Bearer ${token}` } });
  expect((await bearer.get('/api/projects')).status()).toBe(200);
  // Limited to its grants.
  expect((await bearer.get('/api/servers')).status()).toBe(403);
  expect((await bearer.get('/api/access/bindings')).status()).toBe(403);
  expect((await bearer.get('/api/api-keys')).status()).toBe(403);
  expect((await bearer.post('/api/projects', { data: { name: 'zz', slug: 'zzz-key' } })).status()).toBe(403);

  // Token is shown once: dismissing and reloading never shows it again.
  await page.getByRole('button', { name: 'Done' }).click();
  await expect(page.getByText(token)).toHaveCount(0);
  await page.reload();
  await openTab(page, 'API keys');
  await expect(page.getByText(token)).toHaveCount(0);

  page.once('dialog', (d) => d.accept());
  await rowByText(page, keyName).getByRole('button', { name: 'revoke' }).click();
  await expect(rowByText(page, keyName).getByText('revoked')).toBeVisible();
  await expect(rowByText(page, keyName).getByRole('button', { name: 'revoke' })).toHaveCount(0);
  expect((await bearer.get('/api/projects')).status()).toBe(401);
  await bearer.dispose();
  expect(failures).toEqual([]);
});

test('API key form: non-scopable permission has no scope, scoped grant to a project, duplicates, removal', async ({ page }) => {
  const slug = unique('kproj');
  const created = await admin.post('/api/projects', { data: { name: slug, slug, environment_name: 'prod' } });
  expect(created.status()).toBeLessThan(300);
  const projectId = (await created.json()).id;
  await signIn(page);
  await openTab(page, 'API keys');
  await page.getByRole('button', { name: 'New key' }).click();
  await page.locator('fieldset select').first().selectOption('projects:create');
  await expect(page.getByText('projects:create exists only globally.')).toBeVisible();
  await page.locator('fieldset select').first().selectOption('services:deploy');
  await page.locator('fieldset select').nth(1).selectOption({ label: slug });
  await page.locator('fieldset select').nth(2).selectOption({ label: 'prod' });
  await page.getByRole('button', { name: 'Add permission' }).click();
  await page.getByRole('button', { name: 'Add permission' }).click(); // duplicate is ignored
  const grants = page.locator('fieldset').getByRole('row');
  await expect(grants).toHaveCount(1);
  await expect(grants.first()).toContainText(`${slug} / prod`);
  await grants.first().getByRole('button', { name: 'remove' }).click();
  await expect(grants).toHaveCount(0);
  await expect(page.getByRole('button', { name: 'Create key' })).toBeDisabled();
  await page.getByRole('button', { name: 'Cancel' }).click();
  await admin.delete(`/api/projects/${projectId}`);
});

test('an admin cannot be used to mint a key with access:manage, and the catalogue hides it', async ({ page }) => {
  await signIn(page);
  await openTab(page, 'API keys');
  await page.getByRole('button', { name: 'New key' }).click();
  const options = await page.locator('fieldset select').first().locator('option').allTextContents();
  expect(options.some((o) => o.startsWith('access:manage'))).toBe(false);
  expect(options.some((o) => o.startsWith('system:admin'))).toBe(false);
  const res = await admin.post('/api/api-keys', { data: { name: 'x', grants: [{ permission: 'access:manage', scope: { kind: 'global' } }] } });
  expect(res.status()).toBe(400);
});

async function newUserForm(page: Page) {
  await openTab(page, 'Access');
  await expect(page.getByRole('heading', { name: 'Users', exact: true })).toBeVisible();
  await page.getByRole('button', { name: 'New user' }).click();
}

test('user management has a UI: create a user, validation, list and binding select', async ({ page }) => {
  const failures = watchForFailures(page);
  const username = unique('ui').replace(/-/g, '_');
  const posts: string[] = [];
  page.on('request', (r) => { if (r.method() === 'POST' && r.url().endsWith('/api/admin/users')) posts.push(r.url()); });

  await signIn(page);
  await newUserForm(page);
  const form = page.locator('form.card').filter({ has: page.getByRole('button', { name: 'Create user' }) });
  const field = (label: string) => form.getByLabel(label);

  // Browser validation blocks bad input before any request is sent.
  await field('Username').fill('Bad Name');
  await field('Email').fill(`${username}@example.test`);
  await field('Password').fill('short');
  await form.getByRole('button', { name: 'Create user' }).click();
  expect(await field('Username').evaluate((el: HTMLInputElement) => el.validity.patternMismatch)).toBe(true);
  expect(await field('Password').evaluate((el: HTMLInputElement) => el.validity.tooShort)).toBe(true);
  await field('Username').fill(username);
  await form.getByRole('button', { name: 'Create user' }).click();
  expect(await field('Password').evaluate((el: HTMLInputElement) => el.validity.tooShort)).toBe(true);
  await field('Password').fill(PASSWORD);
  await field('Email').fill('not-an-email');
  await form.getByRole('button', { name: 'Create user' }).click();
  expect(await field('Email').evaluate((el: HTMLInputElement) => el.validity.typeMismatch)).toBe(true);
  expect(posts).toEqual([]);

  await field('Email').fill(`${username}@example.test`);
  await form.getByRole('button', { name: 'Create user' }).click();
  const row = rowByText(page, username);
  await expect(row).toBeVisible();
  await expect(row).toContainText(`${username}@example.test`);
  await expect(row).toContainText('user');
  await expect(page.getByRole('button', { name: 'Create user' })).toHaveCount(0);
  expect(posts).toHaveLength(1);

  // The new user is offered in the role-binding form right away.
  await expect(page.getByLabel('User').locator('option', { hasText: username })).toHaveCount(1);

  // And can sign in with the password.
  const api = await apiAs({ username, password: PASSWORD });
  await api.dispose();
  expect(failures).toEqual([]);
});

test('creating a user with a taken username is a clear conflict, not a 500', async ({ page }) => {
  const user = await createUser(admin, 'dup');
  const res = await admin.post('/api/admin/users', { data: { username: user.username, email: `other-${user.username}@example.test`, password: PASSWORD, is_admin: false } });
  expect(res.status()).toBe(409);
  expect((await res.json()).error).toMatch(/already|taken|exists/i);

  await signIn(page);
  await newUserForm(page);
  await page.getByLabel('Username').fill(user.username);
  await page.getByLabel('Email').fill(`third-${user.username}@example.test`);
  await page.getByLabel('Password', { exact: false }).first().fill(PASSWORD);
  await page.getByRole('button', { name: 'Create user' }).click();
  await expect(page.getByRole('alert')).toContainText(/already|taken|exists/i);
});

test('non-admin users: Account tab yes, Users panel no (even with the admin role bound)', async ({ page }) => {
  const user = await createUser(admin, 'plain');
  await signIn(page, user);
  const nav = page.getByRole('navigation', { name: 'Primary content' });
  await expect(nav.getByRole('button', { name: 'Account', exact: true })).toBeVisible();
  await expect(nav.getByRole('button', { name: 'Access', exact: true })).toHaveCount(0);
  await openTab(page, 'Account');
  await expect(page.getByRole('row').filter({ hasText: 'Username' })).toContainText(user.username);
  await expect(page.getByRole('row').filter({ hasText: 'Role' })).toContainText('user');
  await expect(page.getByRole('heading', { name: 'Users', exact: true })).toHaveCount(0);
  await page.getByRole('button', { name: 'sign out' }).click();

  // access:manage through a role (not an installation admin) shows Access, but never the Users panel.
  const bind = await admin.post('/api/access/bindings', { data: { user_id: user.id, role_id: await roleId(admin, 'admin'), scope: { kind: 'global' } } });
  expect(bind.status(), await bind.text()).toBeLessThan(300);
  await signIn(page, user);
  await openTab(page, 'Access');
  await expect(page.getByRole('button', { name: 'New role' })).toBeVisible();
  await expect(page.getByRole('heading', { name: 'Users', exact: true })).toHaveCount(0);
  await expect(page.getByRole('button', { name: 'New user' })).toHaveCount(0);
  await expect(page.getByRole('navigation', { name: 'Primary content' }).getByRole('button', { name: 'Account', exact: true })).toBeVisible();
});

test('TOTP: enrollment is reachable from the UI; an admin resets it from the Users panel', async ({ page, browser }) => {
  const failures = watchForFailures(page);
  const user = await createUser(admin, 'totp');
  await signIn(page, user);
  await openTab(page, 'Account');
  await expect(page.getByRole('row').filter({ hasText: 'Username' })).toContainText(user.username);
  await expect(page.getByRole('row').filter({ hasText: 'Email' })).toContainText(`${user.username}@example.test`);
  const state = page.getByRole('row').filter({ hasText: 'Two-factor authentication' }).getByRole('cell');
  await expect(state).toHaveText('not enabled');

  await page.getByRole('button', { name: 'Set up 2FA' }).click();
  const secret = (await page.getByTestId('totp-secret').innerText()).trim();
  expect(secret).toMatch(/^[A-Z2-7]{16,}$/);
  const code = page.getByLabel('6-digit code');
  // Wrong code: rejected with a message, 2FA stays off.
  const wrong = totpNow(secret) === '000000' ? '000001' : '000000';
  await code.fill(wrong);
  await page.getByRole('button', { name: 'Enable 2FA' }).click();
  await expect(page.getByRole('alert')).toBeVisible();
  await expect(page.getByRole('alert')).not.toBeEmpty();
  await expect(page.getByTestId('totp-secret')).toBeVisible();
  // A malformed code never reaches the server.
  await code.fill('12ab');
  expect(await code.evaluate((el: HTMLInputElement) => el.validity.patternMismatch)).toBe(true);

  // Right code enables it.
  await code.fill(totpNow(secret));
  await page.getByRole('button', { name: 'Enable 2FA' }).click();
  await expect(state).toHaveText('enabled');
  await expect(page.getByRole('button', { name: 'Set up 2FA' })).toHaveCount(0);
  await expect(page.getByTestId('totp-secret')).toHaveCount(0);

  // Sign in now needs the code.
  await page.getByRole('button', { name: 'sign out' }).click();
  await expect(page.getByRole('heading', { name: 'Sign in' })).toBeVisible();
  await page.getByLabel('Username').fill(user.username);
  await page.getByLabel('Password').fill(user.password);
  await page.getByRole('button', { name: 'Sign in' }).click();
  await expect(page.getByRole('alert')).toHaveText('Wrong username, password or 2FA code.');
  await page.getByLabel(/2FA code/).fill(totpNow(secret));
  await page.getByRole('button', { name: 'Sign in' }).click();
  await expect(page.getByRole('navigation', { name: 'Primary content' })).toBeVisible();
  await page.getByRole('button', { name: 'sign out' }).click();

  // An admin resets it in the Users panel.
  const adminPage = await (await browser.newContext()).newPage();
  await signIn(adminPage);
  await openTab(adminPage, 'Access');
  const row = rowByText(adminPage, user.username);
  await expect(row).toContainText('enabled');
  adminPage.once('dialog', (d) => d.accept());
  await row.getByRole('button', { name: 'reset 2FA' }).click();
  await expect(row.getByRole('button', { name: 'reset 2FA' })).toHaveCount(0);
  await adminPage.context().close();

  // Password alone works again, and the Account tab offers setup again.
  await signIn(page, user);
  await openTab(page, 'Account');
  await expect(page.getByRole('row').filter({ hasText: 'Two-factor authentication' }).getByRole('cell')).toHaveText('not enabled');
  await expect(page.getByRole('button', { name: 'Set up 2FA' })).toBeVisible();
  expect(failures).toEqual([]);
});

test('TOTP: enrolling on a created user requires the code at login', async ({ page }) => {
  const user = await createUser(admin, 'totp');
  const api = await apiAs(user);
  const enroll = await api.post('/api/auth/totp/enroll');
  expect(enroll.status()).toBe(200);
  const { secret } = await enroll.json();
  expect((await api.post('/api/auth/totp/verify', { data: { code: '000000' } })).status()).toBe(400);
  expect((await api.post('/api/auth/totp/verify', { data: { code: totpNow(secret) } })).status()).toBe(200);
  await api.dispose();

  // Login form: no code, then wrong code, then the right one.
  await page.goto('/');
  await page.getByLabel('Username').fill(user.username);
  await page.getByLabel('Password').fill(user.password);
  await page.getByRole('button', { name: 'Sign in' }).click();
  await expect(page.getByRole('alert')).toHaveText('Wrong username, password or 2FA code.');
  await page.getByLabel(/2FA code/).fill(totpNow(secret) === '000001' ? '000002' : '000001');
  await page.getByRole('button', { name: 'Sign in' }).click();
  await expect(page.getByRole('alert')).toHaveText('Wrong username, password or 2FA code.');
  await page.getByLabel(/2FA code/).fill(totpNow(secret));
  await page.getByRole('button', { name: 'Sign in' }).click();
  await expect(page.getByRole('navigation', { name: 'Primary content' })).toBeVisible();
  await page.getByRole('button', { name: 'sign out' }).click();

  // An admin can reset it, after which the password alone works.
  expect((await admin.post(`/api/admin/users/${user.id}/disable-totp`)).status()).toBe(200);
  await signIn(page, user);
});

test('TOTP: a signed-in user cannot swap the secret of an already enabled 2FA', async ({ page }) => {
  const user = await createUser(admin, 'totp');
  const api = await apiAs(user);
  const { secret } = await (await api.post('/api/auth/totp/enroll')).json();
  expect((await api.post('/api/auth/totp/verify', { data: { code: totpNow(secret) } })).status()).toBe(200);
  const again = await api.post('/api/auth/totp/enroll');
  expect(again.status(), 'second enroll with 2FA already enabled').toBe(409);
  expect((await again.json()).error).toBe('two-factor authentication is already enabled; ask an admin to reset it first');
  await api.dispose();
  // The old authenticator must still work.
  const ctx = await apiAs(user, { totp: totpNow(secret) });
  await ctx.dispose();

  // The UI offers no setup for it.
  await page.goto('/');
  await page.getByLabel('Username').fill(user.username);
  await page.getByLabel('Password').fill(user.password);
  await page.getByLabel(/2FA code/).fill(totpNow(secret));
  await page.getByRole('button', { name: 'Sign in' }).click();
  await openTab(page, 'Account');
  await expect(page.getByRole('row').filter({ hasText: 'Two-factor authentication' }).getByRole('cell')).toHaveText('enabled');
  await expect(page.getByRole('button', { name: 'Set up 2FA' })).toHaveCount(0);
});

test('wrong credentials and a disabled session: errors are messages, sign out returns to the login form', async ({ page }) => {
  const user = await createUser(admin, 'auth');
  await page.goto('/');
  await page.getByLabel('Username').fill('nobody_here');
  await page.getByLabel('Password').fill('whatever-whatever');
  await page.getByRole('button', { name: 'Sign in' }).click();
  await expect(page.getByRole('alert')).toHaveText('Wrong username, password or 2FA code.');
  await signIn(page, user);
  // No bindings: nothing but the always-open tabs, and listings say nothing is allowed rather than crashing.
  await expect(page.getByRole('navigation', { name: 'Primary content' }).getByRole('button', { name: 'Servers', exact: true })).toHaveCount(0);
  await page.getByRole('button', { name: 'sign out' }).click();
  await expect(page.getByRole('heading', { name: 'Sign in' })).toBeVisible();
  // The session cookie is gone for real.
  await page.reload();
  await expect(page.getByRole('heading', { name: 'Sign in' })).toBeVisible();
});

test('user creation validation through the API the UI lacks', async () => {
  const short = await admin.post('/api/admin/users', { data: { username: unique('v').replace(/-/g, '_'), email: 'a@b.test', password: 'short', is_admin: false } });
  expect(short.status()).toBe(400);
  const bad = await admin.post('/api/admin/users', { data: { username: 'Bad Name', email: 'a@b.test', password: PASSWORD, is_admin: false } });
  expect(bad.status()).toBe(400);
});

test('the name inputs refuse invalid names in the browser (pattern attribute)', async ({ page }) => {
  const sent: string[] = [];
  page.on('request', (r) => { if (r.method() === 'POST' && /\/api\/(servers|access\/roles)$/.test(r.url())) sent.push(r.url()); });
  await signIn(page);
  await openAdd(page);
  await page.getByRole('button', { name: 'SSH-forwarded socket' }).click();
  const serverName = page.getByLabel('Name');
  for (const bad of ['Bad_Name', '-lead', 'trail-', 'has space']) {
    await serverName.fill(bad);
    expect(await serverName.evaluate((el: HTMLInputElement) => el.validity.patternMismatch), bad).toBe(true);
  }
  for (const good of ['ab', 'good-name-1']) {
    await serverName.fill(good);
    expect(await serverName.evaluate((el: HTMLInputElement) => el.validity.patternMismatch), good).toBe(false);
  }
  await serverName.fill('Bad_Name');
  await page.getByLabel('Forwarded socket').fill('/run/scontrol/remote/x.sock');
  await page.getByRole('button', { name: 'Add server' }).last().click();

  await openTab(page, 'Access');
  await page.getByRole('button', { name: 'New role' }).click();
  const roleName = page.getByRole('textbox', { name: 'Name' });
  await roleName.fill('Bad_Role');
  expect(await roleName.evaluate((el: HTMLInputElement) => el.validity.patternMismatch)).toBe(true);
  await page.getByLabel(/projects:read/).check();
  await page.getByRole('button', { name: 'Create role' }).click();
  await roleName.fill('good-role');
  expect(await roleName.evaluate((el: HTMLInputElement) => el.validity.patternMismatch)).toBe(false);
  expect(sent).toEqual([]);
});

// ------------------------------------------------- Deleting users, renaming servers

test('an admin deletes a user from the Users panel; their session stops working', async ({ page }) => {
  const failures = watchForFailures(page);
  const user = await createUser(admin, 'del');
  const session = await apiAs(user);
  expect((await session.get('/api/auth/me')).status()).toBe(200);

  await signIn(page);
  await openTab(page, 'Access');
  const row = rowByText(page, user.username);
  await expect(row).toBeVisible();
  // The signed-in admin cannot delete their own account from the UI.
  await expect(rowByText(page, ADMIN.username).first().getByRole('button', { name: 'delete' })).toHaveCount(0);

  // Dismissing the confirmation keeps the user.
  page.once('dialog', (d) => d.dismiss());
  await row.getByRole('button', { name: 'delete' }).click();
  await expect(row).toBeVisible();

  page.once('dialog', (d) => d.accept());
  const deleted = page.waitForResponse((r) => r.request().method() === 'DELETE' && r.url().endsWith(`/api/admin/users/${user.id}`));
  await row.getByRole('button', { name: 'delete' }).click();
  expect((await deleted).status()).toBe(200);
  await expect(rowByText(page, user.username)).toHaveCount(0);
  // Gone from the role-binding user select too.
  await expect(page.getByLabel('User').locator('option', { hasText: user.username })).toHaveCount(0);

  expect((await session.get('/api/auth/me')).status()).toBe(401);
  await session.dispose();
  const users: { id: string }[] = await (await admin.get('/api/admin/users')).json();
  expect(users.some((u) => u.id === user.id)).toBe(false);
  expect(failures).toEqual([]);
});

test('user deletion refuses self-deletion, unknown users and non-admins', async () => {
  const me = await (await admin.get('/api/auth/me')).json();
  const self = await admin.delete(`/api/admin/users/${me.id}`);
  expect(self.status()).toBe(400);
  expect(await self.text()).toContain('your own account');
  expect((await admin.delete('/api/admin/users/00000000-0000-0000-0000-00000000beef')).status()).toBe(404);

  const user = await createUser(admin, 'nodel');
  const other = await createUser(admin, 'victim');
  const session = await apiAs(user);
  expect((await session.delete(`/api/admin/users/${other.id}`)).status()).toBe(403);
  await session.dispose();
  expect((await admin.delete(`/api/admin/users/${user.id}`)).status()).toBe(200);
  expect((await admin.delete(`/api/admin/users/${other.id}`)).status()).toBe(200);
});

test('renames a server from the Servers tab; the panel host has no rename', async ({ page }) => {
  const failures = watchForFailures(page);
  const name = unique('ren');
  const created = await admin.post('/api/servers', { data: { name, transport: 'ssh_socket', address: `/run/scontrol/remote/${name}.sock` } });
  expect(created.ok(), await created.text()).toBe(true);
  const id = (await created.json()).server.id as string;
  try {
    await signIn(page);
    await openTab(page, 'Servers');
    await expect(page.getByRole('row').filter({ hasText: 'panel host' }).getByRole('button', { name: 'rename' })).toHaveCount(0);

    // An invalid name is refused by the server and the old name stays.
    page.once('dialog', (d) => d.accept('Bad Name'));
    await serverRow(page, name).getByRole('button', { name: 'rename' }).click();
    await expect(page.getByRole('alert')).toContainText('server names are');
    await expect(serverRow(page, name)).toBeVisible();

    const renamed = `${name}-x`;
    page.once('dialog', (d) => d.accept(renamed));
    await serverRow(page, name).getByRole('button', { name: 'rename' }).click();
    await expect(serverRow(page, renamed)).toBeVisible();
    await expect(serverRow(page, name)).toHaveCount(0);
    const servers: { id: string; name: string }[] = await (await admin.get('/api/servers')).json();
    expect(servers.find((s) => s.id === id)?.name).toBe(renamed);
    expect(failures).toEqual([]);
  } finally {
    await admin.delete(`/api/servers/${id}`);
  }
});

test('deleting a user who owns a site and a service removes them too, after typing the username', async ({ page }) => {
  const failures = watchForFailures(page);
  const username = unique('own').replace(/-/g, '_');
  const created = await admin.post('/api/admin/users', { data: { username, email: `${username}@example.test`, password: PASSWORD, is_admin: true } });
  expect(created.status(), await created.text()).toBe(200);
  const id = (await created.json()).id as string;
  const owner = await apiAs({ username, password: PASSWORD });
  const domain = `${unique('owned')}.example.test`;
  const site = await owner.post('/api/sites', { data: { domain, owner_username: unique('sys').replace(/-/g, '_'), runtime: { kind: 'static' } } });
  expect(site.ok(), await site.text()).toBe(true);
  const slug = unique('owned');
  const service = await owner.post('/api/compose-projects', {
    data: { name: slug, slug, source: { kind: 'raw', compose_yaml: 'services:\n  web:\n    image: nginx:alpine\n' }, deploy: false },
  });
  expect(service.ok(), await service.text()).toBe(true);
  await owner.dispose();

  await signIn(page);
  await openTab(page, 'Access');
  const row = rowByText(page, username);
  let deletes = 0;
  page.on('request', (r) => { if (r.method() === 'DELETE' && r.url().includes(`/api/admin/users/${id}`)) deletes += 1; });

  // A wrong confirmation deletes nothing.
  const prompts: string[] = [];
  page.on('dialog', (d) => {
    if (d.type() === 'confirm') return d.accept();
    prompts.push(d.message());
    return d.accept(prompts.length === 1 ? 'wrong' : username);
  });
  await row.getByRole('button', { name: 'delete' }).click();
  await expect(page.getByRole('alert')).toContainText('did not match');
  expect(prompts[0]).toContain('owns 1 site(s) and 1 service(s)');
  expect(deletes).toBe(1);
  await expect(row).toBeVisible();

  await row.getByRole('button', { name: 'delete' }).click();
  await expect(page.getByRole('status')).toContainText(`Deleted ${username} with 1 site(s) and 1 service(s).`);
  await expect(rowByText(page, username)).toHaveCount(0);
  expect(deletes).toBe(3);

  const sites: { domain: string }[] = await (await admin.get('/api/sites')).json();
  expect(sites.some((s) => s.domain === domain)).toBe(false);
  const services: { slug: string }[] = await (await admin.get('/api/compose-projects')).json();
  expect(services.some((s) => s.slug === slug)).toBe(false);
  expect(failures).toEqual([]);
});
