import { expect, Locator, Page, test } from '@playwright/test';
import { openTab, signIn, unique, watchForFailures } from './helpers';

const TEMPLATES = [
  { id: 'wordpress', name: 'WordPress' },
  { id: 'ghost', name: 'Ghost' },
  { id: 'directus', name: 'Directus' },
  { id: 'plausible', name: 'Plausible Analytics' },
  { id: 'uptime-kuma', name: 'Uptime Kuma' },
];

const PROJECTS = 'Projects';

async function createProject(page: Page, name: string, slug?: string) {
  await page.getByRole('button', { name: 'New project' }).click();
  await page.getByLabel('Name', { exact: true }).fill(name);
  if (slug) await page.getByLabel('Slug').fill(slug);
  await page.getByRole('button', { name: 'Create project' }).click();
}

/** Create a project through the UI and land on its detail page. */
async function newProject(page: Page, prefix: string) {
  const slug = unique(prefix);
  await openTab(page, PROJECTS);
  await createProject(page, `Name ${slug}`, slug);
  await expect(page.getByRole('heading', { name: new RegExp(`Name ${slug}`) })).toBeVisible();
  return slug;
}

async function deleteProject(page: Page, slug: string) {
  page.once('dialog', (d) => d.accept(slug));
  await page.getByRole('button', { name: 'Delete project' }).click();
  await expect(page.getByRole('heading', { name: 'Projects' })).toBeVisible();
}

/**
 * One layer's variables editor. The editors render as flat siblings (heading,
 * table, rows, toolbar), so scope by order: project layer first, environment second.
 */
class Editor {
  readonly rows: Locator; readonly names: Locator; readonly values: Locator;
  readonly layers: Locator; readonly saveButton: Locator; readonly saved: Locator; readonly alert: Locator;
  constructor(private page: Page, index: number) {
    this.rows = page.locator('.env-editor').nth(index);
    this.names = this.rows.getByLabel('Variable name');
    this.values = this.rows.getByLabel('Variable value');
    this.layers = this.rows.locator('xpath=preceding-sibling::table[1]');
    const toolbar = this.rows.locator('xpath=following-sibling::div[contains(@class,"toolbar")][1]');
    this.saveButton = toolbar.getByRole('button', { name: 'Save variables' });
    this.saved = toolbar.getByText('Saved. Deploy to apply.');
    this.alert = this.rows.locator('xpath=following-sibling::p[@role="alert"][1]');
  }
  async add(key: string, value: string) {
    await this.rows.getByRole('button', { name: '+ add variable' }).click();
    await this.names.last().fill(key);
    await this.values.last().fill(value);
  }
  remove(position: 'first' | 'last') { return this.rows.getByRole('button', { name: 'remove' })[position](); }
  async save() { await this.saveButton.click(); await expect(this.saved).toBeVisible(); }
  layerRow(key: string) { return this.layers.getByRole('row', { name: new RegExp(key) }); }
}
const projectEditor = (page: Page) => new Editor(page, 0);
const envEditor = (page: Page) => new Editor(page, 1);

test('creates a project, derives the slug from the name, and lists it', async ({ page }) => {
  const failures = watchForFailures(page);
  await signIn(page);
  await openTab(page, PROJECTS);
  const slug = unique('proj');
  await page.getByRole('button', { name: 'New project' }).click();
  await page.getByLabel('Name', { exact: true }).fill(`Hello World ${slug}`);
  await expect(page.getByLabel('Slug')).toHaveValue(`hello-world-${slug}`);
  await page.getByLabel('Slug').fill(slug);
  await page.getByRole('button', { name: 'Create project' }).click();
  await expect(page.getByRole('heading', { name: new RegExp(`Hello World ${slug}`) })).toBeVisible();
  // a new project starts with a production environment
  await expect(page.getByRole('tab', { name: /production/ })).toBeVisible();
  await page.getByRole('button', { name: '← All projects' }).click();
  const row = page.getByRole('row', { name: new RegExp(slug) });
  await expect(row).toContainText('production');
  await row.getByRole('button', { name: `Hello World ${slug}` }).click();
  await deleteProject(page, slug);
  await expect(page.getByRole('row', { name: new RegExp(slug) })).toHaveCount(0);
  expect(failures).toEqual([]);
});

test('rejects invalid and duplicate project slugs', async ({ page }) => {
  await signIn(page);
  const slug = await newProject(page, 'dup');
  await page.getByRole('button', { name: '← All projects' }).click();

  // a bad slug is refused with a message and nothing is created
  await page.getByRole('button', { name: 'New project' }).click();
  await page.getByLabel('Name', { exact: true }).fill('Bad slug');
  await page.getByLabel('Slug').fill('Bad Slug!');
  expect(await page.getByLabel('Slug').evaluate((el: HTMLInputElement) => el.validity.patternMismatch)).toBe(true);
  let posted = false;
  page.on('request', (r) => { if (r.method() === 'POST' && r.url().endsWith('/api/projects')) posted = true; });
  await page.getByRole('button', { name: 'Create project' }).click();
  await expect(page.getByRole('heading', { name: 'Projects' })).toBeVisible();
  expect(posted).toBe(false);

  // server-side duplicate
  await page.getByLabel('Slug').fill(slug);
  await page.getByRole('button', { name: 'Create project' }).click();
  await expect(page.getByRole('alert')).toHaveText('a project with this slug already exists');

  await page.getByRole('button', { name: 'Close' }).click();
  await page.getByRole('row', { name: new RegExp(slug) }).getByRole('button').click();
  await deleteProject(page, slug);
});

test('adds, switches and deletes environments', async ({ page }) => {
  await signIn(page);
  const slug = await newProject(page, 'envs');
  const envName = `Staging ${slug}`;
  const addInput = page.getByLabel('New environment name');
  const add = page.getByRole('button', { name: 'Add environment' });

  await expect(add).toBeDisabled();
  await addInput.fill(envName);
  await add.click();
  const tab = page.getByRole('tab', { name: new RegExp(envName) });
  await expect(tab).toHaveAttribute('aria-selected', 'true');
  await expect(page.getByRole('heading', { name: `${envName} variables` })).toBeVisible();

  // same name -> same slug -> conflict
  await addInput.fill(envName);
  await add.click();
  await expect(page.getByRole('alert')).toHaveText('an environment with this slug already exists');

  // a name with no letters or digits
  await addInput.fill('!!!');
  await add.click();
  await expect(page.getByRole('alert')).toHaveText('environment name must contain letters or digits');

  await page.getByRole('tab', { name: /production/ }).click();
  await expect(page.getByRole('heading', { name: 'production variables' })).toBeVisible();

  await tab.click();
  page.once('dialog', (d) => d.dismiss());
  await page.getByRole('button', { name: `Delete ${envName}` }).click();
  await expect(tab).toBeVisible();

  page.once('dialog', (d) => d.accept());
  await page.getByRole('button', { name: `Delete ${envName}` }).click();
  await expect(tab).toHaveCount(0);

  await deleteProject(page, slug);
});

test('deleting a project needs its slug typed', async ({ page }) => {
  await signIn(page);
  const slug = await newProject(page, 'del');
  page.once('dialog', (d) => d.accept('wrong'));
  await page.getByRole('button', { name: 'Delete project' }).click();
  await expect(page.getByRole('button', { name: 'Delete project' })).toBeVisible();
  await deleteProject(page, slug);
  await expect(page.getByRole('row', { name: new RegExp(slug) })).toHaveCount(0);
});

test('renames a project and keeps its slug', async ({ page }) => {
  await signIn(page);
  const slug = await newProject(page, 'ren');
  const newName = `Renamed ${slug}`;
  await page.getByRole('button', { name: 'Rename' }).click();
  const input = page.getByLabel('Project name');
  const save = page.getByRole('button', { name: 'Save name' });
  await expect(input).toHaveValue(`Name ${slug}`);
  await expect(save).toBeDisabled();
  await input.fill(newName);
  await save.click();
  await expect(page.getByRole('heading', { name: new RegExp(newName) })).toBeVisible();
  await expect(page.getByRole('heading', { name: new RegExp(newName) })).toContainText(slug);
  await page.getByRole('button', { name: '← All projects' }).click();
  const row = page.getByRole('row', { name: new RegExp(slug) });
  await expect(row).toContainText(newName);
  await expect(row.locator('td.mono')).toHaveText(slug);
  await row.getByRole('button', { name: newName }).click();
  await deleteProject(page, slug);
});

const openProject = async (page: Page, slug: string) => {
  await openTab(page, PROJECTS);
  await page.getByRole('row', { name: new RegExp(slug) }).getByRole('button').click();
};

test('project variables are write-only, kept on save, and removable', async ({ page }) => {
  const failures = watchForFailures(page);
  await signIn(page);
  const slug = await newProject(page, 'pvar');
  const ed = projectEditor(page);

  await expect(ed.saveButton).toBeDisabled();
  await ed.add('API_TOKEN', 'secret-one');
  await ed.add('REGION', 'eu');
  await ed.save();

  // the value is never loaded back; the key is stored and read-only
  await page.reload();
  await openProject(page, slug);
  await expect(ed.names).toHaveCount(2);
  await expect(ed.names.first()).toHaveValue('API_TOKEN');
  await expect(ed.names.first()).toHaveJSProperty('readOnly', true);
  await expect(ed.values.first()).toHaveValue('');
  await expect(ed.values.first()).toHaveAttribute('placeholder', /stored/);
  await expect(page.getByText('secret-one')).toHaveCount(0);

  // removing a row deletes that key and keeps the other (empty value = keep stored)
  await ed.remove('last').click();
  await ed.save();
  await page.reload();
  await openProject(page, slug);
  await expect(ed.names).toHaveCount(1);
  await expect(ed.names.first()).toHaveValue('API_TOKEN');

  // an invalid name is rejected with a message
  await ed.add('bad key!', 'x');
  await ed.saveButton.click();
  await expect(ed.alert).toBeVisible();

  await deleteProject(page, slug);
  expect(failures).toEqual([]);
});

test('the environment editor shows project variables saved a moment ago', async ({ page }) => {
  await signIn(page);
  const slug = await newProject(page, 'stale');
  await projectEditor(page).add('FRESH', 'p');
  await projectEditor(page).save();
  await expect(envEditor(page).layerRow('FRESH')).toContainText('project (inherited)');
  await deleteProject(page, slug);
});

test('environment variables override project variables and fall back when removed', async ({ page }) => {
  await signIn(page);
  const slug = await newProject(page, 'layer');
  const project = projectEditor(page);
  const env = envEditor(page);

  await project.add('SHARED', 'from-project');
  await project.add('ONLY_PROJECT', 'p');
  await project.save();
  await page.reload();
  await openProject(page, slug);

  // the environment sees both as inherited from the project
  await expect(env.layerRow('SHARED')).toContainText('project (inherited)');
  await expect(env.layerRow('ONLY_PROJECT')).toContainText('project (inherited)');

  // override one key here
  await env.layerRow('SHARED').getByRole('button', { name: 'override here' }).click();
  await expect(env.names.first()).toHaveValue('SHARED');
  await env.values.first().fill('from-env');
  await env.save();
  await expect(env.layerRow('SHARED')).toContainText('environment');
  await expect(env.layerRow('SHARED')).not.toContainText('inherited');
  await expect(env.layerRow('SHARED').locator('.badge.overridden')).toHaveText('project');
  await expect(env.layerRow('ONLY_PROJECT')).toContainText('project (inherited)');
  // the project layer keeps its own key
  await expect(project.names).toHaveCount(2);

  // removing the override gives the project value back
  await env.remove('first').click();
  await env.save();
  await expect(env.layerRow('SHARED')).toContainText('project (inherited)');
  await expect(env.layerRow('SHARED').locator('.badge.overridden')).toHaveCount(0);

  await deleteProject(page, slug);
});

test('variables of an environment are separate from other environments', async ({ page }) => {
  await signIn(page);
  const slug = await newProject(page, 'sep');
  await page.getByLabel('New environment name').fill('Beta');
  await page.getByRole('button', { name: 'Add environment' }).click();
  await expect(page.getByRole('heading', { name: 'Beta variables' })).toBeVisible();
  const env = envEditor(page);
  await env.add('BETA_ONLY', 'b');
  await env.save();

  await page.getByRole('tab', { name: /production/ }).click();
  await expect(page.getByRole('heading', { name: 'production variables' })).toBeVisible();
  await expect(env.names).toHaveCount(0);
  await page.getByRole('tab', { name: /Beta/ }).click();
  await expect(env.names.first()).toHaveValue('BETA_ONLY');

  await deleteProject(page, slug);
});

test('the catalogue lists every template', async ({ page }) => {
  await signIn(page);
  await openTab(page, 'Templates');
  for (const t of TEMPLATES) {
    await expect(page.getByRole('heading', { name: t.name, exact: true })).toBeVisible();
  }
});

for (const template of TEMPLATES) {
  test(`installs the ${template.name} template and the service appears`, async ({ page }) => {
    const failures = watchForFailures(page);
    await signIn(page);
    await openTab(page, 'Templates');
    const slug = unique(template.id.slice(0, 6));
    const name = `${template.name} ${slug}`;
    const card = page.locator('article.card', { has: page.getByRole('heading', { name: template.name, exact: true }) });
    await card.getByRole('button', { name: 'Install…' }).click();
    const form = page.locator('form.card');
    await expect(form.getByRole('heading', { name: `Install ${template.name}` })).toBeVisible();
    await expect(form.getByLabel('Slug')).toHaveValue(template.id);
    await form.getByLabel('Project name').fill(name);
    await form.getByLabel('Slug').fill(slug);
    await form.getByRole('button', { name: 'Install and deploy' }).click();
    await expect(page.getByText(/the first deployment is queued/)).toContainText(name);
    expect(failures).toEqual([]);

    await openTab(page, 'Services');
    const row = page.getByRole('row', { name: new RegExp(slug) });
    await expect(row).toBeVisible();
    await expect(row).toContainText(name);

    // best-effort cleanup through the API (the Compose tab owns delete UI)
    const list = await page.request.get('/api/compose-projects');
    const found = ((await list.json()) as { id: string; slug: string }[]).find((p) => p.slug === slug);
    if (found) {
      await page.request.post(`/api/compose-projects/${found.id}/stop`).catch(() => undefined);
      await page.request.delete(`/api/compose-projects/${found.id}`).catch(() => undefined);
    }
  });
}

test('template install rejects a duplicate slug and an invalid slug', async ({ page }) => {
  await signIn(page);
  await openTab(page, 'Templates');
  const slug = unique('kuma');
  const install = async (s: string) => {
    const card = page.locator('article.card', { has: page.getByRole('heading', { name: 'Uptime Kuma', exact: true }) });
    await card.getByRole('button', { name: 'Install…' }).click();
    await page.locator('form.card').getByLabel('Slug').fill(s);
    await page.locator('form.card').getByRole('button', { name: 'Install and deploy' }).click();
  };
  await install(slug);
  await expect(page.getByText(/the first deployment is queued/)).toBeVisible();
  await install(slug);
  await expect(page.getByRole('alert')).toHaveText('a project with this slug already exists');

  const slugInput = page.locator('form.card').getByLabel('Slug');
  await slugInput.fill('Bad Slug');
  expect(await slugInput.evaluate((el: HTMLInputElement) => el.validity.patternMismatch)).toBe(true);
  let posted = false;
  page.on('request', (r) => { if (r.method() === 'POST' && r.url().includes('/install')) posted = true; });
  await page.locator('form.card').getByRole('button', { name: 'Install and deploy' }).click();
  await expect(slugInput).toHaveValue('Bad Slug');
  expect(posted).toBe(false);

  const list = await page.request.get('/api/compose-projects');
  const found = ((await list.json()) as { id: string; slug: string }[]).find((p) => p.slug === slug);
  if (found) {
    await page.request.post(`/api/compose-projects/${found.id}/stop`).catch(() => undefined);
    await page.request.delete(`/api/compose-projects/${found.id}`).catch(() => undefined);
  }
});

test('installed template variables land in the service layer, key names only', async ({ page }) => {
  await signIn(page);
  await openTab(page, 'Templates');
  const slug = unique('gh');
  const card = page.locator('article.card', { has: page.getByRole('heading', { name: 'Ghost', exact: true }) });
  await card.getByRole('button', { name: 'Install…' }).click();
  const form = page.locator('form.card');
  await expect(form.getByLabel(/Public URL/)).toHaveValue('http://localhost:2368');
  await expect(form.getByText(/Generated at install/)).toContainText('DB_PASSWORD');
  await form.getByLabel('Slug').fill(slug);
  await form.getByRole('button', { name: 'Install and deploy' }).click();
  await expect(page.getByText(/the first deployment is queued/)).toBeVisible();
  await expect(page.getByText(/bind a domain to service ghost port 2368/)).toBeVisible();

  const list = await page.request.get('/api/compose-projects');
  const found = ((await list.json()) as { id: string; slug: string }[]).find((p) => p.slug === slug)!;
  const vars = await (await page.request.get(`/api/compose-projects/${found.id}/variables`)).text();
  expect(vars).toContain('DB_PASSWORD');
  expect(vars).toContain('SITE_URL');
  expect(vars).not.toContain('localhost:2368');
  await page.request.post(`/api/compose-projects/${found.id}/stop`).catch(() => undefined);
  await page.request.delete(`/api/compose-projects/${found.id}`).catch(() => undefined);
});

test('the slug inputs enforce their pattern in the browser', async ({ page }) => {
  await signIn(page);
  await openTab(page, PROJECTS);
  await page.getByRole('button', { name: 'New project' }).click();
  const slug = page.getByLabel('Slug');
  await slug.fill('Bad Slug!');
  expect(await slug.evaluate((el: HTMLInputElement) => el.validity.patternMismatch)).toBe(true);
  await slug.fill('good-slug-1');
  expect(await slug.evaluate((el: HTMLInputElement) => el.validity.patternMismatch)).toBe(false);
});

test('installs a template into a non-default project and environment', async ({ page }) => {
  await signIn(page);
  const pslug = await newProject(page, 'tpl');
  const envName = `Stage ${pslug}`;
  await page.getByLabel('New environment name').fill(envName);
  await page.getByRole('button', { name: 'Add environment' }).click();
  await expect(page.getByRole('tab', { name: new RegExp(envName) })).toHaveAttribute('aria-selected', 'true');

  await openTab(page, 'Templates');
  const slug = unique('kuma');
  const card = page.locator('article.card', { has: page.getByRole('heading', { name: 'Uptime Kuma', exact: true }) });
  await card.getByRole('button', { name: 'Install…' }).click();
  const form = page.locator('form.card');
  await form.getByLabel('Slug').fill(slug);
  await form.getByLabel('Project / environment').selectOption({ label: `Name ${pslug} / ${envName}` });
  await form.getByRole('button', { name: 'Install and deploy' }).click();
  await expect(page.getByText(/the first deployment is queued/)).toBeVisible();

  const projects = (await (await page.request.get('/api/projects')).json()) as
    { slug: string; environments: { id: string; name: string; service_count: number }[] }[];
  const env = projects.find((p) => p.slug === pslug)!.environments.find((e) => e.name === envName)!;
  const list = (await (await page.request.get('/api/compose-projects')).json()) as { id: string; slug: string; environment_id: string }[];
  const found = list.find((p) => p.slug === slug)!;
  expect(found.environment_id).toBe(env.id);
  expect(env.service_count).toBe(1);

  await page.request.post(`/api/compose-projects/${found.id}/stop`).catch(() => undefined);
  await page.request.delete(`/api/compose-projects/${found.id}`).catch(() => undefined);
  await openTab(page, PROJECTS);
  await page.getByRole('row', { name: new RegExp(pslug) }).getByRole('button').click();
  await deleteProject(page, pslug);
});
