import { expect, Page, test } from '@playwright/test';
import { openTab, signIn, unique, watchForFailures } from './helpers';

test.beforeEach(async ({ page }) => {
  await signIn(page);
  await openTab(page, 'Hosted sites');
  await expect(page.getByRole('heading', { name: 'Hosted sites' })).toBeVisible();
  await expect(page.getByText('Loading…')).toHaveCount(0);
});

const row = (page: Page, text: string) => page.getByRole('row').filter({ hasText: text });
const apiGet = async (page: Page, path: string) => (await page.request.get(`/api${path}`)).json();
const newDomain = () => `${unique('site')}.example.test`;
const newUser = () => unique('u');

/** Answers the next confirm/prompt dialog and returns what it said. */
function nextDialog(page: Page, answer: 'accept' | 'dismiss', promptText?: string) {
  return new Promise<{ type: string; message: string }>((resolve) => {
    page.once('dialog', async (dialog) => {
      const seen = { type: dialog.type(), message: dialog.message() };
      if (answer === 'accept') await dialog.accept(promptText); else await dialog.dismiss();
      resolve(seen);
    });
  });
}

async function siteByDomain(page: Page, domain: string) {
  return (await apiGet(page, '/sites')).find((s: { domain: string }) => s.domain === domain);
}
async function dropSite(page: Page, domain: string) {
  const site = await siteByDomain(page, domain);
  if (site) await page.request.delete(`/api/sites/${site.id}`);
}

async function openForm(page: Page) {
  await page.getByRole('button', { name: 'New site' }).click();
  await expect(page.getByRole('button', { name: 'Create site' })).toBeVisible();
}
async function fillBasics(page: Page, domain: string, owner: string, runtime: string) {
  await page.getByLabel('Domain', { exact: true }).fill(domain);
  await page.getByLabel('System user').fill(owner);
  await page.getByLabel('Runtime').selectOption({ label: runtime });
}

// ------------------------------------------------------------------- Create

const RUNTIMES: { label: string; kind: string; fill?: (page: Page) => Promise<void>; shown: string; runtime: Record<string, unknown> }[] = [
  { label: 'Static files', kind: 'static', shown: 'Static files', runtime: { kind: 'static' } },
  { label: 'PHP (PHP-FPM)', kind: 'php', shown: 'PHP (PHP-FPM) 8.3', runtime: { kind: 'php', version: '8.3' } },
  { label: 'Node.js', kind: 'node_js', shown: 'Node.js 22', runtime: { kind: 'node_js', version: '22', entrypoint: 'server.js' },
    fill: async (p) => { await p.getByLabel('Version').fill('22'); await p.getByLabel('Entrypoint').fill('server.js'); } },
  { label: 'Python', kind: 'python', shown: 'Python 3.12', runtime: { kind: 'python', version: '3.12', entrypoint: 'app.py' } },
  { label: 'Reverse proxy', kind: 'reverse_proxy', shown: 'Reverse proxy → http://127.0.0.1:4567', runtime: { kind: 'reverse_proxy', upstream: 'http://127.0.0.1:4567' },
    fill: async (p) => { await p.getByLabel('Upstream').fill('http://127.0.0.1:4567'); } },
];

for (const { label, kind, fill, shown, runtime } of RUNTIMES) {
  test(`creates a ${label} site from the UI and deletes it`, async ({ page }) => {
    const failures = watchForFailures(page);
    const domain = newDomain();
    try {
      await openForm(page);
      await fillBasics(page, domain, newUser(), label);
      // Each runtime shows only the fields it needs, with sensible defaults.
      await expect(page.getByLabel('Version')).toHaveCount(['php', 'node_js', 'python'].includes(kind) ? 1 : 0);
      await expect(page.getByLabel('Entrypoint')).toHaveCount(['node_js', 'python'].includes(kind) ? 1 : 0);
      await expect(page.getByLabel('Upstream')).toHaveCount(kind === 'reverse_proxy' ? 1 : 0);
      if (kind === 'php') await expect(page.getByLabel('Version')).toHaveValue('8.3');
      if (kind === 'python') { await expect(page.getByLabel('Version')).toHaveValue('3.12'); await expect(page.getByLabel('Entrypoint')).toHaveValue('app.py'); }
      if (kind === 'node_js') { await expect(page.getByLabel('Version')).toHaveValue('20'); await expect(page.getByLabel('Entrypoint')).toHaveValue('index.js'); }
      if (kind === 'reverse_proxy') await expect(page.getByLabel('Upstream')).toHaveValue('http://127.0.0.1:3000');
      await fill?.(page);
      await page.getByRole('button', { name: 'Create site' }).click();

      const created = row(page, domain);
      await expect(created).toBeVisible();
      await expect(page.getByRole('button', { name: 'Create site' })).toHaveCount(0);
      await expect(created).toContainText(shown);
      await expect(created).toContainText('active');
      await expect(created.getByRole('cell').nth(3)).toHaveText('—');
      expect(await siteByDomain(page, domain)).toMatchObject({ domain, runtime, ssl_enabled: false });

      const confirmed = nextDialog(page, 'accept');
      await created.getByRole('button', { name: 'delete' }).click();
      expect((await confirmed).message).toContain(`Delete ${domain}?`);
      await expect(created).toHaveCount(0);
      expect(await siteByDomain(page, domain)).toBeUndefined();
    } finally {
      await dropSite(page, domain);
    }
    expect(failures).toEqual([]);
  });
}

test('the domain is lowercased and the form closes with the Close button', async ({ page }) => {
  const domain = newDomain();
  try {
    await page.getByRole('button', { name: 'New site' }).click();
    await expect(page.getByRole('button', { name: 'Close' })).toBeVisible();
    await fillBasics(page, domain.toUpperCase(), newUser(), 'Static files');
    await page.getByRole('button', { name: 'Create site' }).click();
    await expect(row(page, domain)).toBeVisible();
    expect((await siteByDomain(page, domain)).domain).toBe(domain);

    await page.getByRole('button', { name: 'New site' }).click();
    await page.getByRole('button', { name: 'Close' }).click();
    await expect(page.getByRole('button', { name: 'Create site' })).toHaveCount(0);
  } finally {
    await dropSite(page, domain);
  }
});

// --------------------------------------------------------------- Validation

test('an invalid domain is refused by the server and nothing is listed', async ({ page }) => {
  await openForm(page);
  await fillBasics(page, 'bad domain', newUser(), 'Static files');
  await page.getByRole('button', { name: 'Create site' }).click();
  await expect(page.locator('form').getByRole('alert')).toBeVisible();
  await expect(row(page, 'bad domain')).toHaveCount(0);
  expect(await siteByDomain(page, 'bad domain')).toBeUndefined();
  await page.getByLabel('Domain', { exact: true }).fill('');
  await expect(page.getByLabel('Domain', { exact: true })).toHaveJSProperty('validity.valueMissing', true);
});

test('the system user pattern blocks an invalid name in the browser before any request', async ({ page }) => {
  const domain = newDomain();
  let posts = 0;
  page.on('request', (r) => { if (r.method() === 'POST' && r.url().endsWith('/api/sites')) posts += 1; });
  await openForm(page);
  const owner = page.getByLabel('System user');
  for (const bad of ['Ab', 'ab', '1abc', 'Root', 'has space', 'a'.repeat(40), 'bad.name']) {
    await fillBasics(page, domain, bad, 'Static files');
    await page.getByRole('button', { name: 'Create site' }).click();
    await expect(owner).toHaveJSProperty('validity.patternMismatch', true);
  }
  await owner.fill('good_user-1');
  await expect(owner).toHaveJSProperty('validity.patternMismatch', false);
  expect(posts).toBe(0);
  expect(await siteByDomain(page, domain)).toBeUndefined();
});

test('runtime fields are validated in the browser', async ({ page }) => {
  await openForm(page);
  await fillBasics(page, newDomain(), newUser(), 'PHP (PHP-FPM)');
  await page.getByLabel('Version').fill('8.x');
  await expect(page.getByLabel('Version')).toHaveJSProperty('validity.patternMismatch', true);
  await page.getByLabel('Runtime').selectOption({ label: 'Reverse proxy' });
  await page.getByLabel('Upstream').fill('not a url');
  await expect(page.getByLabel('Upstream')).toHaveJSProperty('validity.typeMismatch', true);
  await page.getByLabel('Runtime').selectOption({ label: 'Node.js' });
  await page.getByLabel('Entrypoint').fill('');
  await expect(page.getByLabel('Entrypoint')).toHaveJSProperty('validity.valueMissing', true);
});

test('a duplicate domain is refused and shown in the form', async ({ page }) => {
  const domain = newDomain();
  try {
    await openForm(page);
    await fillBasics(page, domain, newUser(), 'Static files');
    await page.getByRole('button', { name: 'Create site' }).click();
    await expect(row(page, domain)).toBeVisible();

    await openForm(page);
    await fillBasics(page, domain, newUser(), 'Static files');
    await page.getByRole('button', { name: 'Create site' }).click();
    await expect(page.locator('form').getByRole('alert')).toBeVisible();
    await expect(row(page, domain)).toHaveCount(1);
    expect((await apiGet(page, '/sites')).filter((s: { domain: string }) => s.domain === domain)).toHaveLength(1);
  } finally {
    await dropSite(page, domain);
  }
});

// ------------------------------------------------------------- Certificates

test('issue certificate asks for an email, enables TLS and hides the button', async ({ page }) => {
  const domain = newDomain();
  const created = await page.request.post('/api/sites', { data: { domain, owner_username: newUser(), runtime: { kind: 'static' } } });
  expect(created.status()).toBe(200);
  try {
    await page.reload();
    await openTab(page, 'Hosted sites');
    const siteRow = row(page, domain);
    await expect(siteRow.getByRole('cell').nth(3)).toHaveText('—');

    // Dismissing, or an empty address, issues nothing.
    const dismissed = nextDialog(page, 'dismiss');
    await siteRow.getByRole('button', { name: 'issue certificate' }).click();
    expect((await dismissed).type).toBe('prompt');
    const empty = nextDialog(page, 'accept', '   ');
    await siteRow.getByRole('button', { name: 'issue certificate' }).click();
    await empty;
    expect((await siteByDomain(page, domain)).ssl_enabled).toBe(false);

    const prompted = nextDialog(page, 'accept', 'ops@example.test');
    await siteRow.getByRole('button', { name: 'issue certificate' }).click();
    expect((await prompted).message).toContain(domain);
    await expect(siteRow.getByRole('cell').nth(3)).toHaveText('enabled');
    await expect(siteRow.getByRole('button', { name: 'issue certificate' })).toHaveCount(0);
    expect((await siteByDomain(page, domain)).ssl_enabled).toBe(true);
  } finally {
    await dropSite(page, domain);
  }
});

// ------------------------------------------------------------------- Delete

test('delete asks for confirmation and keeps the site when dismissed', async ({ page }) => {
  const domain = newDomain();
  await page.request.post('/api/sites', { data: { domain, owner_username: newUser(), runtime: { kind: 'static' } } });
  try {
    await page.reload();
    await openTab(page, 'Hosted sites');
    const dismissed = nextDialog(page, 'dismiss');
    await row(page, domain).getByRole('button', { name: 'delete' }).click();
    expect((await dismissed).type).toBe('confirm');
    await expect(row(page, domain)).toBeVisible();
    expect(await siteByDomain(page, domain)).toBeDefined();

    const accepted = nextDialog(page, 'accept');
    await row(page, domain).getByRole('button', { name: 'delete' }).click();
    await accepted;
    await expect(row(page, domain)).toHaveCount(0);
    expect(await siteByDomain(page, domain)).toBeUndefined();
  } finally {
    await dropSite(page, domain);
  }
});

test('deleting the managed site closes its detail panel', async ({ page }) => {
  const domain = newDomain();
  await page.request.post('/api/sites', { data: { domain, owner_username: newUser(), runtime: { kind: 'static' } } });
  try {
    await page.reload();
    await openTab(page, 'Hosted sites');
    await row(page, domain).getByRole('button', { name: 'manage' }).click();
    await expect(page.getByRole('heading', { name: domain })).toBeVisible();
    await expect(row(page, domain).getByRole('button', { name: 'close' })).toBeVisible();
    const accepted = nextDialog(page, 'accept');
    await row(page, domain).getByRole('button', { name: 'delete' }).click();
    await accepted;
    await expect(page.getByRole('heading', { name: domain })).toHaveCount(0);
    await expect(row(page, domain)).toHaveCount(0);
  } finally {
    await dropSite(page, domain);
  }
});

// ------------------------------------------------------------------- Manage

/** Creates a site through the API, opens its management panel, and cleans up afterwards. */
async function withManagedSite(page: Page, body: (domain: string) => Promise<void>) {
  const domain = newDomain();
  const created = await page.request.post('/api/sites', { data: { domain, owner_username: newUser(), runtime: { kind: 'static' } } });
  expect(created.status()).toBe(200);
  try {
    await page.reload();
    await openTab(page, 'Hosted sites');
    await row(page, domain).getByRole('button', { name: 'manage' }).click();
    await expect(page.getByRole('heading', { name: domain })).toBeVisible();
    await expect(page.getByText('Loading…')).toHaveCount(0);
    await body(domain);
  } finally {
    await dropSite(page, domain);
  }
}

test('manage opens and closes the detail panel', async ({ page }) => {
  await withManagedSite(page, async (domain) => {
    await expect(page.getByRole('heading', { name: 'Databases' })).toBeVisible();
    await expect(page.getByRole('heading', { name: 'Cron jobs' })).toBeVisible();
    await expect(page.getByText('No databases for this site.')).toBeVisible();
    await expect(page.getByText('No cron jobs for this site.')).toBeVisible();
    await row(page, domain).getByRole('button', { name: 'close' }).click();
    await expect(page.getByRole('heading', { name: domain })).toHaveCount(0);
  });
});

test('creates PostgreSQL and MySQL databases, shows the password once, and drops them', async ({ page }) => {
  await withManagedSite(page, async (domain) => {
    const site = await siteByDomain(page, domain);
    const pgName = unique('pg').replace(/-/g, '_');
    const myName = unique('my').replace(/-/g, '_');
    const form = page.locator('form').filter({ has: page.getByRole('button', { name: 'Create database' }) });

    // PostgreSQL (the default engine).
    await expect(form.getByLabel('Database engine')).toHaveValue('postgres');
    await form.getByLabel('Database name').fill(pgName);
    await form.getByLabel('Database user').fill(`${pgName}_u`);
    await form.getByRole('button', { name: 'Create database' }).click();
    const card = page.locator('.card').filter({ hasText: 'It is not shown again.' });
    await expect(card).toContainText(`Copy the password for ${pgName}_u now.`);
    const password = (await card.locator('.secret').innerText()).trim();
    expect(password.length).toBeGreaterThanOrEqual(12);
    await expect(row(page, pgName)).toContainText('PostgreSQL');
    await expect(row(page, pgName)).toContainText(`${pgName}_u`);
    await expect(form.getByLabel('Database name')).toHaveValue('');

    // The password is never stored or returned by the list.
    const stored = await apiGet(page, `/databases/site/${site.id}`);
    expect(JSON.stringify(stored)).not.toContain(password);

    // MySQL replaces the card with a new password.
    await form.getByLabel('Database engine').selectOption({ label: 'MySQL' });
    await form.getByLabel('Database name').fill(myName);
    await form.getByLabel('Database user').fill(`${myName}_u`);
    await form.getByRole('button', { name: 'Create database' }).click();
    await expect(card).toContainText(`Copy the password for ${myName}_u now.`);
    await expect(row(page, myName)).toContainText('MySQL');
    expect((await card.locator('.secret').innerText()).trim()).not.toBe(password);

    await card.getByRole('button', { name: 'Done' }).click();
    await expect(card).toHaveCount(0);

    // Drop: dismissed keeps it, accepted removes it.
    const dismissed = nextDialog(page, 'dismiss');
    await row(page, pgName).getByRole('button', { name: 'drop' }).click();
    expect((await dismissed).message).toContain(`Drop database ${pgName}`);
    await expect(row(page, pgName)).toBeVisible();
    for (const name of [pgName, myName]) {
      const accepted = nextDialog(page, 'accept');
      await row(page, name).getByRole('button', { name: 'drop' }).click();
      await accepted;
      await expect(row(page, name)).toHaveCount(0);
    }
    expect(await apiGet(page, `/databases/site/${site.id}`)).toEqual([]);
  });
});

test('database names are validated in the browser and duplicates are refused', async ({ page }) => {
  await withManagedSite(page, async () => {
    const form = page.locator('form').filter({ has: page.getByRole('button', { name: 'Create database' }) });
    const name = form.getByLabel('Database name');
    for (const bad of ['1db', 'has-dash', 'has space', 'a'.repeat(64)]) {
      await name.fill(bad);
      await expect(name).toHaveJSProperty('validity.patternMismatch', true);
    }
    await name.fill('ok_name');
    await expect(name).toHaveJSProperty('validity.patternMismatch', false);
    await form.getByLabel('Database user').fill('bad-user');
    await expect(form.getByLabel('Database user')).toHaveJSProperty('validity.patternMismatch', true);

    const dbName = unique('dup').replace(/-/g, '_');
    await name.fill(dbName);
    await form.getByLabel('Database user').fill(`${dbName}_u`);
    await form.getByRole('button', { name: 'Create database' }).click();
    await expect(row(page, dbName)).toBeVisible();
    await name.fill(dbName);
    await form.getByLabel('Database user').fill(`${dbName}_u`);
    await form.getByRole('button', { name: 'Create database' }).click();
    await expect(page.getByRole('alert')).toBeVisible();
    await expect(row(page, dbName)).toHaveCount(1);
  });
});

test('adds, pauses, resumes and deletes cron jobs', async ({ page }) => {
  await withManagedSite(page, async (domain) => {
    const site = await siteByDomain(page, domain);
    const command = `echo ${unique('job')}`;
    const form = page.locator('form').filter({ has: page.getByRole('button', { name: 'Add cron job' }) });
    await form.getByLabel('Cron schedule').fill('*/5   *  * * *');
    await form.getByLabel('Cron command').fill(command);
    await form.getByRole('button', { name: 'Add cron job' }).click();

    const job = row(page, command);
    await expect(job).toBeVisible();
    // Runs of whitespace in the schedule are collapsed.
    await expect(job.getByRole('cell').first()).toHaveText('*/5 * * * *');
    await expect(job).toContainText('enabled');
    await expect(form.getByLabel('Cron command')).toHaveValue('');

    await job.getByRole('button', { name: 'pause' }).click();
    await expect(job).toContainText('paused');
    expect((await apiGet(page, `/cron/site/${site.id}`))[0]).toMatchObject({ command, enabled: false });
    await job.getByRole('button', { name: 'resume' }).click();
    await expect(job).toContainText('enabled');
    expect((await apiGet(page, `/cron/site/${site.id}`))[0]).toMatchObject({ command, enabled: true });

    const dismissed = nextDialog(page, 'dismiss');
    await job.getByRole('button', { name: 'delete' }).click();
    expect((await dismissed).message).toContain(command);
    await expect(job).toBeVisible();
    const accepted = nextDialog(page, 'accept');
    await job.getByRole('button', { name: 'delete' }).click();
    await accepted;
    await expect(job).toHaveCount(0);
    expect(await apiGet(page, `/cron/site/${site.id}`)).toEqual([]);
  });
});

test('an invalid cron schedule is not kept in the list', async ({ page }) => {
  await withManagedSite(page, async (domain) => {
    const site = await siteByDomain(page, domain);
    const form = page.locator('form').filter({ has: page.getByRole('button', { name: 'Add cron job' }) });
    await form.getByLabel('Cron schedule').fill('* * *');
    await form.getByLabel('Cron command').fill('echo too-few-fields');
    await form.getByRole('button', { name: 'Add cron job' }).click();
    await expect(page.getByRole('alert')).toBeVisible();
    await expect(row(page, 'echo too-few-fields')).toHaveCount(0);
    expect(await apiGet(page, `/cron/site/${site.id}`)).toEqual([]);
  });
});

// ----------------------------------------------------------------- Firewall

const randomPort = () => 20000 + Math.floor(Math.random() * 40000);

test('firewall applies an allow rule, replaces it with deny, and validates the port', async ({ page }) => {
  const section = page.locator('section.panel').filter({ has: page.getByRole('heading', { name: 'Firewall' }) });
  await expect(section).toBeVisible();
  const port = randomPort();
  const description = unique('fw');
  const rule = () => section.getByRole('row').filter({ hasText: description });
  const apply = async (rule: 'allow' | 'deny', protocol: 'tcp' | 'udp', text: string) => {
    await section.getByLabel('Port').fill(String(port));
    await section.getByLabel('Protocol').selectOption(protocol);
    await section.getByLabel('Rule').selectOption(rule);
    await section.getByLabel('Description').fill(text);
    await section.getByRole('button', { name: 'Apply rule' }).click();
  };
  const stored = async () => (await apiGet(page, '/firewall')).filter((r: { port: number }) => r.port === port);

  await apply('allow', 'tcp', description);
  await expect(rule()).toBeVisible();
  await expect(rule()).toContainText(String(port));
  await expect(rule()).toContainText('tcp');
  await expect(rule().locator('.badge')).toHaveText('allow');
  await expect(section.getByLabel('Port')).toHaveValue('');
  expect(await stored()).toMatchObject([{ port, protocol: 'tcp', allow: true, description }]);

  // Same port and protocol as deny replaces the rule instead of adding a second one.
  await apply('deny', 'tcp', description);
  await expect(rule().locator('.badge')).toHaveText('deny');
  await expect(rule()).toHaveCount(1);
  expect(await stored()).toMatchObject([{ port, protocol: 'tcp', allow: false }]);
  expect(await stored()).toHaveLength(1);

  // The other protocol is a separate rule.
  await apply('allow', 'udp', description);
  await expect(rule()).toHaveCount(2);
  expect(await stored()).toHaveLength(2);

  // Out-of-range ports are blocked by the browser.
  let posts = 0;
  page.on('request', (r) => { if (r.method() === 'POST' && r.url().endsWith('/api/firewall')) posts += 1; });
  const portInput = section.getByLabel('Port');
  await portInput.fill('0');
  await section.getByRole('button', { name: 'Apply rule' }).click();
  await expect(portInput).toHaveJSProperty('validity.rangeUnderflow', true);
  await portInput.fill('65536');
  await section.getByRole('button', { name: 'Apply rule' }).click();
  await expect(portInput).toHaveJSProperty('validity.rangeOverflow', true);
  await portInput.fill('');
  await section.getByRole('button', { name: 'Apply rule' }).click();
  await expect(portInput).toHaveJSProperty('validity.valueMissing', true);
  expect(posts).toBe(0);
});
