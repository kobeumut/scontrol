import { expect, test } from '@playwright/test';
import { ADMIN, openTab, signIn, watchForFailures } from './helpers';

test('rejects a wrong password and signs in and out', async ({ page }) => {
  await page.goto('/');
  await page.getByLabel('Username').fill(ADMIN.username);
  await page.getByLabel('Password').fill('not-the-password');
  await page.getByRole('button', { name: 'Sign in' }).click();
  await expect(page.getByRole('alert')).toHaveText('Wrong username, password or 2FA code.');

  await signIn(page);
  await expect(page.getByText('agent ok')).toBeVisible();
  await page.getByRole('button', { name: 'sign out' }).click();
  await expect(page.getByRole('heading', { name: 'Sign in' })).toBeVisible();
});

test('a session survives a reload', async ({ page }) => {
  await signIn(page);
  await page.reload();
  await expect(page.getByRole('navigation', { name: 'Primary content' })).toBeVisible();
});

const TABS = [
  'Projects & environments', 'Compose projects', 'Templates', 'Databases', 'Backups', 'Observability',
  'Registries', 'Monitoring', 'Hosted sites', 'GitHub Apps', 'Notifications', 'Servers', 'Access', 'API keys',
];

test('every tab loads without an error state', async ({ page }) => {
  const failures = watchForFailures(page);
  await signIn(page);
  for (const tab of TABS) {
    await openTab(page, tab);
    await expect(page.getByText('Loading…')).toHaveCount(0);
    await expect(page.locator('.state.error'), `error state on "${tab}"`).toHaveCount(0);
  }
  expect(failures).toEqual([]);
});
