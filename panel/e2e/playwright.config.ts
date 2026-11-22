import { existsSync } from 'node:fs';
import { defineConfig } from '@playwright/test';
import { BASE_URL } from './stack.mjs';

// Playwright's own Chromium needs glibc; on Alpine (or to reuse an installed
// browser) point E2E_CHROMIUM at a system binary. /usr/bin/chromium is picked
// up automatically when no bundled browser was downloaded.
const systemChromium = process.env.E2E_CHROMIUM
  ?? (existsSync('/etc/alpine-release') && existsSync('/usr/bin/chromium') ? '/usr/bin/chromium' : undefined);

export default defineConfig({
  testDir: './tests',
  globalSetup: './global-setup.ts',
  // One shared panel and database: keep the order deterministic.
  workers: 1,
  fullyParallel: false,
  timeout: 60_000,
  expect: { timeout: 10_000 },
  reporter: [['list']],
  use: {
    baseURL: BASE_URL,
    trace: 'retain-on-failure',
    screenshot: 'only-on-failure',
    launchOptions: systemChromium ? { executablePath: systemChromium, args: ['--no-sandbox'] } : {},
  },
});
