import { expect, Page } from '@playwright/test';
import { ADMIN } from '../stack.mjs';

export { ADMIN };

/** A name no other test (or an earlier run against a reused stack) has used. */
export function unique(prefix: string): string {
  return `${prefix}-${Date.now().toString(36)}${Math.floor(Math.random() * 1296).toString(36)}`;
}

export async function signIn(page: Page, user: { username: string; password: string } = ADMIN) {
  await page.goto('/');
  await page.getByLabel('Username').fill(user.username);
  await page.getByLabel('Password').fill(user.password);
  await page.getByRole('button', { name: 'Sign in' }).click();
  await expect(page.getByRole('navigation', { name: 'Primary content' })).toBeVisible();
}

/**
 * Opens a screen from the sidebar by its exact label. On a narrow viewport
 * the sidebar is behind the "Menu" button, which is opened first.
 */
export async function openTab(page: Page, name: string) {
  const menu = page.getByRole('button', { name: 'Menu', exact: true });
  if (await menu.isVisible() && (await menu.getAttribute('aria-expanded')) !== 'true') await menu.click();
  await page.getByRole('navigation', { name: 'Primary content' }).getByRole('button', { name, exact: true }).click();
}

/** Labels of the sidebar entries an administrator sees, in order. */
export const NAV_ENTRIES = [
  'Projects', 'Services', 'Templates', 'Databases', 'Monitoring', 'Observability', 'Backups', 'Hosted sites',
  'Registries', 'GitHub Apps', 'Notifications', 'Servers', 'Access', 'API keys', 'Account',
];

/**
 * Collects what a user would never see but a test must not miss: API
 * responses with a server error, uncaught page errors, and console errors.
 */
export function watchForFailures(page: Page) {
  const failures: string[] = [];
  page.on('response', (response) => {
    if (response.url().includes('/api/') && response.status() >= 500) {
      failures.push(`${response.status()} ${response.request().method()} ${response.url()}`);
    }
  });
  page.on('pageerror', (error) => failures.push(`page error: ${error.message}`));
  return failures;
}
