import { expect, Page, test } from '@playwright/test';
import { openTab, signIn, unique, watchForFailures } from './helpers';

// Compose projects tab: creating services from every source kind, deploy
// history, rollback, logs, variables, Git access and the push webhook.
// The host agent is a mock that keeps pretend host state: a deployment lists
// the project's services as running, writes a short log, and a raw Compose file
// containing `# e2e: slow` keeps deploying (up to 30 s) until it is cancelled.

const COMPOSE_TAB = 'Compose projects';

async function openList(page: Page) {
  await signIn(page);
  await openTab(page, COMPOSE_TAB);
  await expect(page.getByRole('heading', { name: COMPOSE_TAB })).toBeVisible();
}

const form = (page: Page) => page.locator('form.card').filter({ has: page.getByRole('radiogroup', { name: 'Source' }) });
const statusBadge = (page: Page) => page.locator('.panel-title .badge');
const deploymentRows = (page: Page) => page.locator('table').filter({ hasText: 'Trigger' }).locator('tbody tr');

async function openForm(page: Page, name: string, source: string) {
  await page.getByRole('button', { name: 'New project' }).click();
  await form(page).getByLabel('Name', { exact: true }).fill(name);
  await form(page).getByRole('radio', { name: source, exact: true }).click();
}

async function submit(page: Page) {
  await form(page).getByRole('button', { name: /^Create/ }).click();
}

async function expectDeployed(page: Page, name: string) {
  await expect(page.getByRole('heading', { name })).toBeVisible();
  await expect(statusBadge(page)).toHaveText('succeeded');
}

/** Options of the Container logs service dropdown (the terminal panel has its own, later select). */
const logServiceOptions = (page: Page) => page.getByLabel('Service', { exact: true }).first().locator('option');

async function backToList(page: Page) {
  await page.getByRole('button', { name: '← All projects' }).click();
}

/** Deletes the open project through the UI (the prompt wants the slug). */
async function deleteOpenProject(page: Page, slug: string) {
  page.once('dialog', (dialog) => dialog.accept(slug));
  await page.getByRole('button', { name: 'Delete', exact: true }).click();
  await expect(page.getByRole('button', { name: 'New project' })).toBeVisible();
}

async function removeFromList(page: Page, slug: string) {
  await page.getByRole('button', { name: slug, exact: true }).click();
  await deleteOpenProject(page, slug);
  await expect(page.getByRole('cell', { name: slug, exact: true })).toHaveCount(0);
}

test('lists projects and toggles the create form', async ({ page }) => {
  const failures = watchForFailures(page);
  await openList(page);
  await expect(page.getByRole('columnheader', { name: 'Environment' })).toBeVisible();
  await page.getByRole('button', { name: 'New project' }).click();
  await expect(form(page)).toBeVisible();
  await expect(form(page).getByRole('radio')).toHaveCount(6);
  await page.getByRole('button', { name: 'Close' }).click();
  await expect(form(page)).toHaveCount(0);
  expect(failures).toEqual([]);
});

test('create form: name derives the slug and the button follows the deploy checkbox', async ({ page }) => {
  await openList(page);
  await page.getByRole('button', { name: 'New project' }).click();
  await form(page).getByLabel('Name', { exact: true }).fill('My Cool_App 2');
  await expect(form(page).getByLabel('Slug')).toHaveValue('my-cool-app-2');
  await expect(form(page).getByRole('button', { name: 'Create' })).toBeVisible();
  await form(page).getByRole('radio', { name: 'Raw Compose' }).click();
  await expect(form(page).getByRole('button', { name: 'Create and deploy' })).toBeVisible();
  await form(page).getByLabel('deploy right away').uncheck();
  await expect(form(page).getByRole('button', { name: 'Create', exact: true })).toBeVisible();
});

test('create form: browser validation blocks empty and malformed fields', async ({ page }) => {
  await openList(page);
  await page.getByRole('button', { name: 'New project' }).click();
  await form(page).getByRole('radio', { name: 'Raw Compose' }).click();
  await submit(page);
  // Nothing was submitted: still on the form, name is required.
  await expect(form(page)).toBeVisible();
  expect(await form(page).getByLabel('Name', { exact: true }).evaluate((el: HTMLInputElement) => el.validity.valueMissing)).toBe(true);

  await form(page).getByRole('radio', { name: 'App: image' }).click();
  expect(await form(page).getByLabel('Image').evaluate((el: HTMLInputElement) => el.validity.valueMissing)).toBe(true);
  await form(page).getByRole('radio', { name: 'Compose from Git' }).click();
  for (const label of ['Repository URL', 'Branch', 'Compose path']) {
    await expect(form(page).getByLabel(label)).toBeVisible();
  }
});

test('create form: a too-short slug is blocked by the browser', async ({ page }) => {
  await openList(page);
  await page.getByRole('button', { name: 'New project' }).click();
  await form(page).getByLabel('Name', { exact: true }).fill('ab');
  const requests: string[] = [];
  page.on('request', (r) => { if (r.method() === 'POST' && r.url().includes('/api/')) requests.push(r.url()); });
  expect(await form(page).getByLabel('Slug').evaluate((el: HTMLInputElement) => el.validity.patternMismatch)).toBe(true);
  await form(page).getByRole('radio', { name: 'Raw Compose' }).click();
  await submit(page);
  await expect(form(page)).toBeVisible();
  expect(requests).toEqual([]);
  await form(page).getByLabel('Slug').fill('Bad_Slug');
  expect(await form(page).getByLabel('Slug').evaluate((el: HTMLInputElement) => el.validity.patternMismatch)).toBe(true);
  await form(page).getByLabel('Slug').fill('good-slug');
  expect(await form(page).getByLabel('Slug').evaluate((el: HTMLInputElement) => el.validity.patternMismatch)).toBe(false);
});

test('create form: the server rejects a bad image', async ({ page }) => {
  await openList(page);
  await openForm(page, unique('cx-img'), 'App: image');
  await form(page).getByLabel('Image').fill('bad image!');
  await submit(page);
  await expect(form(page).getByRole('alert')).toHaveText('invalid image reference');
});

test('create form: application field errors are explained', async ({ page }) => {
  const slugName = unique('cx-val');
  await openList(page);
  await openForm(page, slugName, 'App: image');
  await form(page).getByLabel('Image').fill('traefik/whoami:v1.10');
  await form(page).getByLabel('deploy right away').uncheck();

  const expectError = async (field: string, value: string, message: string) => {
    await form(page).getByLabel(field).fill(value);
    await submit(page);
    await expect(form(page).getByRole('alert')).toHaveText(message);
    await form(page).getByLabel(field).fill('');
  };
  await expectError('Container port', '70000', 'Container port must be 1–65535.');
  await expectError('Memory limit (MiB)', '2', 'Memory limit is a whole number of MiB, at least 6.');
  await expectError('CPU limit (cores)', '0.001', 'CPU limit must be at least 0.01.');
  await expectError('Named volumes (name:/container/path per line)', 'nocolon', 'Volume "nocolon" must be name:/container/path.');
});

test('create form: static site build needs both image and commands', async ({ page }) => {
  await openList(page);
  await openForm(page, unique('cx-sv'), 'Static site');
  await form(page).getByLabel('Repository URL').fill('https://git.example.com/org/site.git');
  await form(page).getByLabel('Build commands (one per line, no shell)').fill('npm ci');
  await submit(page);
  await expect(form(page).getByRole('alert')).toHaveText('A build needs a toolchain image, e.g. node:22-alpine.');
  await form(page).getByLabel('Build commands (one per line, no shell)').fill('');
  await form(page).getByLabel('Build image (optional)').fill('node:22-alpine');
  await submit(page);
  await expect(form(page).getByRole('alert')).toHaveText('Add at least one build command, e.g. npm ci.');
});

test('create form: server rejects a duplicate slug', async ({ page }) => {
  const slug = unique('cx-dup');
  await openList(page);
  await openForm(page, slug, 'Raw Compose');
  await form(page).getByLabel('deploy right away').uncheck();
  await submit(page);
  await expect(page.getByRole('heading', { name: slug })).toBeVisible();
  await backToList(page);

  await page.getByRole('button', { name: 'New project' }).click();
  await form(page).getByLabel('Name', { exact: true }).fill(slug);
  await form(page).getByRole('radio', { name: 'Raw Compose' }).click();
  await form(page).getByLabel('deploy right away').uncheck();
  await submit(page);
  await expect(form(page).getByRole('alert')).toHaveText('a project with this slug already exists');
  await page.getByRole('button', { name: 'Close' }).click();
  await removeFromList(page, slug);
});

test('raw Compose: create, deploy, redeploy, roll back, stop and delete', async ({ page }) => {
  const failures = watchForFailures(page);
  const slug = unique('cx-raw');
  await openList(page);
  await openForm(page, slug, 'Raw Compose');
  await submit(page);
  await expectDeployed(page, slug);
  await expect(page.getByText('Raw Compose YAML')).toBeVisible();
  await expect(deploymentRows(page)).toHaveCount(1);
  await expect(deploymentRows(page).first()).toContainText('manual');
  await expect(page.locator('.log pre')).toContainText('Pulling images');
  await expect(page.locator('.log pre')).toContainText('Deployment finished');
  await expect(page.getByText('No output.')).toHaveCount(0);
  const services = page.locator('table').filter({ hasText: 'Image' }).filter({ hasText: 'Ports' });
  await expect(services.getByRole('cell', { name: 'web', exact: true })).toBeVisible();
  await expect(services).toContainText('mock-image:latest');
  await expect(services).toContainText('running');
  await expect(logServiceOptions(page)).toHaveText(['all services', 'web']);

  // The list shows it.
  await backToList(page);
  const row = page.getByRole('row').filter({ hasText: slug });
  await expect(row).toContainText('raw');
  await expect(row).toContainText('succeeded');
  await row.getByRole('button', { name: slug }).click();

  // Redeploy with "pull images".
  await page.getByLabel('pull images').check();
  await page.getByRole('button', { name: 'Deploy', exact: true }).click();
  await expect(deploymentRows(page)).toHaveCount(2);
  await expect(deploymentRows(page).first().getByText('succeeded')).toBeVisible();

  // Roll back to the older deployment.
  page.once('dialog', (dialog) => { expect(dialog.message()).toContain('Roll back to deployment'); dialog.accept(); });
  await deploymentRows(page).nth(1).getByRole('button', { name: 'Rollback to this version' }).click();
  await expect(deploymentRows(page)).toHaveCount(3);
  await expect(deploymentRows(page).first()).toContainText('rollback');
  await expect(deploymentRows(page).first().getByText('succeeded')).toBeVisible();

  // Cancelling the confirmation queues nothing.
  page.once('dialog', (dialog) => dialog.dismiss());
  await deploymentRows(page).nth(1).getByRole('button', { name: 'Rollback to this version' }).click();
  await expect(deploymentRows(page)).toHaveCount(3);

  // Stop, then a wrong confirmation keeps the project, the right one deletes it.
  await page.getByRole('button', { name: 'Stop', exact: true }).click();
  await expect(statusBadge(page)).toHaveText('stopped');
  await page.reload();
  await openTab(page, COMPOSE_TAB);
  await page.getByRole('button', { name: slug, exact: true }).click();
  await expect(services.getByRole('cell', { name: 'web', exact: true })).toHaveCount(0);
  await expect(services).toContainText('No containers.');
  page.once('dialog', (dialog) => dialog.accept('wrong'));
  await page.getByRole('button', { name: 'Delete', exact: true }).click();
  await expect(page.getByRole('heading', { name: slug })).toBeVisible();
  await deleteOpenProject(page, slug);
  await expect(page.getByRole('cell', { name: slug, exact: true })).toHaveCount(0);
  expect(failures).toEqual([]);
});

test('Compose from Git over HTTPS with a token', async ({ page }) => {
  const slug = unique('cx-git');
  await openList(page);
  await openForm(page, slug, 'Compose from Git');
  await form(page).getByLabel('Repository URL').fill('https://git.example.com/org/app.git');
  await expect(form(page).getByText('Private repository token (optional, write-only)')).toBeVisible();
  await form(page).getByLabel('Token').fill('s3cret-token');
  await submit(page);
  await expectDeployed(page, slug);
  await expect(page.getByText('https://git.example.com/org/app.git').first()).toBeVisible();
  await expect(page.getByText('docker-compose.yml').first()).toBeVisible();

  // Git access: the saved token is announced, never shown.
  await expect(page.getByText('A token is saved for')).toContainText('git.example.com');
  await expect(page.getByText('s3cret-token')).toHaveCount(0);
  await page.getByLabel('Token').fill('another-token');
  await page.getByRole('button', { name: 'Replace token' }).click();
  await expect(page.getByLabel('Token')).toHaveValue('');
  await page.getByRole('button', { name: 'Remove token' }).click();
  await expect(page.getByText('Public repository, or no token yet.')).toBeVisible();
  await page.getByLabel('Token').fill('third');
  await page.getByRole('button', { name: 'Save token' }).click();
  await expect(page.getByText('A token is saved for')).toBeVisible();

  await deleteOpenProject(page, slug);
});

test('Compose from Git over SSH: deploy key lifecycle', async ({ page }) => {
  const slug = unique('cx-ssh');
  await openList(page);
  await openForm(page, slug, 'Compose from Git');
  await form(page).getByLabel('Repository URL').fill('git@git.example.com:org/app.git');
  await expect(form(page).getByText('SSH remote: after creating')).toBeVisible();
  // No automatic deploy: the key does not exist yet.
  await expect(form(page).getByLabel('deploy right away')).not.toBeChecked();
  await submit(page);
  await expect(page.getByRole('heading', { name: slug })).toBeVisible();

  await expect(page.getByText('No deploy key yet')).toBeVisible();
  await page.getByRole('button', { name: 'Generate deploy key' }).click();
  await expect(page.locator('.secret.mono')).toContainText('ssh-ed25519');
  await page.getByRole('button', { name: 'Copy' }).click();
  page.once('dialog', (dialog) => dialog.accept());
  await page.getByRole('button', { name: 'Replace key' }).click();
  await expect(page.locator('.secret.mono')).toContainText('ssh-ed25519');
  await page.getByRole('button', { name: 'Remove', exact: true }).click();
  await expect(page.getByText('No deploy key yet')).toBeVisible();

  await deleteOpenProject(page, slug);
});

test('push webhook: enable, rotate, watch paths, disable', async ({ page }) => {
  const slug = unique('cx-hook');
  await openList(page);
  await openForm(page, slug, 'Compose from Git');
  await form(page).getByLabel('Repository URL').fill('https://git.example.com/org/hook.git');
  await form(page).getByLabel('deploy right away').uncheck();
  await submit(page);
  await expect(page.getByRole('heading', { name: slug })).toBeVisible();

  await expect(page.locator('span.state', { hasText: /^disabled$/ })).toBeVisible();
  await page.getByRole('button', { name: 'Enable webhook' }).click();
  await expect(page.getByText('Secret (shown once)')).toBeVisible();
  await expect(page.locator('span.state', { hasText: /^enabled$/ })).toBeVisible();
  const first = await page.locator('.secret .mono').innerText();
  await page.getByRole('button', { name: 'Rotate secret' }).click();
  await expect(page.locator('.secret .mono')).not.toHaveText(first);

  const save = page.getByRole('button', { name: 'Save watch paths' });
  await expect(save).toBeDisabled();
  await page.getByLabel('Watch paths').fill('src/**\ncompose.yml');
  // Wait for the save itself: the reload below must not abort it.
  await Promise.all([
    page.waitForResponse((r) => r.url().endsWith('/webhook') && r.request().method() === 'PUT' && r.ok()),
    save.click(),
  ]);
  await expect(save).toBeDisabled();
  await expect(page.getByLabel('Watch paths')).toHaveValue('src/**\ncompose.yml');
  await page.reload();
  await openTab(page, COMPOSE_TAB);
  await page.getByRole('button', { name: slug, exact: true }).click();
  await expect(page.getByLabel('Watch paths')).toHaveValue('src/**\ncompose.yml');

  await page.getByRole('button', { name: 'Disable', exact: true }).last().click();
  await expect(page.locator('span.state', { hasText: /^disabled$/ })).toBeVisible();
  await deleteOpenProject(page, slug);
});

test('application from an image with limits, volumes and variables', async ({ page }) => {
  const slug = unique('cx-img');
  await openList(page);
  await openForm(page, slug, 'App: image');
  await form(page).getByLabel('Image').fill('traefik/whoami:v1.10');
  await form(page).getByLabel('Container port').fill('80');
  await form(page).getByLabel('Memory limit (MiB)').fill('128');
  await form(page).getByLabel('CPU limit (cores)').fill('0.5');
  await form(page).getByLabel('Restart').selectOption('always');
  await form(page).getByLabel('Named volumes (name:/container/path per line)').fill('uploads:/app/uploads');
  await form(page).getByRole('button', { name: '+ add variable' }).click();
  await form(page).getByLabel('Variable name').fill('GREETING');
  await form(page).getByLabel('Variable value').fill('hello');
  await submit(page);
  await expectDeployed(page, slug);
  await expect(page.getByText('Application')).toBeVisible();
  await expect(page.getByText('port 80 · 128 MiB · 0.5 CPU · always')).toBeVisible();

  // Variable created with the service is listed by key, value is not shown.
  await expect(page.getByLabel('Variable name').first()).toHaveValue('GREETING');
  await expect(page.getByLabel('Variable name').first()).toHaveJSProperty('readOnly', true);
  await expect(page.getByLabel('Variable value').first()).toHaveAttribute('placeholder', /stored/);

  // Add a second variable and save it.
  await page.getByRole('button', { name: '+ add variable' }).click();
  await page.getByLabel('Variable name').last().fill('MODE');
  await page.getByLabel('Variable value').last().fill('prod');
  await page.getByRole('button', { name: 'Save variables' }).click();
  await expect(page.getByText('Saved. Deploy to apply.')).toBeVisible();
  await page.reload();
  await openTab(page, COMPOSE_TAB);
  await page.getByRole('button', { name: slug, exact: true }).click();
  await expect(page.getByLabel('Variable name')).toHaveCount(2);

  // Remove one and save.
  await page.locator('.env-row').first().getByRole('button', { name: 'remove' }).click();
  await page.getByRole('button', { name: 'Save variables' }).click();
  await expect(page.getByText('Saved. Deploy to apply.')).toBeVisible();
  await expect(page.getByLabel('Variable name')).toHaveCount(1);
  await expect(page.getByLabel('Variable name').first()).toHaveValue('MODE');

  await deleteOpenProject(page, slug);
});

test('application from a Dockerfile in Git', async ({ page }) => {
  const slug = unique('cx-dock');
  await openList(page);
  await openForm(page, slug, 'App: Dockerfile');
  await form(page).getByLabel('Repository URL').fill('https://git.example.com/org/app.git');
  await form(page).getByLabel('Build context').fill('services/api');
  await form(page).getByLabel('Dockerfile', { exact: true }).fill('Dockerfile.prod');
  await form(page).getByLabel('Health check command (one argument per line, no shell)').fill('wget\n-qO-\nhttp://127.0.0.1/health');
  await submit(page);
  await expectDeployed(page, slug);
  await expect(page.getByText('Dockerfile.prod')).toBeVisible();
  await expect(page.getByText('services/api')).toBeVisible();
  await deleteOpenProject(page, slug);
});

test('application built by a builder', async ({ page }) => {
  const slug = unique('cx-bld');
  await openList(page);
  await openForm(page, slug, 'App: Nixpacks / Railpack / Buildpacks');
  await form(page).getByLabel('Repository URL').fill('https://git.example.com/org/app.git');
  const options = await form(page).getByLabel('Builder').locator('option').allInnerTexts();
  expect(options.length).toBe(3);
  await form(page).getByLabel('Builder').selectOption({ index: 1 });
  await form(page).getByLabel('App directory').fill('web');
  await submit(page);
  await expectDeployed(page, slug);
  await expect(page.getByText(/build of/)).toBeVisible();
  await deleteOpenProject(page, slug);
});

test('static site: no stop, no container logs, spa and publish directory', async ({ page }) => {
  const slug = unique('cx-site');
  await openList(page);
  await openForm(page, slug, 'Static site');
  await form(page).getByLabel('Repository URL').fill('https://git.example.com/org/site.git');
  await form(page).getByLabel('Publish directory').fill('public');
  await form(page).getByLabel('single-page app').check();
  await form(page).getByLabel('Build image (optional)').fill('node:22-alpine');
  await form(page).getByLabel('Build commands (one per line, no shell)').fill('npm ci\nnpm run build');
  await submit(page);
  await expectDeployed(page, slug);
  await expect(page.getByText('Static site from')).toBeVisible();
  await expect(page.getByText('SPA')).toBeVisible();
  await expect(page.getByRole('button', { name: 'Stop', exact: true })).toHaveCount(0);
  await expect(page.getByLabel('pull images')).toHaveCount(0);
  await expect(page.getByRole('heading', { name: 'Container logs' })).toHaveCount(0);
  await deleteOpenProject(page, slug);
});

test('live container logs: follow, search, stop', async ({ page }) => {
  const slug = unique('cx-logs');
  await openList(page);
  await openForm(page, slug, 'Raw Compose');
  await submit(page);
  await expectDeployed(page, slug);

  await expect(page.getByText('Shows the last 200 lines')).toBeVisible();
  await page.getByRole('button', { name: 'Follow' }).click();
  const log = page.locator('.log').filter({ hasText: 'live ·' });
  await expect(log).toContainText('listening on :80');
  await expect(log).toContainText('GET / 200');
  await expect(log).toContainText('stream ended');

  await page.getByLabel('Search').fill('GET');
  await page.getByRole('button', { name: /Follow|Restart/ }).click();
  await expect(log).toContainText('GET / 200');
  await expect(log).not.toContainText('listening on :80');

  await page.getByLabel('Search').fill('no-such-line-anywhere');
  await page.getByRole('button', { name: /Follow|Restart/ }).click();
  await expect(log).toContainText('Waiting for output…');

  await deleteOpenProject(page, slug);
});

const SLOW_YAML = '# e2e: slow\nservices:\n  api:\n    image: traefik/whoami:v1.10\n  worker:\n    image: traefik/whoami:v1.10\n';

async function openSlow(page: Page, slug: string) {
  await openForm(page, slug, 'Raw Compose');
  await form(page).getByLabel('Compose YAML').fill(SLOW_YAML);
  await submit(page);
  await expect(page.getByRole('heading', { name: slug })).toBeVisible();
}

test('raw Compose: several services are listed; a slow deployment shows running and blocks Stop/Delete until cancelled', async ({ page }) => {
  const slug = unique('cx-slow');
  await openList(page);
  await openSlow(page, slug);
  await expect(statusBadge(page)).toHaveText('running');
  await expect(deploymentRows(page).first().getByText('running')).toBeVisible();
  await expect(page.getByRole('button', { name: 'Stop', exact: true })).toBeDisabled();
  await expect(page.getByRole('button', { name: 'Delete', exact: true })).toBeDisabled();
  await expect(deploymentRows(page).first().getByRole('button', { name: 'cancel' })).toBeVisible();
  await expect(deploymentRows(page).first().getByRole('button', { name: 'Rollback to this version' })).toHaveCount(0);

  await deploymentRows(page).first().getByRole('button', { name: 'cancel' }).click();
  await expect(statusBadge(page)).toHaveText('cancelled');
  await expect(deploymentRows(page).first().getByText('cancelled')).toBeVisible();
  await expect(deploymentRows(page).first().getByRole('button', { name: 'cancel' })).toHaveCount(0);
  await expect(page.getByRole('button', { name: 'Delete', exact: true })).toBeEnabled();
  await expect(page.getByRole('button', { name: 'Stop', exact: true })).toBeEnabled();
  await deleteOpenProject(page, slug);
});

test('raw Compose: the Services table and log dropdown follow the services: keys', async ({ page }) => {
  const slug = unique('cx-multi');
  await openList(page);
  await openForm(page, slug, 'Raw Compose');
  await form(page).getByLabel('Compose YAML').fill(SLOW_YAML.replace('# e2e: slow\n', ''));
  await submit(page);
  await expectDeployed(page, slug);
  const services = page.locator('table').filter({ hasText: 'Image' }).filter({ hasText: 'Ports' });
  await expect(services.getByRole('cell', { name: 'api', exact: true })).toBeVisible();
  await expect(services.getByRole('cell', { name: 'worker', exact: true })).toBeVisible();
  await expect(logServiceOptions(page)).toHaveText(['all services', 'api', 'worker']);
  await deleteOpenProject(page, slug);
});

test('Git and application projects list their service after deploying', async ({ page }) => {
  const git = unique('cx-sg');
  const app = unique('cx-sa');
  await openList(page);
  await openForm(page, git, 'Compose from Git');
  await form(page).getByLabel('Repository URL').fill('https://git.example.com/org/app.git');
  await submit(page);
  await expectDeployed(page, git);
  await expect(page.getByRole('cell', { name: 'web', exact: true })).toBeVisible();
  await deleteOpenProject(page, git);

  await openForm(page, app, 'App: image');
  await form(page).getByLabel('Image').fill('traefik/whoami:v1.10');
  await form(page).getByLabel('Service name').fill('frontdoor');
  await submit(page);
  await expectDeployed(page, app);
  await expect(page.getByRole('cell', { name: 'frontdoor', exact: true })).toBeVisible();
  await expect(logServiceOptions(page)).toHaveText(['all services', 'frontdoor']);
  await deleteOpenProject(page, app);
});

test('registry panel: choose and clear a private registry for a project', async ({ page }) => {
  const slug = unique('cx-reg');
  const regName = unique('reg');
  await openList(page);
  const created = await page.request.post('/api/registries', {
    data: { name: regName, server_url: 'registry.example.com:5000', username: 'u', password: 'p' },
  });
  expect(created.ok()).toBe(true);
  const registry = await created.json();
  try {
    await openForm(page, slug, 'Raw Compose');
    await form(page).getByLabel('deploy right away').uncheck();
    await submit(page);
    await expect(page.getByRole('heading', { name: slug })).toBeVisible();
    const select = page.getByLabel('Private registry');
    await expect(select).toHaveValue('');
    await expect(select.locator('option', { hasText: regName })).toHaveCount(1);
    await select.selectOption({ label: `${regName} · registry.example.com:5000` });
    await expect(select).toHaveValue(registry.id);
    await page.reload();
    await openTab(page, COMPOSE_TAB);
    await page.getByRole('button', { name: slug, exact: true }).click();
    await expect(page.getByLabel('Private registry')).toHaveValue(registry.id);
    await page.getByLabel('Private registry').selectOption('');
    await expect(page.getByLabel('Private registry')).toHaveValue('');
    await deleteOpenProject(page, slug);
  } finally {
    await page.request.delete(`/api/registries/${registry.id}`);
  }
});

test('create form: the server select appears only when several servers exist', async ({ page }) => {
  await openList(page);
  const servers: unknown[] = await (await page.request.get('/api/servers')).json();
  await page.getByRole('button', { name: 'New project' }).click();
  await expect(form(page).getByLabel('Private registry')).toBeVisible();
  await expect(form(page).getByLabel('Server', { exact: true })).toHaveCount(servers.length >= 2 ? 1 : 0);
  if (servers.length >= 2) {
    await expect(form(page).getByRole('option', { name: 'this host' })).toBeAttached();
    await expect(form(page).getByRole('option', { name: /automatic/ })).toBeAttached();
  }
});
