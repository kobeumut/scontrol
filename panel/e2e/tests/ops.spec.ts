import { generateKeyPairSync } from 'node:crypto';
import { createServer, IncomingMessage } from 'node:http';
import { AddressInfo } from 'node:net';
import { expect, Page, test } from '@playwright/test';
import { openTab, signIn, unique, watchForFailures } from './helpers';

test.beforeEach(async ({ page }) => {
  page.on('dialog', (dialog) => dialog.accept());
  await signIn(page);
});

const row = (page: Page, text: string) => page.getByRole('row').filter({ hasText: text });
const apiGet = async (page: Page, path: string) => (await page.request.get(`/api${path}`)).json();

// ---------------------------------------------------------------- Monitoring

test('monitoring shows host state, switches ranges and requests matching data', async ({ page }) => {
  const failures = watchForFailures(page);
  await openTab(page, 'Monitoring');
  await expect(page.getByRole('heading', { name: 'Monitoring' })).toBeVisible();
  await expect(page.getByRole('heading', { name: 'Threshold alerts' })).toBeVisible();

  // Live tiles come straight from the mock agent's pretend host.
  const tile = (label: string) => page.locator('.stat').filter({ has: page.getByText(label, { exact: true }) });
  await expect(tile('CPU')).toContainText('12.5%');
  await expect(tile('CPU')).toContainText('4 cores · load 0.40 / 0.30 / 0.20');
  await expect(tile('Memory')).toContainText('37.5%');
  await expect(tile('Memory')).toContainText('3.0 GB of 8.0 GB');
  await expect(tile('Disk /')).toContainText('40.0%');
  await expect(tile('Disk /')).toContainText('40.0 GB of 100 GB');
  await expect(tile('Disk I/O')).toContainText('read 2.0 KB/s · write 8.0 KB/s');
  await expect(page.getByText('Waiting for the first metrics sample from the agent…')).toHaveCount(0);
  await expect(page.getByText(/^Sampled /)).toBeVisible();

  // The collector stores a sample every 30 s; the 1 h series refreshes every 30 s, so charts appear within ~1-2 min.
  await expect(page.getByRole('img', { name: 'CPU, latest CPU 12.5%' })).toBeVisible({ timeout: 100_000 });
  await expect(page.getByRole('img', { name: 'Memory, latest Memory 37.5%' })).toBeVisible();
  await expect(page.getByRole('img', { name: 'Disk used, latest Disk used 40.0%' })).toBeVisible();
  await expect(page.getByRole('img', { name: /^Disk I\/O, latest Read 2\.0 KB\/s, Write 8\.0 KB\/s/ })).toBeVisible();
  await expect(page.getByText('No samples stored for this range yet.')).toHaveCount(0);

  const group = page.getByRole('group', { name: 'Time range' });
  for (const range of ['6h', '24h', '7d', '30d', '1h']) {
    const request = page.waitForRequest((r) => r.url().includes(`/api/metrics/server?range=${range}`));
    await group.getByRole('button', { name: range, exact: true }).click();
    await request;
    await expect(group.getByRole('button', { name: range, exact: true })).toHaveClass(/selected/);
    await expect(page.getByText('Loading…')).toHaveCount(0);
  }
  expect(failures).toEqual([]);
});

test('threshold alert settings can be edited, saved and are validated by the server', async ({ page }) => {
  await openTab(page, 'Monitoring');
  const form = page.locator('form').filter({ has: page.getByRole('button', { name: 'Save thresholds' }) });
  const original = await apiGet(page, '/metrics/alerts');
  const settings = original.settings;
  try {
    await form.getByLabel('CPU %').fill('77');
    await form.getByLabel('Memory %', { exact: true }).fill('66');
    await form.getByLabel('Disk % (any mount)').fill('55');
    await form.getByLabel('Service memory % of limit').fill('88');
    await form.getByLabel('Hysteresis (points)').fill('2.5');
    await form.getByLabel('Fire after N samples').fill('4');
    await form.getByLabel('Resolve after N samples').fill('5');
    await form.getByRole('checkbox').uncheck();
    await form.getByRole('button', { name: 'Save thresholds' }).click();
    await expect(form.getByText('Saved.')).toBeVisible();

    // Survives a reload and is what the API stores.
    await page.reload();
    await openTab(page, 'Monitoring');
    await expect(form.getByLabel('CPU %')).toHaveValue('77');
    await expect(form.getByLabel('Resolve after N samples')).toHaveValue('5');
    await expect(form.getByRole('checkbox')).not.toBeChecked();
    const stored = (await apiGet(page, '/metrics/alerts')).settings;
    expect(stored).toMatchObject({ enabled: false, cpu_pct: 77, memory_pct: 66, disk_pct: 55, container_memory_pct: 88, hysteresis_pct: 2.5, trigger_samples: 4, clear_samples: 5 });

    // Editing again clears the "Saved." marker.
    await form.getByLabel('CPU %').fill('78');
    await expect(form.getByText('Saved.')).toHaveCount(0);

    // A hysteresis at or above the threshold is refused by the server and the message is shown.
    await form.getByLabel('CPU %').fill('10');
    await form.getByLabel('Hysteresis (points)').fill('20');
    await form.getByRole('button', { name: 'Save thresholds' }).click();
    await expect(form.getByRole('alert')).toContainText('hysteresis must be between 0 and 50 and below the threshold');
    expect((await apiGet(page, '/metrics/alerts')).settings.cpu_pct).toBe(77);
  } finally {
    await page.request.put('/api/metrics/alerts/settings', { data: settings });
  }
});

test('threshold fields enforce their ranges in the browser', async ({ page }) => {
  await openTab(page, 'Monitoring');
  const form = page.locator('form').filter({ has: page.getByRole('button', { name: 'Save thresholds' }) });
  await form.getByLabel('CPU %').fill('0');
  const before = await apiGet(page, '/metrics/alerts');
  await form.getByRole('button', { name: 'Save thresholds' }).click();
  await expect(form.getByLabel('CPU %')).toHaveJSProperty('validity.rangeUnderflow', true);
  await form.getByLabel('CPU %').fill('101');
  await form.getByRole('button', { name: 'Save thresholds' }).click();
  await expect(form.getByLabel('CPU %')).toHaveJSProperty('validity.rangeOverflow', true);
  await form.getByLabel('Fire after N samples').fill('61');
  await expect(form.getByLabel('Fire after N samples')).toHaveJSProperty('validity.rangeOverflow', true);
  expect(await apiGet(page, '/metrics/alerts')).toEqual(before);
});

// ------------------------------------------------------------- Observability

test('request statistics switch range, filter by domain and show an empty state', async ({ page }) => {
  const failures = watchForFailures(page);
  await openTab(page, 'Observability');
  await expect(page.getByRole('heading', { name: 'Request statistics' })).toBeVisible();
  await expect(page.getByText('Loading…')).toHaveCount(0);
  await expect(page.getByLabel('Domain')).toContainText('All domains');
  await expect(page.getByText(/\d+ requests · .* sent · \d+ min buckets/)).toBeVisible();

  const group = page.getByRole('group', { name: 'Time range' });
  for (const [label, hours, step] of [['1 h', 1, 1], ['6 h', 6, 2], ['7 d', 168, 60], ['30 d', 720, 180], ['24 h', 24, 15]] as const) {
    const request = page.waitForRequest((r) => r.url().includes(`/api/observability/requests?hours=${hours}`));
    await group.getByRole('button', { name: label, exact: true }).click();
    await request;
    await expect(group.getByRole('button', { name: label, exact: true })).toHaveClass(/selected/);
    if (hours !== 6) await expect(page.getByText(new RegExp(`${step} min buckets`))).toBeVisible();
  }
  expect(failures).toEqual([]);
});

test('request statistics reject an out-of-range window and a malformed host', async ({ page }) => {
  expect((await page.request.get('/api/observability/requests?hours=0')).status()).toBe(400);
  expect((await page.request.get('/api/observability/requests?hours=721')).status()).toBe(400);
  expect((await page.request.get('/api/observability/requests?host=a%20b')).status()).toBe(400);
});

test('docker cleanup settings can be edited and saved; the run button exists but is not pressed', async ({ page }) => {
  await openTab(page, 'Observability');
  await expect(page.getByRole('heading', { name: 'Docker cleanup' })).toBeVisible();
  // Deliberately never clicked: it POSTs /api/observability/docker-cleanup/run and blocks deployments.
  await expect(page.getByRole('button', { name: 'Clean up now' })).toBeEnabled();

  const original = (await apiGet(page, '/observability/docker-cleanup')).settings;
  const section = page.locator('section.panel').filter({ has: page.getByRole('heading', { name: 'Docker cleanup' }) });
  try {
    await section.getByLabel('Automatic cleanup').check();
    await section.getByLabel('Every').selectOption({ label: '3 days' });
    await section.getByLabel('Older than (hours)').fill('200');
    await section.getByRole('button', { name: 'Save', exact: true }).click();
    await expect.poll(async () => (await apiGet(page, '/observability/docker-cleanup')).settings)
      .toEqual({ enabled: true, interval_hours: 72, older_than_hours: 200 });

    await page.reload();
    await openTab(page, 'Observability');
    await expect(section.getByLabel('Automatic cleanup')).toBeChecked();
    await expect(section.getByLabel('Every')).toHaveValue('72');
    await expect(section.getByLabel('Older than (hours)')).toHaveValue('200');
    await expect(section.getByRole('option')).toHaveText(['6 hours', '12 hours', '1 day', '3 days', '7 days']);
  } finally {
    await page.request.put('/api/observability/docker-cleanup/settings', { data: original });
  }
});

test('docker cleanup validates the age in the browser; the server also refuses it', async ({ page }) => {
  await openTab(page, 'Observability');
  const original = (await apiGet(page, '/observability/docker-cleanup')).settings;
  const section = page.locator('section.panel').filter({ has: page.getByRole('heading', { name: 'Docker cleanup' }) });
  const age = section.getByLabel('Older than (hours)');
  await expect(age).toHaveAttribute('required', '');
  await expect(age).toHaveAttribute('min', '1');
  await expect(age).toHaveAttribute('max', '8760');
  let puts = 0;
  page.on('request', (r) => { if (r.method() === 'PUT' && r.url().includes('/docker-cleanup/settings')) puts += 1; });
  await age.fill('0');
  await section.getByRole('button', { name: 'Save', exact: true }).click();
  await expect(age).toHaveJSProperty('validity.rangeUnderflow', true);
  await age.fill('8761');
  await section.getByRole('button', { name: 'Save', exact: true }).click();
  await expect(age).toHaveJSProperty('validity.rangeOverflow', true);
  await expect(section.getByText('Saved.')).toHaveCount(0);
  expect(puts).toBe(0);
  expect((await apiGet(page, '/observability/docker-cleanup')).settings).toEqual(original);

  // The server enforces the same bound for clients that skip the browser.
  const refused = await page.request.put('/api/observability/docker-cleanup/settings', { data: { ...original, older_than_hours: 0 } });
  expect(refused.status()).toBe(400);
  expect(await refused.text()).toContain('older_than_hours must be 1 to 8760');
  expect((await apiGet(page, '/observability/docker-cleanup')).settings).toEqual(original);
});

test('docker cleanup confirms that settings were saved', async ({ page }) => {
  await openTab(page, 'Observability');
  const original = (await apiGet(page, '/observability/docker-cleanup')).settings;
  const section = page.locator('section.panel').filter({ has: page.getByRole('heading', { name: 'Docker cleanup' }) });
  try {
    await section.getByRole('button', { name: 'Save', exact: true }).click();
    await expect(section.getByText('Saved.')).toBeVisible();
    // Editing again clears the marker.
    await section.getByLabel('Older than (hours)').fill('201');
    await expect(section.getByText('Saved.')).toHaveCount(0);
  } finally {
    await page.request.put('/api/observability/docker-cleanup/settings', { data: original });
  }
});

test('docker cleanup can run on a chosen server; the request carries its id', async ({ page }) => {
  // A second, online server, so the picker is offered. Its list and the run
  // are answered in the browser: nothing is pruned and no deployment waits.
  const remote = {
    id: '00000000-0000-4000-8000-00000000c1ea', name: unique('cleanup'), transport: 'ssh_socket',
    address: '/run/scontrol/remote/x.sock', status: 'online', draining: false, max_services: null, service_count: 0,
    agent_version: null, docker: true, last_error: null, last_seen_at: null, cert_expires_at: null,
    cert_fingerprint: null, rotation_pending: false,
  };
  await page.route('**/api/servers', async (route) => {
    const response = await route.fetch();
    await route.fulfill({ response, json: [...await response.json(), remote] });
  });
  const sent: unknown[] = [];
  await page.route('**/api/observability/docker-cleanup/run', async (route) => {
    sent.push(route.request().postDataJSON());
    await route.fulfill({ json: {
      id: '00000000-0000-4000-8000-0000000000aa', server_id: remote.id, trigger: 'manual', status: 'succeeded',
      older_than_hours: 24, started_at: new Date().toISOString(), finished_at: new Date().toISOString(),
      images_freed_bytes: 0, builder_freed_bytes: 0, error_message: null,
    } });
  });

  await openTab(page, 'Observability');
  const section = page.locator('section.panel').filter({ has: page.getByRole('heading', { name: 'Docker cleanup' }) });
  const picker = section.getByLabel('Clean up now on');
  await expect(picker).toHaveValue('');
  await expect(picker.getByRole('option', { name: /automatic/ })).toHaveCount(0);

  await picker.selectOption({ label: remote.name });
  // beforeEach accepts every dialog; this only records the question.
  const messages: string[] = [];
  page.on('dialog', (d) => { messages.push(d.message()); });
  await section.getByRole('button', { name: 'Clean up now' }).click();
  await expect.poll(() => sent.length).toBe(1);
  expect(sent[0]).toMatchObject({ server_id: remote.id });
  expect(messages[0]).toContain(`on ${remote.name}`);

  // This host sends no server id.
  await picker.selectOption({ label: 'this host' });
  await section.getByRole('button', { name: 'Clean up now' }).click();
  await expect.poll(() => sent.length).toBe(2);
  expect(sent[1]).not.toHaveProperty('server_id');
});

// ------------------------------------------------------------- Notifications

/** A local HTTP receiver the API can deliver webhooks to. */
async function receiver() {
  const received: { headers: IncomingMessage['headers']; body: string }[] = [];
  const server = createServer((req, res) => {
    let body = '';
    req.on('data', (chunk) => { body += chunk; });
    req.on('end', () => { received.push({ headers: req.headers, body }); res.end('ok'); });
  });
  await new Promise<void>((resolve) => server.listen(0, '127.0.0.1', resolve));
  const url = `http://127.0.0.1:${(server.address() as AddressInfo).port}/hook`;
  return { url, received, close: () => new Promise((resolve) => server.close(resolve)) };
}

async function startChannel(page: Page, name: string, kind: string) {
  await openTab(page, 'Notifications');
  await page.getByRole('button', { name: 'Add channel' }).click();
  await page.getByLabel('Name', { exact: true }).fill(name);
  await page.getByLabel('Kind').selectOption({ label: kind });
}

async function deleteChannel(page: Page, name: string) {
  await row(page, name).getByRole('button', { name: 'delete' }).click();
  await expect(row(page, name)).toHaveCount(0);
}

test('notification channel form validates before creating', async ({ page }) => {
  await openTab(page, 'Notifications');
  await expect(page.getByText('Loading…')).toHaveCount(0);
  const name = unique('ops-bad');

  await startChannel(page, name, 'Slack');
  await page.getByLabel('Webhook URL').fill('https://evil.example/services/x');
  await page.getByRole('button', { name: 'Create channel' }).click();
  await expect(page.getByRole('alert')).toHaveText('a Slack webhook URL starts with https://hooks.slack.com/');

  await page.getByLabel('Webhook URL').fill('https://hooks.slack.com/services/T0/B0/x');
  await page.getByLabel('Deploy failed').uncheck();
  await page.getByRole('button', { name: 'Create channel' }).click();
  await expect(page.getByRole('alert')).toHaveText('choose at least one event');

  await page.getByLabel('Kind').selectOption({ label: 'ntfy' });
  await page.getByLabel('Server URL').fill('https://ntfy.example.test');
  await page.getByLabel('Topic').fill('has space');
  await page.getByLabel('Deploy failed').check();
  await page.getByRole('button', { name: 'Create channel' }).click();
  await expect(page.getByRole('alert')).toHaveText("topic may use letters, digits, '-' and '_' only");

  await page.getByLabel('Kind').selectOption({ label: 'Telegram' });
  await page.getByLabel('Chat ID').fill('not a chat');
  await page.getByLabel('Bot token').fill('123456:ABCdef');
  await page.getByRole('button', { name: 'Create channel' }).click();
  await expect(page.getByRole('alert')).toHaveText('chat ID must be a number or an @channel name');

  await page.getByLabel('Kind').selectOption({ label: 'Email (SMTP)' });
  await page.getByLabel('SMTP host').fill('mail.example.test');
  await page.getByLabel('From').fill('panel@example.test');
  await page.getByLabel('To (comma-separated)').fill('');
  await page.getByRole('button', { name: 'Create channel' }).click();
  await expect(page.getByRole('alert')).toHaveText('give between 1 and 10 recipients');
  await page.getByLabel('To (comma-separated)').fill('not-an-address');
  await page.getByRole('button', { name: 'Create channel' }).click();
  await expect(page.getByRole('alert')).toContainText('invalid recipient address');
  await page.getByLabel('Security').selectOption('none');
  await page.getByLabel('To (comma-separated)').fill('ops@example.test');
  await page.getByRole('button', { name: 'Create channel' }).click();
  await expect(page.getByRole('alert')).toHaveText('unencrypted SMTP is allowed only to a loopback relay');

  // A required secret cannot be left empty (native validation), and cancel creates nothing.
  await page.getByLabel('Kind').selectOption({ label: 'Gotify' });
  await page.getByLabel('Server URL').fill('https://gotify.example.test');
  // Changing Kind cleared the Telegram token, so the Gotify token is empty again.
  await expect(page.getByLabel('Application token')).toHaveValue('');
  await expect(page.getByLabel('Application token')).toHaveJSProperty('validity.valueMissing', true);
  await page.getByRole('button', { name: 'Cancel' }).click();
  await expect(row(page, name)).toHaveCount(0);
});

const CREATE_CASES: { kind: string; label: string; fill: (page: Page) => Promise<void> }[] = [
  { kind: 'slack', label: 'Slack', fill: (p) => p.getByLabel('Webhook URL').fill('https://hooks.slack.com/services/T0/B0/secret-value') },
  { kind: 'discord', label: 'Discord', fill: (p) => p.getByLabel('Webhook URL').fill('https://discord.com/api/webhooks/1/secret-value') },
  { kind: 'telegram', label: 'Telegram', fill: async (p) => { await p.getByLabel('Chat ID').fill('@opschannel'); await p.getByLabel('Bot token').fill('123456:ABCDEFGHIJKLMNOPQRSTUVWXYZ-secret-value'); } },
  { kind: 'ntfy', label: 'ntfy', fill: async (p) => { await p.getByLabel('Server URL').fill('https://ntfy.example.test'); await p.getByLabel('Topic').fill('ops-alerts'); } },
  { kind: 'gotify', label: 'Gotify', fill: async (p) => { await p.getByLabel('Server URL').fill('https://gotify.example.test'); await p.getByLabel('Application token').fill('secret-value'); } },
  { kind: 'webhook', label: 'Webhook', fill: (p) => p.getByLabel('URL', { exact: true }).fill('https://hooks.example.test/in') },
  { kind: 'email', label: 'Email (SMTP)', fill: async (p) => {
    await p.getByLabel('SMTP host').fill('mail.example.test');
    await p.getByLabel('Username (optional)').fill('mailer');
    await p.getByLabel('From').fill('Panel <panel@example.test>');
    await p.getByLabel('To (comma-separated)').fill('a@example.test, b@example.test');
    await p.getByLabel('SMTP password').fill('secret-value');
  } },
];

for (const { kind, label, fill } of CREATE_CASES) {
  test(`creates, lists and deletes a ${label} channel`, async ({ page }) => {
    const name = unique(`ops-${kind}`);
    await startChannel(page, name, label);
    await fill(page);
    await page.getByLabel('Deploy succeeded').check();
    await page.getByLabel('Resource alert (CPU, memory, disk)', { exact: true }).check();
    await page.getByRole('button', { name: 'Create channel' }).click();

    const created = row(page, name);
    await expect(created).toBeVisible();
    await expect(created).toContainText(label);
    await expect(created).toContainText('Deploy failed, Deploy succeeded, Resource alert (CPU, memory, disk)');
    await expect(page.getByRole('button', { name: 'Create channel' })).toHaveCount(0);

    const stored = (await apiGet(page, '/notifications')).find((c: { name: string }) => c.name === name);
    expect(stored).toMatchObject({ kind, enabled: true });
    expect(JSON.stringify(stored)).not.toContain('secret-value');
    expect(stored.has_secret).toBe(kind !== 'ntfy' && kind !== 'webhook');

    await deleteChannel(page, name);
    expect((await apiGet(page, '/notifications')).some((c: { name: string }) => c.name === name)).toBe(false);
  });
}

test('edits a channel, keeps its write-only secret and can disable it', async ({ page }) => {
  const name = unique('ops-edit');
  const renamed = `${name}-b`;
  await startChannel(page, name, 'Webhook');
  await page.getByLabel('URL', { exact: true }).fill('https://hooks.example.test/one');
  await page.getByLabel('Signing secret (optional)').fill('signing-secret-value');
  await page.getByRole('button', { name: 'Create channel' }).click();
  await expect(row(page, name)).toBeVisible();

  await row(page, name).getByRole('button', { name: 'edit' }).click();
  // The secret is never sent back to the browser.
  const secret = page.getByLabel('Signing secret (optional)');
  await expect(secret).toHaveValue('');
  await expect(secret).toHaveAttribute('placeholder', 'stored; leave empty to keep');
  await expect(page.getByLabel('Deploy failed')).toBeChecked();
  await page.getByLabel('Name', { exact: true }).fill(renamed);
  await page.getByLabel('URL', { exact: true }).fill('https://hooks.example.test/two');
  await page.getByLabel('Deploy failed').uncheck();
  await page.getByLabel('Scheduled job failed').check();
  await page.getByLabel('enabled').uncheck();
  await page.getByRole('button', { name: 'Save channel' }).click();

  await expect(row(page, renamed)).toContainText('(disabled)');
  await expect(row(page, renamed)).toContainText('Scheduled job failed');
  await expect(row(page, renamed)).not.toContainText('Deploy failed');
  await expect(page.getByRole('cell', { name, exact: true })).toHaveCount(0);
  const stored = (await apiGet(page, '/notifications')).find((c: { name: string }) => c.name === renamed);
  expect(stored).toMatchObject({ url: 'https://hooks.example.test/two', has_secret: true, enabled: false, events: ['job_failed'] });

  // Switching to a kind whose secret is required must ask for one again.
  await row(page, renamed).getByRole('button', { name: 'edit' }).click();
  await page.getByLabel('Kind').selectOption({ label: 'Slack' });
  await expect(page.getByLabel('Webhook URL')).toHaveJSProperty('validity.valueMissing', true);
  await page.getByRole('button', { name: 'Cancel' }).click();

  await deleteChannel(page, renamed);
});

test('changing the kind clears a secret typed for the previous kind', async ({ page }) => {
  await startChannel(page, unique('ops-kind'), 'Slack');
  await page.getByLabel('Webhook URL').fill('https://hooks.slack.com/services/T0/B0/typed-secret');
  await page.getByLabel('Kind').selectOption({ label: 'Gotify' });
  await expect(page.getByLabel('Application token')).toHaveValue('');
  await page.getByLabel('Application token').fill('gotify-typed');
  await page.getByLabel('Kind').selectOption({ label: 'Slack' });
  await expect(page.getByLabel('Webhook URL')).toHaveValue('');
  await page.getByLabel('Kind').selectOption({ label: 'Telegram' });
  await expect(page.getByLabel('Bot token')).toHaveValue('');
  await page.getByRole('button', { name: 'Cancel' }).click();
});

test('cancelling the edit form leaves the channel untouched', async ({ page }) => {
  const name = unique('ops-cancel');
  await startChannel(page, name, 'Webhook');
  await page.getByLabel('URL', { exact: true }).fill('https://hooks.example.test/c');
  await page.getByRole('button', { name: 'Create channel' }).click();
  await row(page, name).getByRole('button', { name: 'edit' }).click();
  await page.getByLabel('Name', { exact: true }).fill('something else');
  await page.getByRole('button', { name: 'Cancel' }).click();
  await expect(row(page, name)).toBeVisible();
  await expect(page.getByRole('button', { name: 'Add channel' })).toBeVisible();
  await deleteChannel(page, name);
});

test('test send reports delivery and failure, with a signed payload', async ({ page }) => {
  const hook = await receiver();
  const good = unique('ops-ok');
  const bad = unique('ops-down');
  try {
    await startChannel(page, good, 'Webhook');
    await page.getByLabel('URL', { exact: true }).fill(hook.url);
    await page.getByLabel('Signing secret (optional)').fill('signing-secret-value');
    await page.getByRole('button', { name: 'Create channel' }).click();
    await expect(row(page, good)).toContainText('—');

    await row(page, good).getByRole('button', { name: 'test' }).click();
    await expect(page.getByRole('status')).toHaveText(`Test sent to ${good}.`);
    await expect(row(page, good)).toContainText(/sent /);
    expect(hook.received).toHaveLength(1);
    expect(hook.received[0].headers['x-scontrol-event']).toBe('test');
    expect(hook.received[0].headers['x-scontrol-signature']).toBeTruthy();

    // Nothing listens on port 1: the failure is reported and remembered on the row.
    await page.getByRole('button', { name: 'Add channel' }).click();
    await page.getByLabel('Name', { exact: true }).fill(bad);
    await page.getByLabel('Kind').selectOption({ label: 'Webhook' });
    await page.getByLabel('URL', { exact: true }).fill('http://127.0.0.1:1/hook');
    await page.getByRole('button', { name: 'Create channel' }).click();
    await row(page, bad).getByRole('button', { name: 'test' }).click();
    await expect(page.getByRole('status')).toContainText(`Test to ${bad} failed:`);
    await expect(row(page, bad).locator('.state.error')).toContainText('Connection refused');
  } finally {
    await hook.close();
    for (const name of [good, bad]) {
      const channel = (await apiGet(page, '/notifications')).find((c: { name: string }) => c.name === name);
      if (channel) await page.request.delete(`/api/notifications/${channel.id}`);
    }
  }
});

test('the notification event list matches what the form offers', async ({ page }) => {
  const events: string[] = await apiGet(page, '/notifications/events');
  await startChannel(page, unique('ops-events'), 'Slack');
  const checkboxes = page.locator('form .toolbar .check').filter({ hasNotText: /^\s*enabled\s*$/ });
  await expect(checkboxes).toHaveCount(events.length);
  await page.getByRole('button', { name: 'Cancel' }).click();
});

// --------------------------------------------------------------- GitHub Apps

const privateKey = () => generateKeyPairSync('rsa', { modulusLength: 2048, privateKeyEncoding: { type: 'pkcs1', format: 'pem' }, publicKeyEncoding: { type: 'pkcs1', format: 'pem' } }).privateKey;

test('github app rejects a bad private key and a duplicate name', async ({ page }) => {
  await openTab(page, 'GitHub Apps');
  await expect(page.getByText('Loading…')).toHaveCount(0);
  const name = unique('ops-app');
  await page.getByRole('button', { name: 'Add App' }).click();
  await page.getByLabel('Name', { exact: true }).fill(name);
  await page.getByLabel('App ID').fill('12345');
  await page.locator('textarea').fill('this is not a pem key');
  await page.getByRole('button', { name: 'Save App' }).click();
  await expect(page.getByRole('alert')).toHaveText('the private key must be the PEM file GitHub generated for the App');
  await expect(row(page, name)).toHaveCount(0);

  // App ID is numeric only (browser pattern), so a letter blocks the request.
  await page.getByLabel('App ID').fill('12a');
  await expect(page.getByLabel('App ID')).toHaveJSProperty('validity.patternMismatch', true);
  await page.getByRole('button', { name: 'Cancel' }).click();
  await expect(page.getByRole('button', { name: 'Add App' })).toBeVisible();
});

test('registers a github app from a key file, lists it and deletes it', async ({ page }) => {
  const name = unique('ops-app');
  await openTab(page, 'GitHub Apps');
  await page.getByRole('button', { name: 'Add App' }).click();
  await page.getByLabel('Name', { exact: true }).fill(name);
  await page.getByLabel('App ID').fill('424242');
  await page.locator('input[type=file]').setInputFiles({ name: 'app.pem', mimeType: 'application/x-pem-file', buffer: Buffer.from(privateKey()) });
  await expect(page.locator('textarea')).toHaveValue(/BEGIN RSA PRIVATE KEY/);
  await page.getByRole('button', { name: 'Save App' }).click();

  await expect(row(page, name)).toContainText('424242');
  await expect(page.locator('textarea')).toHaveCount(0);
  const stored = (await apiGet(page, '/github-apps')).find((a: { name: string }) => a.name === name);
  expect(stored.app_id).toBe(424242);
  expect(JSON.stringify(stored)).not.toContain('PRIVATE KEY');

  // Same name again: conflict, shown in the form.
  await page.getByRole('button', { name: 'Add App' }).click();
  await page.getByLabel('Name', { exact: true }).fill(name);
  await page.getByLabel('App ID').fill('1');
  await page.locator('textarea').fill(privateKey());
  await page.getByRole('button', { name: 'Save App' }).click();
  await expect(page.getByRole('alert')).toHaveText('a GitHub App with this name already exists');
  await page.getByRole('button', { name: 'Cancel' }).click();

  await row(page, name).getByRole('button', { name: 'delete' }).click();
  await expect(row(page, name)).toHaveCount(0);
});
