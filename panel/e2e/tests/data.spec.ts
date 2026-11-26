import { expect, Locator, Page, test } from '@playwright/test';
import { openTab, signIn, unique, watchForFailures } from './helpers';

// Every confirm() the UI raises is accepted; tests that need the text read it from `dialogs`.
let dialogs: string[];
test.beforeEach(async ({ page }) => {
  dialogs = [];
  page.on('dialog', (dialog) => { dialogs.push(dialog.message()); void dialog.accept(); });
});

const slugOf = (name: string) => name.toLowerCase();
const row = (page: Page, text: string): Locator => page.locator('tr', { hasText: text });
const invalid = (field: Locator, key: string) => field.evaluate((el, k) => (el as HTMLInputElement).validity[k as 'valueMissing'], key);
const randomPort = () => 2000 + Math.floor(Math.random() * 20000);

// ---------------------------------------------------------------- Databases

const ENGINES: { label: string; uri: RegExp; hasDb: boolean; root: boolean }[] = [
  { label: 'PostgreSQL', uri: /postgres(ql)?:\/\//, hasDb: true, root: false },
  { label: 'MySQL', uri: /mysql:\/\//, hasDb: true, root: true },
  { label: 'MariaDB', uri: /(mysql|mariadb):\/\//, hasDb: true, root: true },
  { label: 'MongoDB', uri: /mongodb:\/\//, hasDb: false, root: false },
  { label: 'Redis', uri: /redis:\/\//, hasDb: false, root: false },
];

async function createDatabase(page: Page, engineLabel: string, name: string) {
  await openTab(page, 'Databases');
  await page.getByRole('button', { name: 'New database' }).click();
  await page.getByRole('group', { name: 'Engine' }).getByRole('button', { name: engineLabel, exact: true }).click();
  await page.getByLabel('Name', { exact: true }).fill(name);
  await page.getByRole('button', { name: 'Create and start' }).click();
  await expect(page.getByRole('heading', { name: new RegExp(`^${name}`) })).toBeVisible();
}

const detailHeading = (page: Page, name: string) => page.getByRole('heading', { name: new RegExp(`^${name}`) });

for (const engine of ENGINES) {
  test(`database: create ${engine.label}, show credentials, then delete`, async ({ page }) => {
    const failures = watchForFailures(page);
    await signIn(page);
    const name = unique(`db-${engine.label.toLowerCase()}`);
    await createDatabase(page, engine.label, name);

    await expect(row(page, slugOf(name))).toContainText(engine.label);
    await expect(row(page, slugOf(name))).toContainText(/127\.0\.0\.1:\d+/);
    await expect(row(page, slugOf(name))).toContainText('closed');

    // Credentials are hidden until asked for, then shown, then hideable.
    await expect(page.getByText('Password', { exact: true })).toHaveCount(0);
    await page.getByRole('button', { name: 'Show credentials' }).click();
    const secret = page.locator('.secret');
    await expect(secret.getByRole('row', { name: /^Host/ })).toContainText(/127\.0\.0\.1:\d+/);
    await expect(secret.getByRole('row', { name: /^Password/ })).toContainText(/\S{8,}/);
    await expect(secret.getByRole('row', { name: /^URI/ })).toContainText(engine.uri);
    if (engine.hasDb) {
      await expect(secret.getByRole('row', { name: /^Database/ })).toContainText('app');
      await expect(secret.getByRole('row', { name: /^User/ })).toContainText('app');
    }
    if (engine.root) await expect(secret.getByRole('row', { name: /^Root password/ })).toBeVisible();
    else await expect(secret.getByRole('row', { name: /^Root password/ })).toHaveCount(0);
    await secret.getByRole('button', { name: 'Hide' }).click();
    await expect(secret).toHaveCount(0);

    // The deployment finishes (mock agent) and the status settles.
    await expect(detailHeading(page, name).locator('.badge')).toHaveText('succeeded', { timeout: 30_000 });
    await expect(page.getByRole('heading', { name: 'Recent deployments' })).toBeVisible();

    await page.getByRole('button', { name: 'Delete', exact: true }).click();
    expect(dialogs.at(-1)).toContain(name);
    await expect(row(page, slugOf(name))).toHaveCount(0);
    await expect(detailHeading(page, name)).toHaveCount(0);
    expect(failures).toEqual([]);
  });
}

test('database: create form validation and cancel', async ({ page }) => {
  await signIn(page);
  await openTab(page, 'Databases');
  await page.getByRole('button', { name: 'New database' }).click();
  // Required name: submitting empty does not create anything.
  await page.getByRole('button', { name: 'Create and start' }).click();
  expect(await invalid(page.getByLabel('Name', { exact: true }), 'valueMissing')).toBe(true);
  // Bad slug pattern and too-small memory are blocked by the browser.
  await page.getByLabel('Name', { exact: true }).fill(unique('db-val'));
  await page.getByLabel('Slug').fill('');
  await page.getByLabel(/Memory limit/).fill('10');
  expect(await invalid(page.getByLabel(/Memory limit/), 'rangeUnderflow')).toBe(true);
  // Redis has no database/user fields.
  await page.getByRole('group', { name: 'Engine' }).getByRole('button', { name: 'Redis' }).click();
  await expect(page.getByLabel('Database name')).toHaveCount(0);
  await page.getByRole('group', { name: 'Engine' }).getByRole('button', { name: 'PostgreSQL' }).click();
  await expect(page.getByLabel('Database name')).toBeVisible();
  await page.getByRole('button', { name: 'Cancel' }).click();
  await expect(page.getByRole('button', { name: 'New database' })).toBeVisible();
});

test('database: a malformed slug is blocked by the browser before any request', async ({ page }) => {
  await signIn(page);
  await openTab(page, 'Databases');
  await page.getByRole('button', { name: 'New database' }).click();
  await page.getByLabel('Name', { exact: true }).fill(unique('db-slug'));
  await page.getByLabel('Slug').fill('A_bad slug');
  expect(await invalid(page.getByLabel('Slug'), 'patternMismatch')).toBe(true);
  let posted = false;
  page.on('request', (r) => { if (r.method() === 'POST' && r.url().includes('/database-services')) posted = true; });
  await page.getByRole('button', { name: 'Create and start' }).click();
  await expect(page.getByLabel('Slug')).toBeFocused();
  expect(posted).toBe(false);
  await page.getByLabel('Slug').fill('good-slug-1');
  expect(await invalid(page.getByLabel('Slug'), 'patternMismatch')).toBe(false);
});

test('database: a duplicate slug is rejected with a message', async ({ page }) => {
  await signIn(page);
  const name = unique('db-dup');
  await createDatabase(page, 'Redis', name);
  await page.getByRole('button', { name: 'New database' }).click();
  await page.getByLabel('Name', { exact: true }).fill(`${name} again`);
  await page.getByLabel('Slug').fill(slugOf(name));
  await page.getByRole('button', { name: 'Create and start' }).click();
  await expect(page.getByRole('alert')).toContainText('already exists');
  await page.getByRole('button', { name: 'Cancel' }).click();
  await expect(detailHeading(page, name).locator('.badge')).toHaveText('succeeded', { timeout: 30_000 });
  await page.getByRole('button', { name: 'Delete', exact: true }).click();
  await expect(row(page, slugOf(name))).toHaveCount(0);
});

test('database: outside access can be opened, changed and closed', async ({ page }) => {
  const failures = watchForFailures(page);
  await signIn(page);
  const name = unique('db-expose');
  await createDatabase(page, 'PostgreSQL', name);
  await expect(detailHeading(page, name).locator('.badge')).toHaveText('succeeded', { timeout: 30_000 });

  const access = page.locator('form', { has: page.getByRole('heading', { name: /Outside access/ }) });
  await expect(access.locator('.badge')).toHaveText('closed');
  await expect(access.getByRole('button', { name: 'Open access' })).toBeDisabled();
  await expect(access.getByRole('button', { name: 'Close access' })).toHaveCount(0);

  const port = randomPort();
  await access.getByLabel('Public port').fill(String(port));
  await access.getByLabel(/Allowed addresses/).fill('203.0.113.4\n10.0.0.0/8');
  await access.getByRole('button', { name: 'Open access' }).click();
  await expect(access.locator('.badge')).toHaveText('open');
  await expect(row(page, slugOf(name))).toContainText(`port ${port} · 203.0.113.4, 10.0.0.0/8`);
  await expect(access.getByRole('button', { name: 'Update access' })).toBeVisible();

  // The credentials show an outside URI once open.
  await page.getByRole('button', { name: 'Show credentials' }).click();
  await expect(page.locator('.secret').getByRole('row', { name: /^Outside URI/ })).toContainText(String(port));
  await page.locator('.secret').getByRole('button', { name: 'Hide' }).click();

  // Empty sources = any address, behind a confirmation.
  await access.getByLabel(/Allowed addresses/).fill('');
  await access.getByRole('button', { name: 'Update access' }).click();
  expect(dialogs.at(-1)).toContain('any address');
  await expect(row(page, slugOf(name))).toContainText('any address');

  await access.getByRole('button', { name: 'Close access' }).click();
  await expect(access.locator('.badge')).toHaveText('closed');
  await expect(row(page, slugOf(name))).toContainText('closed');
  await expect(access.getByLabel('Public port')).toHaveValue('');

  await page.getByRole('button', { name: 'Delete', exact: true }).click();
  await expect(row(page, slugOf(name))).toHaveCount(0);
  expect(failures).toEqual([]);
});

test('database: outside access rejects a bad address and a reserved port', async ({ page }) => {
  await signIn(page);
  const name = unique('db-badexp');
  await createDatabase(page, 'Redis', name);
  await expect(detailHeading(page, name).locator('.badge')).toHaveText('succeeded', { timeout: 30_000 });
  const access = page.locator('form', { has: page.getByRole('heading', { name: /Outside access/ }) });

  await access.getByLabel('Public port').fill(String(randomPort()));
  await access.getByLabel(/Allowed addresses/).fill('not-an-ip');
  await access.getByRole('button', { name: 'Open access' }).click();
  await expect(access.getByRole('alert')).toBeVisible();
  await expect(access.locator('.badge')).toHaveText('closed');

  await access.getByLabel('Public port').fill('8080');
  await access.getByLabel(/Allowed addresses/).fill('203.0.113.4');
  await access.getByRole('button', { name: 'Open access' }).click();
  await expect(access.getByRole('alert')).toContainText('public port');
  await expect(access.locator('.badge')).toHaveText('closed');

  await page.getByRole('button', { name: 'Delete', exact: true }).click();
  await expect(row(page, slugOf(name))).toHaveCount(0);
});

test('database: stop, start and restart from the detail panel', async ({ page }) => {
  const failures = watchForFailures(page);
  await signIn(page);
  const name = unique('db-life');
  await createDatabase(page, 'MariaDB', name);
  const badge = detailHeading(page, name).locator('.badge');
  await expect(badge).toHaveText('succeeded', { timeout: 30_000 });

  await page.getByRole('button', { name: 'Stop', exact: true }).click();
  await expect(page.getByRole('status').filter({ hasText: 'Stopped.' })).toBeVisible();
  await expect(badge).toHaveText('stopped');
  await expect(row(page, slugOf(name)).locator('.badge')).toHaveText('stopped');

  await page.getByRole('button', { name: 'Start', exact: true }).click();
  await expect(page.getByRole('status').filter({ hasText: 'Start queued.' })).toBeVisible();
  await expect(badge).toHaveText('succeeded', { timeout: 30_000 });

  await page.getByRole('button', { name: 'Restart', exact: true }).click();
  await expect(page.getByRole('status').filter({ hasText: 'Restarted.' })).toBeVisible();
  await expect(page.getByRole('heading', { name: 'Recent deployments' })).toBeVisible();

  await page.getByRole('button', { name: 'Delete', exact: true }).click();
  await expect(row(page, slugOf(name))).toHaveCount(0);
  expect(failures).toEqual([]);
});

// ------------------------------------------------------------------ Backups

async function fillDestination(page: Page, name: string, over: Record<string, string> = {}) {
  const v = { endpoint: 'http://127.0.0.1:9000', region: 'us-east-1', bucket: 'e2e-bucket', prefix: 'scontrol',
    retention: '7', access: 'AKIAE2ETESTKEY123', secret: 'e2e-secret-key-value', ...over };
  await page.getByLabel('Name', { exact: true }).fill(name);
  await page.getByLabel('Endpoint').fill(v.endpoint);
  await page.getByLabel('Region').fill(v.region);
  await page.getByLabel('Bucket').fill(v.bucket);
  await page.getByLabel('Key prefix').fill(v.prefix);
  await page.getByLabel(/Backups to keep/).fill(v.retention);
  await page.getByLabel('Access key ID').fill(v.access);
  await page.getByLabel('Secret access key').fill(v.secret);
}

async function addDestination(page: Page, name: string, over: Record<string, string> = {}) {
  await openTab(page, 'Backups');
  await page.getByRole('button', { name: 'Add destination' }).click();
  await fillDestination(page, name, over);
  await page.getByRole('button', { name: 'Create destination' }).click();
  await expect(row(page, name)).toBeVisible();
}

async function deleteDestination(page: Page, name: string) {
  await row(page, name).getByRole('button', { name: 'delete' }).click();
  await expect(row(page, name)).toHaveCount(0);
}

test('backups: create, test, edit and delete an S3 destination', async ({ page }) => {
  const failures = watchForFailures(page);
  await signIn(page);
  const name = unique('s3');
  await addDestination(page, name, { retention: '3', prefix: 'e2e/area' });
  await expect(row(page, name)).toContainText('e2e-bucket');
  await expect(row(page, name)).toContainText('http://127.0.0.1:9000');
  await expect(row(page, name)).toContainText('e2e/area');
  await expect(row(page, name).locator('td').nth(3)).toHaveText('3');
  // Keys are write-only: nothing of them is rendered.
  await expect(page.getByText('AKIAE2ETESTKEY123')).toHaveCount(0);

  await row(page, name).getByRole('button', { name: 'test' }).click();
  await expect(page.getByRole('status').filter({ hasText: `${name}: bucket reachable.` })).toBeVisible();

  // Edit: keys stay stored when left empty; retention 0 shows as "all".
  await row(page, name).getByRole('button', { name: 'edit' }).click();
  await expect(page.getByLabel('Access key ID')).toHaveValue('');
  await expect(page.getByLabel('Access key ID')).toHaveAttribute('placeholder', /stored; leave empty to keep/);
  await expect(page.getByLabel('Secret access key')).toHaveAttribute('placeholder', /stored; leave empty to keep/);
  await expect(page.getByLabel('Secret access key')).toHaveValue('');
  await expect(page.getByLabel('Name', { exact: true })).toHaveValue(name);
  await page.getByLabel(/Backups to keep/).fill('0');
  await page.getByLabel('Key prefix').fill('e2e/renamed');
  await page.getByRole('button', { name: 'Save destination' }).click();
  await expect(row(page, name)).toContainText('e2e/renamed');
  await expect(row(page, name).locator('td').nth(3)).toHaveText('all');
  expect((await (await page.request.get('/api/backups/destinations')).json()).find((d: { name: string }) => d.name === name).has_credential).toBe(true);

  // Moving the destination to another bucket needs the keys again (stored keys never follow).
  await row(page, name).getByRole('button', { name: 'edit' }).click();
  await page.getByLabel('Bucket').fill('renamed-bucket');
  await expect(page.getByLabel('Access key ID')).toHaveAttribute('placeholder', 'enter it again: the endpoint or bucket changed');
  await expect(page.getByLabel('Secret access key')).toHaveAttribute('placeholder', 'enter it again: the endpoint or bucket changed');
  await page.getByRole('button', { name: 'Save destination' }).click();
  await expect(page.getByRole('alert')).toContainText('enter the keys again');
  await page.getByLabel('Access key ID').fill('AKIAE2ETESTKEY456');
  await page.getByLabel('Secret access key').fill('another-secret');
  await page.getByRole('button', { name: 'Save destination' }).click();
  await expect(row(page, name)).toContainText('renamed-bucket');

  await deleteDestination(page, name);
  expect(dialogs.at(-1)).toContain(name);
  expect(failures).toEqual([]);
});

test('backups: destination form validation, presets, test connection and duplicate name', async ({ page }) => {
  await signIn(page);
  await openTab(page, 'Backups');
  await page.getByRole('button', { name: 'Add destination' }).click();

  // Presets fill endpoint/region/name.
  await page.getByRole('group', { name: 'Presets' }).getByRole('button', { name: 'MinIO' }).click();
  await expect(page.getByLabel('Endpoint')).toHaveValue('http://127.0.0.1:9000');
  await expect(page.getByLabel('Name', { exact: true })).toHaveValue('MinIO');
  await page.getByLabel('Name', { exact: true }).fill('');

  // Test connection needs keys, endpoint and bucket.
  await expect(page.getByRole('button', { name: 'Test connection' })).toBeDisabled();
  await fillDestination(page, unique('s3-val'));
  await expect(page.getByRole('button', { name: 'Test connection' })).toBeEnabled();
  await page.getByRole('button', { name: 'Test connection' }).click();
  await expect(page.getByRole('status').filter({ hasText: 'Bucket reachable.' })).toBeVisible();

  // Retention is limited to 0..1000 by the field and by the API.
  await page.getByLabel(/Backups to keep/).fill('1001');
  expect(await invalid(page.getByLabel(/Backups to keep/), 'rangeOverflow')).toBe(true);
  await page.getByLabel(/Backups to keep/).fill('-1');
  expect(await invalid(page.getByLabel(/Backups to keep/), 'rangeUnderflow')).toBe(true);
  await page.getByLabel(/Backups to keep/).fill('7');

  // Server-side validation errors surface in the form.
  await page.getByLabel('Endpoint').fill('ftp://nope');
  await page.getByRole('button', { name: 'Create destination' }).click();
  await expect(page.getByRole('alert')).toContainText('endpoint must look like');
  await page.getByLabel('Endpoint').fill('http://127.0.0.1:9000');
  await page.getByLabel('Bucket').fill('Bad_Bucket!');
  await page.getByRole('button', { name: 'Create destination' }).click();
  await expect(page.getByRole('alert')).toContainText('invalid bucket name');
  await page.getByLabel('Bucket').fill('e2e-bucket');
  await page.getByLabel('Key prefix').fill('bad prefix!');
  await page.getByRole('button', { name: 'Create destination' }).click();
  await expect(page.getByRole('alert')).toContainText('prefix must be');
  await page.getByLabel('Key prefix').fill('scontrol');
  await page.getByLabel('Region').fill('Bad Region');
  await page.getByRole('button', { name: 'Create destination' }).click();
  await expect(page.getByRole('alert')).toContainText('region must be');
  await page.getByLabel('Region').fill('us-east-1');
  await page.getByLabel('Secret access key').fill('has space');
  await page.getByRole('button', { name: 'Create destination' }).click();
  await expect(page.getByRole('alert')).toContainText('secret key must be');
  await page.getByRole('button', { name: 'Cancel' }).click();

  // Duplicate name.
  const name = unique('s3-dup');
  await addDestination(page, name);
  await page.getByRole('button', { name: 'Add destination' }).click();
  await fillDestination(page, name);
  await page.getByRole('button', { name: 'Create destination' }).click();
  await expect(page.getByRole('alert')).toContainText('already exists');
  await page.getByRole('button', { name: 'Cancel' }).click();
  await deleteDestination(page, name);
});

test('backups: back up a database now, see it in the run history', async ({ page }) => {
  const failures = watchForFailures(page);
  await signIn(page);
  const name = unique('s3-run');
  await addDestination(page, name, { prefix: 'e2e-run' });
  await row(page, name).getByRole('button', { name: 'backups' }).click();
  await expect(page.getByRole('heading', { name: `${name}: backups` })).toBeVisible();
  await expect(page.getByText('No runs yet.')).toBeVisible();

  const form = page.locator('form', { has: page.getByRole('button', { name: 'Back up now' }) });
  await form.getByLabel('Database', { exact: true }).fill('shop');
  await form.getByLabel('Username').fill('app');
  await form.getByLabel('Password').fill('pw-for-this-run');
  await form.getByRole('button', { name: 'Back up now' }).click();
  await expect(page.getByRole('status').filter({ hasText: 'Backup started.' })).toBeVisible();

  const runs = page.locator('table', { has: page.getByRole('columnheader', { name: 'Kind' }) });
  await expect(runs.getByRole('row', { name: /DB backup/ })).toBeVisible();
  await expect(runs.getByRole('row', { name: /DB backup/ })).toContainText('succeeded', { timeout: 30_000 });
  await expect(runs.getByRole('row', { name: /DB backup/ })).toContainText('e2e-run/');

  // Switching to the volume form keeps the same destination.
  await page.getByRole('group', { name: 'Backup type' }).getByRole('button', { name: 'Volume' }).click();
  await expect(page.getByLabel('Volume name')).toBeVisible();
  await page.getByLabel('Project slug').fill('some-project');
  await page.getByLabel('Volume name').fill('data');
  await page.getByRole('button', { name: 'Back up now' }).click();
  await expect(runs.getByRole('row', { name: /Volume backup/ })).toContainText('succeeded', { timeout: 30_000 });

  await openTab(page, 'Backups');
  await deleteDestination(page, name);
  await expect(page.getByRole('heading', { name: `${name}: backups` })).toHaveCount(0);
  expect(failures).toEqual([]);
});

test('backups: an invalid database backup is refused with a message', async ({ page }) => {
  await signIn(page);
  const name = unique('s3-bad');
  await addDestination(page, name);
  await row(page, name).getByRole('button', { name: 'backups' }).click();
  const form = page.locator('form', { has: page.getByRole('button', { name: 'Back up now' }) });
  await form.getByLabel('Host').fill('bad host!');
  await form.getByLabel('Database', { exact: true }).fill('shop');
  await form.getByLabel('Username').fill('app');
  await form.getByLabel('Password').fill('x');
  await form.getByRole('button', { name: 'Back up now' }).click();
  await expect(page.getByRole('alert')).toContainText('invalid database host');
  await deleteDestination(page, name);
});

test('backups: restore a database and a volume backup from the bucket listing', async ({ page }) => {
  const failures = watchForFailures(page);
  await signIn(page);
  const name = unique('s3-restore');
  await addDestination(page, name, { prefix: 'e2e-restore' });
  await row(page, name).getByRole('button', { name: 'backups' }).click();
  await expect(page.getByRole('heading', { name: `${name}: backups` })).toBeVisible();
  const runs = page.locator('table', { has: page.getByRole('columnheader', { name: 'Kind' }) });
  const objects = page.locator('table', { has: page.getByRole('columnheader', { name: 'Modified' }) });

  // Back up a database and a volume; the bucket listing then offers both objects.
  const form = page.locator('form', { has: page.getByRole('button', { name: 'Back up now' }) });
  await form.getByLabel('Database', { exact: true }).fill('shop');
  await form.getByLabel('Username').fill('app');
  await form.getByLabel('Password').fill('pw-backup');
  await form.getByRole('button', { name: 'Back up now' }).click();
  await expect(runs.getByRole('row', { name: /DB backup/ })).toContainText('succeeded', { timeout: 30_000 });
  await page.getByRole('group', { name: 'Backup type' }).getByRole('button', { name: 'Volume' }).click();
  await page.getByLabel('Project slug').fill('some-project');
  await page.getByLabel('Volume name').fill('data');
  await page.getByRole('button', { name: 'Back up now' }).click();
  await expect(runs.getByRole('row', { name: /Volume backup/ })).toContainText('succeeded', { timeout: 30_000 });

  // The listing may refresh only on reopening; reopen the destination to be deterministic.
  await openTab(page, 'Backups');
  await row(page, name).getByRole('button', { name: 'backups' }).click();
  const dbObject = objects.getByRole('row', { name: /\.sql\.gz|\.archive|\.gz/ }).filter({ hasNotText: '.tar.gz' });
  const volObject = objects.getByRole('row', { name: /\.tar\.gz/ });
  await expect(dbObject.first()).toBeVisible();
  await expect(dbObject.first()).toContainText('2.0 KiB');
  await expect(volObject.first()).toBeVisible();

  // Database restore: form, confirmation, run in history.
  await dbObject.first().getByRole('button', { name: 'restore' }).click();
  const card = page.locator('.card', { has: page.getByRole('heading', { name: /^Restore / }) });
  await expect(card).toBeVisible();
  await card.getByLabel('Database', { exact: true }).fill('shop');
  await card.getByLabel('Username').fill('app');
  await card.getByLabel('Password').fill('pw-restore');
  await card.getByRole('button', { name: 'Restore', exact: true }).click();
  expect(dialogs.at(-1)).toContain('Restore ');
  expect(dialogs.at(-1)).toContain('database contents');
  await expect(card).toHaveCount(0);
  await expect(runs.getByRole('row', { name: /DB restore/ })).toContainText('succeeded', { timeout: 30_000 });

  // Volume restore.
  await volObject.first().getByRole('button', { name: 'restore' }).click();
  const vcard = page.locator('.card', { has: page.getByRole('heading', { name: /^Restore / }) });
  await vcard.getByLabel('Project slug').fill('some-project');
  await vcard.getByLabel('Volume name').fill('data');
  await vcard.getByRole('button', { name: 'Restore', exact: true }).click();
  expect(dialogs.at(-1)).toContain('volume contents');
  await expect(vcard).toHaveCount(0);
  await expect(runs.getByRole('row', { name: /Volume restore/ })).toContainText('succeeded', { timeout: 30_000 });

  await openTab(page, 'Backups');
  await deleteDestination(page, name);
  expect(failures).toEqual([]);
});

// --------------------------------------------------------------- Registries

async function addRegistry(page: Page, name: string, over: Record<string, string> = {}) {
  const v = { server: 'registry.example.com:5000', username: 'e2e-user', password: 'e2e-token-123', ...over };
  await openTab(page, 'Registries');
  await page.getByRole('button', { name: 'Add registry' }).click();
  await page.getByLabel('Name', { exact: true }).fill(name);
  await page.getByLabel('Server').fill(v.server);
  await page.getByLabel('Username').fill(v.username);
  await page.getByLabel('Password or token').fill(v.password);
  await page.getByRole('button', { name: 'Create registry' }).click();
  await expect(row(page, name)).toBeVisible();
}

test('registries: create, test login, edit with write-only password, delete', async ({ page }) => {
  const failures = watchForFailures(page);
  await signIn(page);
  const name = unique('reg');
  await addRegistry(page, name);
  await expect(row(page, name)).toContainText('registry.example.com:5000');
  await expect(row(page, name)).toContainText('e2e-user');
  await expect(page.getByText('e2e-token-123')).toHaveCount(0);

  await row(page, name).getByRole('button', { name: 'test' }).click();
  await expect(page.getByRole('status').filter({ hasText: 'Login to registry.example.com:5000 succeeded.' })).toBeVisible();

  await row(page, name).getByRole('button', { name: 'edit' }).click();
  await expect(page.getByLabel('Password or token')).toHaveValue('');
  await expect(page.getByLabel('Password or token')).toHaveAttribute('placeholder', 'stored; leave empty to keep');
  await expect(page.getByLabel('Password or token')).not.toHaveAttribute('required', '');
  // Test login needs a typed password; saved ones are tested from the list.
  await expect(page.getByRole('button', { name: 'Test login' })).toBeDisabled();
  // Renaming alone keeps the stored password; changing the username demands it again.
  await page.getByLabel('Name', { exact: true }).fill(`${name}-x`);
  await page.getByRole('button', { name: 'Save registry' }).click();
  await expect(row(page, `${name}-x`)).toBeVisible();
  await row(page, `${name}-x`).getByRole('button', { name: 'edit' }).click();
  await page.getByLabel('Username').fill('renamed-user');
  await expect(page.getByLabel('Password or token')).toHaveAttribute('placeholder', 'enter it again: the server or username changed');
  await page.getByRole('button', { name: 'Save registry' }).click();
  await expect(page.getByRole('alert')).toContainText('enter the password again');
  await page.getByLabel('Password or token').fill('new-token-456');
  await page.getByRole('button', { name: 'Save registry' }).click();
  await expect(row(page, `${name}-x`)).toContainText('renamed-user');
  const saved = (await (await page.request.get('/api/registries')).json()).find((r: { name: string }) => r.name === `${name}-x`);
  expect(saved.has_credential).toBe(true);
  expect(JSON.stringify(saved)).not.toContain('e2e-token-123');

  await row(page, `${name}-x`).getByRole('button', { name: 'delete' }).click();
  expect(dialogs.at(-1)).toContain(name);
  await expect(row(page, `${name}-x`)).toHaveCount(0);
  expect(failures).toEqual([]);
});

test('registries: form validation, presets, test typed login and duplicate name', async ({ page }) => {
  await signIn(page);
  await openTab(page, 'Registries');
  await page.getByRole('button', { name: 'Add registry' }).click();
  await page.getByRole('group', { name: 'Presets' }).getByRole('button', { name: 'AWS ECR' }).click();
  await expect(page.getByLabel('Username')).toHaveValue('AWS');
  await expect(page.getByLabel('Name', { exact: true })).toHaveValue('AWS ECR');
  await page.getByRole('group', { name: 'Presets' }).getByRole('button', { name: 'GitHub (ghcr.io)' }).click();
  await expect(page.getByLabel('Server')).toHaveValue('ghcr.io');
  await expect(page.getByLabel('Name', { exact: true })).toHaveValue('AWS ECR'); // already named: kept

  await expect(page.getByRole('button', { name: 'Test login' })).toBeDisabled();
  await page.getByLabel('Name', { exact: true }).fill(unique('reg-val'));
  await page.getByLabel('Password or token').fill('tok');
  await expect(page.getByRole('button', { name: 'Test login' })).toBeEnabled();
  await page.getByRole('button', { name: 'Test login' }).click();
  await expect(page.getByRole('status').filter({ hasText: 'Login succeeded.' })).toBeVisible();

  await page.getByLabel('Server').fill('not a host!');
  await page.getByRole('button', { name: 'Create registry' }).click();
  await expect(page.getByRole('alert')).toContainText('server must be a registry host');
  await page.getByLabel('Server').fill('ghcr.io');
  await page.getByLabel('Username').fill('bad user name');
  await page.getByRole('button', { name: 'Create registry' }).click();
  await expect(page.getByRole('alert')).toContainText('username may contain');
  await page.getByLabel('Username').fill('ok');
  await page.getByLabel('Password or token').fill('has space');
  await page.getByRole('button', { name: 'Create registry' }).click();
  await expect(page.getByRole('alert')).toContainText('password must be');
  await page.getByRole('button', { name: 'Cancel' }).click();
  await expect(page.getByRole('button', { name: 'Add registry' })).toBeVisible();

  const name = unique('reg-dup');
  await addRegistry(page, name);
  await page.getByRole('button', { name: 'Add registry' }).click();
  await page.getByLabel('Name', { exact: true }).fill(name);
  await page.getByLabel('Server').fill('ghcr.io');
  await page.getByLabel('Username').fill('u');
  await page.getByLabel('Password or token').fill('p');
  await page.getByRole('button', { name: 'Create registry' }).click();
  await expect(page.getByRole('alert')).toContainText('already exists');
  await page.getByRole('button', { name: 'Cancel' }).click();
  await row(page, name).getByRole('button', { name: 'delete' }).click();
  await expect(row(page, name)).toHaveCount(0);
});

test('backups: schedule a database backup, run it now, pause, edit and delete it', async ({ page }) => {
  const failures = watchForFailures(page);
  await signIn(page);
  const database = unique('sched-db');
  await createDatabase(page, 'PostgreSQL', database);
  const dest = unique('s3-sched');
  await addDestination(page, dest, { prefix: 'e2e-sched' });

  const section = page.locator('section', { has: page.getByRole('heading', { name: 'Scheduled backups' }) });
  await section.getByRole('button', { name: 'Add schedule' }).click();
  const form = section.locator('form');
  const name = unique('nightly');
  await form.getByLabel('Name', { exact: true }).fill(name);
  const service = form.getByLabel('Database service');
  const option = await service.locator('option', { hasText: database }).getAttribute('value');
  await service.selectOption(option!);
  await form.getByLabel('S3 destination').selectOption({ label: dest });
  await form.getByLabel(/Backups to keep/).fill('3');
  await form.getByLabel('Time zone').fill('UTC');

  // The expression is checked as it is typed, and the next runs are shown.
  await form.getByLabel(/Cron expression/).fill('0 3 * *');
  await expect(form.getByText(/five fields/)).toBeVisible();
  await form.getByLabel(/Cron expression/).fill('0 3 * * *');
  await expect(form.getByText(/^Next runs:/)).toBeVisible();
  await form.getByRole('button', { name: 'Create schedule' }).click();

  const scheduleRow = section.locator('tr', { hasText: name });
  await expect(scheduleRow).toContainText('0 3 * * *');
  await expect(scheduleRow).toContainText(`${slugOf(database)} · postgres/app`);
  await expect(scheduleRow).toContainText(dest);
  await expect(scheduleRow).toContainText('never');

  // Run now: the run uses the service's stored credentials and is listed as scheduled.
  await scheduleRow.getByRole('button', { name: 'run now' }).click();
  await expect(section.getByRole('status').filter({ hasText: `${name}: backup started.` })).toBeVisible();
  const runs = section.locator('.card', { has: page.getByRole('heading', { name: `Runs of ${name}` }) });
  await expect(runs.getByRole('row').nth(1)).toContainText('succeeded', { timeout: 30_000 });
  await expect(runs.getByRole('row').nth(1)).toContainText(`e2e-sched/postgres-svc-${slugOf(database)}`);
  await expect(scheduleRow).toContainText('succeeded', { timeout: 30_000 });
  await runs.getByRole('button', { name: 'Close' }).click();

  // Pause and resume.
  await scheduleRow.getByRole('button', { name: 'pause' }).click();
  await expect(scheduleRow).toContainText('paused');
  await scheduleRow.getByRole('button', { name: 'resume' }).click();
  await expect(scheduleRow.getByRole('button', { name: 'pause' })).toBeVisible();

  // Edit the retention and the expression; the target stays fixed.
  await scheduleRow.getByRole('button', { name: 'edit' }).click();
  await expect(form.getByText(/fixed for the life of a schedule/)).toBeVisible();
  await form.getByLabel(/Backups to keep/).fill('5');
  await form.getByLabel(/Cron expression/).fill('30 2 * * 0');
  await form.getByRole('button', { name: 'Save schedule' }).click();
  await expect(scheduleRow).toContainText('30 2 * * 0');
  const saved = (await (await page.request.get('/api/backups/schedules')).json()).find((s: { name: string }) => s.name === name);
  expect(saved.retention_count).toBe(5);
  expect(JSON.stringify(saved)).not.toMatch(/password|secret/i);

  await scheduleRow.getByRole('button', { name: 'delete' }).click();
  await expect(scheduleRow).toHaveCount(0);
  await deleteDestination(page, dest);
  await openTab(page, 'Databases');
  await row(page, slugOf(database)).click();
  await page.getByRole('button', { name: 'Delete', exact: true }).click();
  await expect(row(page, slugOf(database))).toHaveCount(0);
  expect(failures).toEqual([]);
});
