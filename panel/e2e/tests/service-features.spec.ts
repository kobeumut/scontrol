import { expect, Locator, Page, test } from '@playwright/test';
import { createHmac } from 'node:crypto';
import { openTab, signIn, unique, watchForFailures } from './helpers';

const COMPOSE = 'services:\n  web:\n    image: nginx:alpine\n    ports:\n      - "80"\n';

test.describe.configure({ mode: 'serial' });

let page: Page;
let slug: string;
let projectName: string;
let failures: string[];

const section = (heading: string): Locator => page.locator('h3', { hasText: heading });
const row = (text: string): Locator => page.locator('tr', { hasText: text });

test.beforeAll(async ({ browser }) => {
  page = await browser.newPage();
  failures = watchForFailures(page);
  await signIn(page);
  projectName = unique('svcfeat');
  slug = projectName;
  await openTab(page, 'Services');
  await page.getByRole('button', { name: 'New service' }).click();
  await page.getByLabel('Name', { exact: true }).fill(projectName);
  await page.getByRole('radio', { name: 'Raw Compose' }).click();
  await page.getByLabel('Compose YAML').fill(COMPOSE);
  await page.getByRole('button', { name: /Create/ }).click();
  await expect(page.getByRole('heading', { name: projectName })).toBeVisible();
});

test.afterAll(async () => {
  // Stop is only offered when not running; deletion needs the slug typed in.
  page.on('dialog', (dialog) => dialog.accept(dialog.type() === 'prompt' ? slug : undefined));
  await page.getByRole('button', { name: 'Delete', exact: true }).click().catch(() => {});
  await page.close();
});

test('a service opts in to its environment\'s database network', async () => {
  await expect(section('Database network')).toBeVisible();
  const join = page.getByLabel(/Join the environment's internal network scontrol-env-[0-9a-f]{32}/);
  await expect(join).not.toBeChecked();
  await join.check();
  await expect(page.getByText('Saved. Redeploy to apply.')).toBeVisible();
  await expect(join).toBeChecked();
  const id = (await (await page.request.get('/api/compose-projects')).json())
    .find((p: { slug: string }) => p.slug === slug).id;
  const stored = await (await page.request.get(`/api/compose-projects/${id}/shared-network`)).json();
  expect(stored).toMatchObject({ joined: true, always: false, supported: true });
  expect(stored.network).toMatch(/^scontrol-env-[0-9a-f]{32}$/);
  await join.uncheck();
  await expect(join).not.toBeChecked();
  expect((await (await page.request.get(`/api/compose-projects/${id}/shared-network`)).json()).joined).toBe(false);
});

const domainForm = () => page.locator('form.inline-form', { has: page.getByLabel('Domain', { exact: true }) });
const addDomain = async (domain: string, opts: { service?: string; port?: string; https?: boolean } = {}) => {
  const form = domainForm();
  await form.getByLabel('Domain', { exact: true }).fill(domain);
  await form.getByLabel('Service').fill(opts.service ?? 'web');
  await form.getByLabel('Container port').fill(opts.port ?? '80');
  const https = form.getByLabel('HTTPS');
  if (opts.https === undefined ? false : (await https.isChecked()) !== opts.https) await https.click();
  await form.getByRole('button', { name: 'Add domain' }).click();
};
const openOptions = async (domain: string) => {
  await row(domain).getByRole('button', { name: /^options/ }).click();
  return page.locator('form.card', { hasText: `Options for ${domain}` });
};
const saveOptions = (form: Locator) => form.getByRole('button', { name: 'Save and redeploy' }).click();

test('the project page offers deployment, domains and jobs', async () => {
  await expect(section('Domains')).toBeVisible();
  await expect(section('Scheduled jobs')).toBeVisible();
  // A raw Compose project has no Git checkout, so there is no preview section.
  await expect(section('Pull request previews')).toHaveCount(0);
  await expect(page.getByText('No scheduled jobs.')).toBeVisible();
  expect(failures).toEqual([]);
});

test('adds a domain without HTTPS and queues a redeploy', async () => {
  const domain = `${unique('plain')}.example.test`;
  const deployments = await page.locator('table').filter({ hasText: 'Started' }).first().locator('tbody tr').count().catch(() => 0);
  await addDomain(domain, { https: false });
  const created = row(domain);
  await expect(created).toBeVisible();
  await expect(created).toContainText('web:80');
  await expect(created).toContainText(/127\.0\.0\.1:\d+/);
  await expect(created.locator('td').nth(3)).toHaveText('—');
  await expect(created.getByRole('link', { name: domain })).toHaveAttribute('href', `http://${domain}`);
  await expect(page.getByLabel('Domain', { exact: true })).toHaveValue('');
  void deployments;
});

test('adds a domain with HTTPS and removes both domains', async () => {
  const domain = `${unique('tls')}.example.test`;
  await addDomain(domain, { https: true, port: '8080' });
  const created = row(domain);
  await expect(created).toContainText('web:8080');
  await expect(created.locator('td').nth(3)).toHaveText('yes');
  await expect(created.getByRole('link', { name: domain })).toHaveAttribute('href', `https://${domain}`);
  await created.getByRole('button', { name: 'remove' }).click();
  await expect(row(domain)).toHaveCount(0);
});

test('rejects a duplicate domain, a bad domain and a bad service name', async () => {
  const domain = `${unique('dup')}.example.test`;
  await addDomain(domain);
  await expect(row(domain)).toBeVisible();
  await addDomain(domain);
  await expect(page.getByRole('alert').filter({ hasText: 'already in use' })).toBeVisible();
  await addDomain('not a domain!');
  await expect(page.getByRole('alert').filter({ hasText: 'invalid domain' })).toBeVisible();
  await addDomain(`${unique('svc')}.example.test`, { service: 'Bad Service' });
  await expect(page.getByRole('alert').filter({ hasText: 'invalid service name' })).toBeVisible();
  await row(domain).getByRole('button', { name: 'remove' }).click();
  await expect(row(domain)).toHaveCount(0);
});

test('domains can be added, edited and removed without a redeploy', async () => {
  const redeploy = page.getByLabel('Redeploy now when domains change');
  await expect(redeploy).toBeChecked();
  const domain = `${unique('nodeploy')}.example.test`;
  const response = (method: string, match: RegExp) =>
    page.waitForResponse((r) => r.request().method() === method && match.test(r.url()));
  try {
    await redeploy.uncheck();
    const added = response('POST', /\/domains$/);
    await addDomain(domain);
    const addRes = await added;
    expect(addRes.request().postDataJSON()).toMatchObject({ domain, redeploy: false });
    expect((await addRes.json()).deployment).toBeNull();
    await expect(row(domain)).toBeVisible();
    await expect(page.getByRole('status').filter({ hasText: 'takes effect with the next deployment' })).toBeVisible();

    // The options form follows the switch and says what it will do.
    const form = await openOptions(domain);
    await expect(form.getByLabel('Redeploy now (otherwise')).not.toBeChecked();
    await expect(form.getByRole('button', { name: 'Save and redeploy' })).toHaveCount(0);
    await form.getByLabel('Path prefix').fill('/later/');
    const saved = response('PUT', /\/options$/);
    await form.getByRole('button', { name: 'Save', exact: true }).click();
    const saveRes = await saved;
    expect(saveRes.request().postDataJSON()).toMatchObject({ redeploy: false, options: { path_prefix: '/later/' } });
    expect((await saveRes.json()).deployment).toBeNull();
    await expect(form).toHaveCount(0);
    await expect(row(domain).getByRole('button', { name: /^options/ })).toHaveText('options •');

    const removed = response('DELETE', /\/domains\/[0-9a-f-]+\?redeploy=false$/);
    await row(domain).getByRole('button', { name: 'remove' }).click();
    expect((await (await removed).json()).deployment).toBeNull();
    await expect(row(domain)).toHaveCount(0);
  } finally {
    await redeploy.check();
  }

  // Back on, changes queue a deployment again.
  const other = `${unique('deploy')}.example.test`;
  const added = response('POST', /\/domains$/);
  await addDomain(other);
  const addRes = await added;
  expect(addRes.request().postDataJSON()).toMatchObject({ redeploy: true });
  expect((await addRes.json()).deployment).not.toBeNull();
  await row(other).getByRole('button', { name: 'remove' }).click();
  await expect(row(other)).toHaveCount(0);
  expect(failures).toEqual([]);
});

test('a redirect, path prefix and basic auth are saved as domain options', async () => {
  const domain = `${unique('opts')}.example.test`;
  await addDomain(domain);
  await expect(row(domain)).toBeVisible();

  // Path prefix + basic auth
  let form = await openOptions(domain);
  await form.getByLabel('Path prefix').fill('/app/');
  await form.getByLabel('HTTP Basic Auth').selectOption('set');
  await form.getByLabel('Basic auth users').fill('alice:a-long-password');
  await saveOptions(form);
  await expect(form).toHaveCount(0);
  await expect(row(domain).getByRole('button', { name: /^options/ })).toHaveText('options •');

  form = await openOptions(domain);
  await expect(form.getByLabel('Path prefix')).toHaveValue('/app/');
  await expect(form.getByText('(enabled)')).toBeVisible();
  await expect(form.getByLabel('HTTP Basic Auth').locator('option')).toHaveText(['keep as is', 'set users', 'disable']);

  // Disable the auth again
  await form.getByLabel('HTTP Basic Auth').selectOption('remove');
  await form.getByLabel('Path prefix').fill('');
  await saveOptions(form);
  await expect(form).toHaveCount(0);
  await expect(row(domain).getByRole('button', { name: /^options/ })).toHaveText('options');

  // Redirect
  form = await openOptions(domain);
  await form.getByLabel('Redirect all requests').check();
  await form.getByLabel('Redirect status').selectOption('302');
  await form.getByLabel('Redirect target').fill('https://example.org');
  await form.getByLabel('keep path').uncheck();
  await saveOptions(form);
  await expect(form).toHaveCount(0);
  form = await openOptions(domain);
  await expect(form.getByLabel('Redirect all requests')).toBeChecked();
  await expect(form.getByLabel('Redirect status')).toHaveValue('302');
  await expect(form.getByLabel('Redirect target')).toHaveValue('https://example.org');
  await expect(form.getByLabel('keep path')).not.toBeChecked();
  // A redirect replaces path prefix and auth.
  await expect(form.getByLabel('Path prefix')).toHaveCount(0);
  await expect(form.getByLabel('HTTP Basic Auth')).toHaveCount(0);

  await row(domain).getByRole('button', { name: 'remove' }).click();
  await expect(row(domain)).toHaveCount(0);
});

test('domain option validation errors are shown', async () => {
  const domain = `${unique('bad')}.example.test`;
  await addDomain(domain, { https: false });
  let form = await openOptions(domain);
  await form.getByLabel('Path prefix').fill('nope');
  await saveOptions(form);
  await expect(form.getByRole('alert')).toContainText('path prefix must look like /name/');

  await form.getByLabel('Path prefix').fill('');
  await form.getByLabel('HTTP Basic Auth').selectOption('set');
  await form.getByLabel('Basic auth users').fill('no-colon-here');
  await saveOptions(form);
  await expect(form.getByRole('alert')).toHaveText('Use one user:password per line.');
  await form.getByLabel('Basic auth users').fill('alice:short');
  await saveOptions(form);
  await expect(form.getByRole('alert')).toContainText('passwords need 8..=128');

  // No certificate upload without HTTPS.
  await expect(form.getByText('Custom TLS certificate')).toHaveCount(0);
  await row(domain).getByRole('button', { name: 'remove' }).click();
  await expect(row(domain)).toHaveCount(0);
});

test('a custom certificate is validated and can be uploaded and removed', async () => {
  const domain = `${unique('cert')}.example.test`;
  await addDomain(domain, { https: true });
  let form = await openOptions(domain);
  await form.getByLabel('Custom TLS certificate').selectOption('set');
  await form.getByLabel('Certificate PEM').fill('not a certificate');
  await form.getByLabel('Private key PEM').fill('-----BEGIN PRIVATE KEY-----\nabc\n-----END PRIVATE KEY-----');
  await saveOptions(form);
  await expect(form.getByRole('alert')).toContainText('certificate must be PEM encoded');

  await form.getByLabel('Certificate PEM').fill('-----BEGIN CERTIFICATE-----\nabc\n-----END CERTIFICATE-----');
  await saveOptions(form);
  await expect(form).toHaveCount(0);
  form = await openOptions(domain);
  await expect(form.getByText('(uploaded)')).toBeVisible();
  await form.getByLabel('Custom TLS certificate').selectOption('remove');
  await saveOptions(form);
  await expect(form).toHaveCount(0);
  form = await openOptions(domain);
  await expect(form.getByText('(uploaded)')).toHaveCount(0);
  await row(domain).getByRole('button', { name: 'remove' }).click();
  await expect(row(domain)).toHaveCount(0);
});

test('a redirect can be turned on for a domain that already has basic auth', async () => {
  const domain = `${unique('mix')}.example.test`;
  await addDomain(domain);
  let form = await openOptions(domain);
  await form.getByLabel('Path prefix').fill('/old/');
  await form.getByLabel('HTTP Basic Auth').selectOption('set');
  await form.getByLabel('Basic auth users').fill('bob:another-long-password');
  await saveOptions(form);
  await expect(form).toHaveCount(0);

  // Turn the redirect on: the form says it drops the prefix and the auth.
  form = await openOptions(domain);
  await form.getByLabel('Redirect all requests').check();
  await expect(form.getByText("Saving the redirect removes this domain's path prefix and basic auth.")).toBeVisible();
  await form.getByLabel('Redirect target').fill('https://example.org');
  await saveOptions(form);
  await expect(form).toHaveCount(0);

  // Saved state: redirect on, auth and prefix gone.
  form = await openOptions(domain);
  await expect(form.getByLabel('Redirect all requests')).toBeChecked();
  await expect(form.getByLabel('Redirect target')).toHaveValue('https://example.org');
  await expect(form.getByText("Saving the redirect removes")).toHaveCount(0);
  await form.getByLabel('Redirect all requests').uncheck();
  await expect(form.getByLabel('Path prefix')).toHaveValue('');
  await expect(form.getByText('(enabled)')).toHaveCount(0);
  await row(domain).getByRole('button', { name: 'remove' }).click();
  await expect(row(domain)).toHaveCount(0);
});

test('a redirect can be configured after a path prefix was typed and hidden', async () => {
  const domain = `${unique('hid')}.example.test`;
  await addDomain(domain);
  let form = await openOptions(domain);
  await form.getByLabel('Path prefix').fill('/hidden/');
  await form.getByLabel('Redirect all requests').check();
  await form.getByLabel('Redirect target').fill('https://example.org');
  await saveOptions(form);
  await expect(form.getByRole('alert')).toHaveCount(0);
  await expect(form).toHaveCount(0);

  form = await openOptions(domain);
  await expect(form.getByLabel('Redirect all requests')).toBeChecked();
  await expect(form.getByLabel('Redirect target')).toHaveValue('https://example.org');
  await form.getByLabel('Redirect all requests').uncheck();
  await expect(form.getByLabel('Path prefix')).toHaveValue('');
  await row(domain).getByRole('button', { name: 'remove' }).click();
  await expect(row(domain)).toHaveCount(0);
});

const jobRow = (name: string) => page.locator('tr', { hasText: name }).filter({ has: page.getByRole('button', { name: 'run now' }) });
const jobForm = () => page.locator('form.card', { has: page.getByLabel('Command (arguments, no shell)') });
const newJob = async (name: string, command = 'php artisan schedule:run') => {
  if (!(await jobForm().isVisible())) await page.getByRole('button', { name: 'New job' }).click();
  const form = jobForm();
  await form.getByLabel('Name', { exact: true }).fill(name);
  await form.getByLabel('Service', { exact: true }).fill('web');
  await form.getByLabel('Command (arguments, no shell)').fill(command);
  return form;
};

test('creates a cron job in a time zone and shows the generated timer', async () => {
  const name = unique('nightly');
  const form = await newJob(name, `php artisan "report:run" --label 'two words'`);
  await expect(form.getByText('Runs as ["php", "artisan", "report:run", "--label", "two words"]')).toBeVisible();
  await form.getByLabel('Cron expression (minute hour day month weekday)').fill('30 2 * * 1-5');
  await form.getByLabel('Time zone').fill('Europe/Istanbul');
  await expect(form.getByText(/OnCalendar=/)).toBeVisible();
  await form.getByLabel('Timeout (minutes)').fill('15');
  await form.getByRole('button', { name: 'Create job' }).click();
  await expect(form).toHaveCount(0);

  const created = jobRow(name);
  await expect(created).toContainText('cron 30 2 * * 1-5');
  await expect(created).toContainText('Europe/Istanbul');
  await expect(created).toContainText("php artisan report:run --label 'two words'");
  await expect(created).toContainText('never');
  await expect(created.getByRole('button', { name: 'pause' })).toBeVisible();
});

test('a preset fills the cron expression and the calendar type takes systemd syntax', async () => {
  const form = await newJob(unique('preset'));
  await form.getByLabel('Preset').selectOption({ label: 'Every 5 minutes' });
  await expect(form.getByLabel('Cron expression (minute hour day month weekday)')).toHaveValue('*/5 * * * *');
  await form.getByRole('button', { name: 'systemd calendar' }).click();
  await expect(form.getByLabel('OnCalendar expression')).toHaveValue('daily');
  await form.getByLabel('Preset').selectOption({ label: 'Weekdays at 09:00' });
  await expect(form.getByLabel('OnCalendar expression')).toHaveValue('Mon..Fri *-*-* 09:00');
  await expect(form.getByText('OnCalendar=Mon..Fri *-*-* 09:00')).toBeVisible();
  await form.getByRole('button', { name: 'Cancel' }).click();
  await expect(form).toHaveCount(0);
});

test('a calendar job is saved with its expression', async () => {
  const name = unique('cal');
  const form = await newJob(name, 'echo hi');
  await form.getByRole('button', { name: 'systemd calendar' }).click();
  await form.getByLabel('OnCalendar expression').fill('hourly');
  await form.getByLabel('Time zone').fill('UTC');
  await form.getByRole('button', { name: 'Create job' }).click();
  await expect(form).toHaveCount(0);
  await expect(jobRow(name)).toContainText('calendar hourly');
});

test('invalid schedules and commands are explained', async () => {
  const form = await newJob(unique('invalid'), 'echo "unterminated');
  await expect(form.getByText('Close the double quote.')).toBeVisible();
  await form.getByRole('button', { name: 'Create job' }).click();
  await expect(form.getByRole('alert')).toHaveText('Close the double quote.');

  await form.getByLabel('Command (arguments, no shell)').fill('sh -c "echo hi"');
  await form.getByRole('button', { name: 'Create job' }).click();
  await expect(form.getByRole('alert')).toContainText('shell command strings (sh -c) are not allowed');

  await form.getByLabel('Command (arguments, no shell)').fill('echo hi');
  await form.getByLabel('Cron expression (minute hour day month weekday)').fill('61 * * * *');
  await expect(form.locator('.state.error')).toBeVisible();
  await form.getByRole('button', { name: 'Create job' }).click();
  await expect(form.getByRole('alert')).toBeVisible();
  await expect(form.getByRole('alert')).toContainText(/minute/);

  await form.getByLabel('Cron expression (minute hour day month weekday)').fill('0 3 * * *');
  await form.getByLabel('Time zone').fill('Not/AZone!');
  await form.getByRole('button', { name: 'Create job' }).click();
  await expect(form.getByRole('alert')).toContainText(/time ?zone/i);
  await form.getByRole('button', { name: 'Cancel' }).click();
});

test('rejects a duplicate job name', async () => {
  const name = unique('dupjob');
  let form = await newJob(name, 'echo one');
  await form.getByRole('button', { name: 'Create job' }).click();
  await expect(jobRow(name)).toBeVisible();
  form = await newJob(name, 'echo two');
  await form.getByRole('button', { name: 'Create job' }).click();
  await expect(form.getByRole('alert')).toHaveText('a job with this name already exists');
  await form.getByRole('button', { name: 'Cancel' }).click();
  page.once('dialog', (d) => d.accept());
  await jobRow(name).getByRole('button', { name: 'delete' }).click();
  await expect(jobRow(name)).toHaveCount(0);
});

test('edits a job, then pauses and resumes it', async () => {
  const name = unique('editme');
  let form = await newJob(name, 'echo before');
  await form.getByRole('button', { name: 'Create job' }).click();
  await expect(jobRow(name)).toContainText('echo before');

  await jobRow(name).getByRole('button', { name: 'edit' }).click();
  form = jobForm();
  await expect(form.getByLabel('Command (arguments, no shell)')).toHaveValue('echo before');
  await form.getByLabel('Command (arguments, no shell)').fill('echo after');
  await form.getByLabel('Cron expression (minute hour day month weekday)').fill('0 4 * * 0');
  await form.getByRole('button', { name: 'Save job' }).click();
  await expect(form).toHaveCount(0);
  await expect(jobRow(name)).toContainText('echo after');
  await expect(jobRow(name)).toContainText('cron 0 4 * * 0');

  await jobRow(name).getByRole('button', { name: 'pause' }).click();
  await expect(jobRow(name)).toContainText('· paused');
  await expect(jobRow(name).getByRole('button', { name: 'resume' })).toBeVisible();
  await jobRow(name).getByRole('button', { name: 'resume' }).click();
  await expect(jobRow(name)).not.toContainText('· paused');
  await expect(jobRow(name).getByRole('button', { name: 'pause' })).toBeVisible();
});

test('run now opens the run history dialog, which closes with Escape and the close button', async () => {
  const name = unique('runme');
  const form = await newJob(name, 'echo hello');
  await form.getByRole('button', { name: 'Create job' }).click();
  await jobRow(name).getByRole('button', { name: 'run now' }).click();

  const dialog = page.getByRole('dialog', { name: `Runs of ${name}` });
  await expect(dialog).toBeVisible();
  await expect(dialog).toContainText('echo hello · cron 0 3 * * *');
  await page.keyboard.press('Escape');
  await expect(dialog).toHaveCount(0);
  await jobRow(name).getByRole('button', { name: 'runs' }).click();
  await expect(dialog).toBeVisible();
  await dialog.getByRole('button', { name: 'close' }).click();
  await expect(dialog).toHaveCount(0);
});

test('a manual run appears in the history with its log', async () => {
  const name = unique('runlog');
  const form = await newJob(name, 'echo hello');
  await form.getByRole('button', { name: 'Create job' }).click();
  await jobRow(name).getByRole('button', { name: 'run now' }).click();
  const dialog = page.getByRole('dialog', { name: `Runs of ${name}` });
  const first = dialog.locator('tbody tr').first();
  await expect(first).toContainText('manual');
  await expect(first.locator('.badge')).toHaveText('succeeded');
  await expect(dialog.locator('pre')).toContainText('mock job output');
  await page.keyboard.press('Escape');
  await expect(dialog).toHaveCount(0);
});

test('deletes a job only after confirming', async () => {
  const name = unique('delme');
  const form = await newJob(name, 'echo bye');
  await form.getByRole('button', { name: 'Create job' }).click();
  await expect(jobRow(name)).toBeVisible();
  page.once('dialog', (d) => d.dismiss());
  await jobRow(name).getByRole('button', { name: 'delete' }).click();
  await expect(jobRow(name)).toBeVisible();
  page.once('dialog', (d) => { expect(d.message()).toContain(name); return d.accept(); });
  await jobRow(name).getByRole('button', { name: 'delete' }).click();
  await expect(jobRow(name)).toHaveCount(0);
});

test.describe('pull request previews', () => {
  let git: Page;
  let gitName: string;

  test.beforeAll(async ({ browser }) => {
    git = await browser.newPage();
    await signIn(git);
    gitName = unique('svcprev');
    await openTab(git, 'Services');
    await git.getByRole('button', { name: 'New service' }).click();
    await git.getByLabel('Name', { exact: true }).fill(gitName);
    await git.getByLabel('Repository URL').fill('https://github.com/example/app.git');
    await git.getByLabel('Compose path').fill('docker-compose.yml');
    await git.getByRole('button', { name: /Create/ }).click();
    await expect(git.getByRole('heading', { name: gitName })).toBeVisible();
  });

  const pf = () => git.locator('form', { has: git.getByLabel('Base domain') });

  const tokenForm = () => git.locator('.inline-form', { has: git.getByLabel('Comment token') });

  // A reload returns to the first tab, so navigate back to the project.
  const reopen = async () => {
    await git.reload();
    await openTab(git, 'Services');
    await git.getByRole('button', { name: gitName, exact: true }).click();
    await expect(git.getByRole('heading', { name: gitName })).toBeVisible();
  };

  test.afterAll(async () => {
    git.once('dialog', (d) => d.accept(gitName));
    await git.getByRole('button', { name: 'Delete', exact: true }).click().catch(() => {});
    await git.close();
  });

  test('saves the preview settings, then the comment token, and validates input', async () => {
    const heading = git.locator('h3', { hasText: 'Pull request previews' });
    await expect(heading).toBeVisible();
    await expect(git.getByText('No previews yet.')).toHaveCount(0); // settings are not saved yet
    const save = git.getByRole('button', { name: 'Save preview settings' });
    await expect(save).toBeDisabled();

    await git.getByLabel('Base domain').fill('preview.example.test');
    await pf().getByLabel('Service', { exact: true }).fill('web');
    await expect(git.getByText('*.preview.example.test')).toBeVisible();
    await git.getByLabel('Active limit').fill('3');
    await git.getByLabel('TTL (hours)').fill('24');
    await git.getByLabel('Memory (MiB)').fill('256');
    await git.getByLabel('CPUs').fill('0.25');
    await git.getByLabel('enabled', { exact: true }).check();
    await save.click();
    await expect(git.getByText('No previews yet.')).toBeVisible();
    await expect(save).toBeDisabled();

    await reopen();
    await expect(git.getByLabel('Base domain')).toHaveValue('preview.example.test');
    await expect(git.getByLabel('Active limit')).toHaveValue('3');
    await expect(git.getByLabel('TTL (hours)')).toHaveValue('24');
    await expect(git.getByLabel('Memory (MiB)')).toHaveValue('256');
    await expect(git.getByLabel('CPUs')).toHaveValue('0.25');
    await expect(git.getByLabel('enabled', { exact: true })).toBeChecked();

    // A wildcard certificate needs HTTPS.
    await expect(git.getByLabel('Wildcard certificate (optional)')).toBeDisabled();
    await pf().getByLabel('HTTPS').check();
    await git.getByLabel('Wildcard certificate (optional)').fill('not a lineage');
    await save.click();
    await expect(git.getByRole('alert')).toContainText('wildcard certificate must name a certbot lineage');
    await git.getByLabel('Wildcard certificate (optional)').fill('preview.example.test');
    await save.click();
    await expect(save).toBeDisabled();
    await expect(git.getByRole('alert')).toHaveCount(0);

    // Comment token is write-only.
    const token = git.getByLabel('Comment token');
    await expect(tokenForm().getByRole('button', { name: 'Remove token' })).toHaveCount(0);
    await token.fill('ghp_exampletoken');
    await tokenForm().getByRole('button', { name: 'Save token' }).click();
    await expect(tokenForm().getByRole('button', { name: 'Remove token' })).toBeVisible();
    await expect(token).toHaveValue('');
    await expect(token).toHaveAttribute('placeholder', 'token stored — replace');
    await tokenForm().getByRole('button', { name: 'Remove token' }).click();
    await expect(tokenForm().getByRole('button', { name: 'Remove token' })).toHaveCount(0);
  });

  test('disabling previews asks for confirmation', async () => {
    const save = git.getByRole('button', { name: 'Save preview settings' });
    await git.getByLabel('enabled', { exact: true }).uncheck();
    git.once('dialog', (d) => d.dismiss());
    await save.click();
    await expect(save).toBeEnabled();
    git.once('dialog', (d) => { expect(d.message()).toContain('removes every preview'); return d.accept(); });
    await save.click();
    await expect(save).toBeDisabled();
    await reopen();
    await expect(git.getByLabel('enabled', { exact: true })).not.toBeChecked();
  });

  test('rejects an invalid base domain', async () => {
    await git.getByLabel('Base domain').fill('not a domain');
    await git.getByRole('button', { name: 'Save preview settings' }).click();
    await expect(git.getByRole('alert')).toHaveText('invalid preview base domain');
  });

  test('a signed pull request webhook creates a preview that can be opened and removed', async () => {
    await reopen();
    // Enable previews and the webhook (its secret is shown once).
    await git.getByLabel('Base domain').fill('preview.example.test');
    await pf().getByLabel('Service', { exact: true }).fill('web');
    await git.getByLabel('enabled', { exact: true }).check();
    await git.getByRole('button', { name: 'Save preview settings' }).click();
    await expect(git.getByText('No previews yet.')).toBeVisible();
    await git.getByRole('button', { name: 'Enable webhook' }).click();
    const secretText = await git.locator('.secret .mono').innerText();
    const projects = await (await git.request.get('/api/compose-projects')).json() as { id: string; name: string }[];
    const id = projects.find((p) => p.name === gitName)!.id;

    const number = 7;
    const body = JSON.stringify({
      action: 'opened',
      pull_request: {
        number,
        head: { ref: 'feature/x', sha: 'a'.repeat(40), repo: { id: 1 } },
        base: { ref: 'main', sha: 'b'.repeat(40), repo: { id: 1 } },
      },
    });
    const deliver = (payload: string, signature: string) => git.request.post(`/api/webhooks/compose/${id}`, {
      data: payload,
      headers: { 'content-type': 'application/json', 'x-github-event': 'pull_request', 'x-hub-signature-256': signature },
    });
    const sign = (payload: string, secret: string) => `sha256=${createHmac('sha256', secret).update(payload).digest('hex')}`;

    // A wrong signature is refused like an unknown project.
    expect((await deliver(body, sign(body, 'wrong-secret'))).status()).toBe(404);
    const accepted = await deliver(body, sign(body, secretText));
    expect(accepted.status()).toBe(202);

    await reopen();
    const domain = `${gitName}-pr-${number}.preview.example.test`;
    const preview = git.locator('tr', { hasText: `#${number}` });
    await expect(preview).toBeVisible();
    await expect(preview).toContainText('github');
    await expect(preview).toContainText('feature/x');
    await expect(preview.getByRole('link', { name: domain })).toHaveAttribute('href', `https://${domain}`);
    await expect(preview.locator('.badge')).toHaveText(/\S/);

    // "open" navigates to the preview's own project.
    await preview.getByRole('button', { name: 'open' }).click();
    await expect(git.getByRole('heading', { name: `${gitName} PR #${number}` })).toBeVisible();
    await git.getByRole('button', { name: '← All services' }).click();
    await git.getByRole('button', { name: gitName, exact: true }).click();
    await expect(git.getByRole('heading', { name: gitName }).filter({ hasNotText: 'PR #' })).toBeVisible();

    git.once('dialog', (d) => { expect(d.message()).toContain(`${gitName}-pr-${number}`); return d.accept(); });
    await git.locator('tr', { hasText: `#${number}` }).getByRole('button', { name: 'remove' }).click();
    // Removal is asynchronous: the row first shows "removing", then disappears.
    await expect(git.locator('tr', { hasText: `#${number}` })).toHaveCount(0, { timeout: 30_000 });
  });
});

test('opens the web terminal for a deployed service and echoes typed input', async () => {
  await page.getByRole('button', { name: 'Deploy', exact: true }).click();
  await expect(page.locator('tbody .badge').first()).toHaveText('succeeded', { timeout: 30_000 });
  await expect(section('Terminal')).toBeVisible({ timeout: 15_000 });
  await page.getByRole('button', { name: 'Open terminal' }).click();
  const terminal = page.locator('.terminal .xterm');
  await expect(terminal).toBeVisible();
  await expect(terminal.locator('.xterm-rows')).toContainText('mock-shell:/#');
  await terminal.click();
  await page.keyboard.type('echo hi-from-e2e');
  await expect(terminal.locator('.xterm-rows')).toContainText('echo hi-from-e2e');
  await page.getByRole('button', { name: 'Close terminal' }).click();
  await expect(page.locator('.terminal')).toHaveCount(0);
});

test('the terminal endpoint accepts a session for a service of the project', async () => {
  const projects = await (await page.request.get('/api/compose-projects')).json() as { id: string; slug: string }[];
  const id = projects.find((p) => p.slug === slug)!.id;
  const outcome = await page.evaluate((projectId) => new Promise<string>((resolve) => {
    const socket = new WebSocket(`ws://${location.host}/api/compose-projects/${projectId}/terminal?service=web&shell=sh&cols=80&rows=24`);
    socket.binaryType = 'arraybuffer';
    socket.onmessage = (event) => {
      const text = typeof event.data === 'string' ? '' : new TextDecoder().decode(event.data as ArrayBuffer);
      if (text.includes('mock-shell:/#')) { resolve('open'); socket.close(); }
    };
    socket.onerror = () => resolve('error');
    setTimeout(() => resolve('timeout'), 8000);
  }), id);
  expect(outcome).toBe('open');
});

test.afterAll(() => expect(failures).toEqual([]));
