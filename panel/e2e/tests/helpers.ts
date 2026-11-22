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

export async function openTab(page: Page, name: string) {
  await page.getByRole('navigation', { name: 'Primary content' }).getByRole('button', { name, exact: true }).click();
}

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
