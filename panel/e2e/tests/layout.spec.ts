import { expect, Page, test } from '@playwright/test';
import { NAV_ENTRIES, openTab, signIn, unique, watchForFailures } from './helpers';

// The app frame (sidebar, narrow-screen menu), page width at every screen,
// and the project page that shows what runs in each environment.

const nav = (page: Page) => page.getByRole('navigation', { name: 'Primary content' });

/** How far the page is wider than the viewport; 0 means no sideways scrolling. */
const overflow = (page: Page) => page.evaluate(() => document.documentElement.scrollWidth - document.documentElement.clientWidth);

test('the sidebar groups every screen and marks the open one', async ({ page }) => {
  await signIn(page);
  await expect(nav(page).getByRole('button')).toHaveText(NAV_ENTRIES);
  await expect(nav(page).getByRole('group', { name: 'Deploy' }).getByRole('button')).toHaveText(['Projects', 'Services', 'Templates', 'Databases']);
  // Projects is where a session starts.
  await expect(nav(page).getByRole('button', { name: 'Projects', exact: true })).toHaveAttribute('aria-current', 'page');
  await expect(page.getByRole('heading', { name: 'Projects', exact: true })).toBeVisible();

  await openTab(page, 'Databases');
  await expect(nav(page).getByRole('button', { name: 'Databases', exact: true })).toHaveAttribute('aria-current', 'page');
  await expect(nav(page).getByRole('button', { name: 'Projects', exact: true })).not.toHaveAttribute('aria-current', 'page');

  // The address names the screen, so a reload stays on it.
  await expect(page).toHaveURL(/#\/databases$/);
  await page.reload();
  await expect(page.getByRole('heading', { name: 'Databases', exact: true })).toBeVisible();
  await expect(page.getByRole('region', { name: 'Installed capabilities' })).toContainText('docker');
});

for (const width of [1440, 1024, 768, 390]) {
  test(`no screen scrolls sideways at ${width}px`, async ({ page }) => {
    const failures = watchForFailures(page);
    await signIn(page);
    await page.setViewportSize({ width, height: 900 });
    const wide: string[] = [];
    for (const entry of NAV_ENTRIES) {
      await openTab(page, entry);
      await expect(page.getByText('Loading…')).toHaveCount(0);
      const extra = await overflow(page);
      if (extra > 0) wide.push(`${entry}: ${extra}px too wide`);
    }
    expect(wide).toEqual([]);
    expect(failures).toEqual([]);
  });
}

test('on a narrow screen the sidebar is behind a menu button', async ({ page }) => {
  await signIn(page);
  await page.setViewportSize({ width: 390, height: 800 });
  const menu = page.getByRole('button', { name: 'Menu', exact: true });
  await expect(menu).toBeVisible();
  await expect(menu).toHaveAttribute('aria-expanded', 'false');
  await expect(menu).toContainText('Projects');
  await expect(nav(page)).toBeHidden();

  await menu.click();
  await expect(menu).toHaveAttribute('aria-expanded', 'true');
  await expect(nav(page).getByRole('button')).toHaveText(NAV_ENTRIES);
  await nav(page).getByRole('button', { name: 'Templates', exact: true }).click();

  // Choosing a screen closes the menu and the button names where you are.
  await expect(nav(page)).toBeHidden();
  await expect(menu).toContainText('Templates');
  await expect(page.getByRole('heading', { name: 'Template catalogue' })).toBeVisible();
  expect(await overflow(page)).toBe(0);
});

test('a project shows what runs in each environment and leads to it', async ({ page }) => {
  const failures = watchForFailures(page);
  page.on('dialog', (dialog) => void dialog.accept(dialog.type() === 'prompt' ? dialog.message().match(/^Type (\S+) /)?.[1] : undefined));
  await signIn(page);
  const slug = unique('shop');
  const projectName = `Shop ${slug}`;
  const serviceSlug = `${slug}-web`;
  const dbName = `${slug}-db`;

  await openTab(page, 'Projects');
  await page.getByRole('button', { name: 'New project' }).click();
  await page.getByLabel('Name', { exact: true }).fill(projectName);
  await page.getByLabel('Slug').fill(slug);
  await page.getByRole('button', { name: 'Create project' }).click();
  await expect(page.getByRole('heading', { name: new RegExp(projectName) })).toBeVisible();

  // An empty environment says what to do next.
  const production = page.getByRole('tabpanel', { name: 'production environment' });
  await expect(production.getByText('Nothing runs in production yet.')).toBeVisible();
  await page.getByLabel('New environment name').fill('staging');
  await page.getByRole('button', { name: 'Add environment' }).click();
  await expect(page.getByRole('tabpanel', { name: 'staging environment' })).toContainText('Nothing runs in staging yet.');
  await page.getByRole('tab', { name: /production/ }).click();

  // "Add service" opens the new-service form aimed at this environment.
  await production.getByRole('button', { name: 'Add service' }).click();
  await expect(page.getByRole('heading', { name: 'Services', exact: true })).toBeVisible();
  const form = page.locator('form.card').filter({ has: page.getByRole('radiogroup', { name: 'Source' }) });
  await expect(form.getByLabel('Project / environment').locator('option:checked')).toHaveText(`${projectName} / production`);
  await form.getByLabel('Name', { exact: true }).fill(`Web ${slug}`);
  await form.getByLabel('Slug').fill(serviceSlug);
  await form.getByRole('radio', { name: 'Raw Compose', exact: true }).click();
  await form.getByRole('button', { name: /^Create/ }).click();
  await expect(page.getByRole('heading', { name: new RegExp(`Web ${slug}`) })).toBeVisible();
  await expect(page.locator('.panel-title .badge')).toHaveText('succeeded', { timeout: 30_000 });

  // The service page says which project and environment it belongs to, and links back.
  const facts = page.locator('dl.facts');
  await expect(facts).toContainText(`${projectName} / production`);
  await expect(facts).toContainText('Compose · raw');
  await expect(page.getByRole('navigation', { name: 'Sections of this service' }).getByRole('link', { name: 'Domains' })).toBeVisible();
  const domain = `${slug}.example.test`;
  await page.getByLabel('Redeploy now when domains change').uncheck();
  await page.getByLabel('Domain', { exact: true }).fill(domain);
  await page.getByPlaceholder('service', { exact: true }).fill('web');
  await page.getByLabel('HTTPS').uncheck();
  await page.getByRole('button', { name: 'Add domain' }).click();
  await expect(page.getByRole('link', { name: domain })).toBeVisible();
  await facts.getByRole('button', { name: projectName }).click();
  await expect(page.getByRole('heading', { name: new RegExp(projectName) })).toBeVisible();

  // Back on the project: the service with its type, state, containers, domain and server.
  const services = production.getByRole('table', { name: 'Services in production' });
  const serviceRow = services.getByRole('row', { name: new RegExp(serviceSlug) });
  await expect(serviceRow).toContainText(`Web ${slug}`);
  await expect(serviceRow).toContainText('Compose · raw');
  await expect(serviceRow).toContainText('succeeded');
  await expect(serviceRow).toContainText('1/1 running');
  await expect(serviceRow.getByRole('link', { name: domain })).toHaveAttribute('href', `http://${domain}`);
  await expect(serviceRow).toContainText('this host');
  await expect(page.getByRole('tab', { name: /production/ })).toContainText('1 service');

  // "Add database" opens the new-database form aimed at this environment.
  await production.getByRole('button', { name: 'Add database' }).click();
  await expect(page.getByLabel('Project / environment').locator('option:checked')).toHaveText(`${projectName} / production`);
  await page.getByLabel('Name', { exact: true }).fill(dbName);
  await page.getByRole('button', { name: 'Create and start' }).click();
  await expect(page.getByRole('heading', { name: new RegExp(`^${dbName}`) })).toBeVisible();
  await expect(page.locator('tr', { hasText: dbName }).first()).toContainText(`${projectName} / production`);

  // The project lists it under Databases, with the name services use to reach it.
  await openTab(page, 'Projects');
  await page.getByRole('row', { name: new RegExp(slug) }).getByRole('button').click();
  const databases = production.getByRole('table', { name: 'Databases in production' });
  const dbRow = databases.getByRole('row', { name: new RegExp(dbName) });
  await expect(dbRow).toContainText('PostgreSQL');
  await expect(dbRow).toContainText(/127\.0\.0\.1:\d+/);
  await expect(dbRow).toContainText('closed');
  await expect(services.getByRole('row', { name: new RegExp(dbName) })).toHaveCount(0);
  await expect(page.getByRole('tab', { name: /production/ })).toContainText('2 services');
  // The other environment stays empty, and an environment with services cannot be deleted.
  await expect(page.getByRole('button', { name: 'Delete production' })).toBeDisabled();
  await page.getByRole('tab', { name: /staging/ }).click();
  await expect(page.getByRole('tabpanel', { name: 'staging environment' })).toContainText('Nothing runs in staging yet.');
  await page.getByRole('tab', { name: /production/ }).click();

  // "Install template" opens the catalogue aimed at this environment.
  await production.getByRole('button', { name: 'Install template' }).click();
  await expect(page.getByRole('status')).toContainText(`${projectName} / production`);
  const card = page.locator('article.card', { has: page.getByRole('heading', { name: 'Uptime Kuma', exact: true }) });
  await card.getByRole('button', { name: 'Install…' }).click();
  await expect(page.locator('form.card').getByLabel('Project / environment').locator('option:checked')).toHaveText(`${projectName} / production`);

  // A database row opens the database; a service row opens the service.
  await openTab(page, 'Projects');
  await page.getByRole('row', { name: new RegExp(slug) }).getByRole('button').click();
  await dbRow.getByRole('button', { name: dbName }).click();
  await expect(page.getByRole('heading', { name: new RegExp(`^${dbName}`) })).toBeVisible();
  await page.getByRole('button', { name: 'Delete', exact: true }).click();
  await expect(page.getByRole('heading', { name: new RegExp(`^${dbName}`) })).toHaveCount(0);

  await openTab(page, 'Projects');
  await page.getByRole('row', { name: new RegExp(slug) }).getByRole('button').click();
  await serviceRow.getByRole('button', { name: `Web ${slug}` }).click();
  await expect(page.getByRole('heading', { name: new RegExp(`Web ${slug}`) })).toBeVisible();
  await page.getByRole('button', { name: 'Delete', exact: true }).click();
  await expect(page.getByRole('button', { name: 'New service' })).toBeVisible();

  await openTab(page, 'Projects');
  await page.getByRole('row', { name: new RegExp(slug) }).getByRole('button').click();
  await expect(production.getByText('Nothing runs in production yet.')).toBeVisible();
  await page.getByRole('button', { name: 'Delete project' }).click();
  await expect(page.getByRole('heading', { name: 'Projects', exact: true })).toBeVisible();
  await expect(page.getByRole('row', { name: new RegExp(slug) })).toHaveCount(0);
  expect(failures).toEqual([]);
});
