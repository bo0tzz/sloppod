import { existsSync } from 'node:fs';
import { defineConfig, devices } from '@playwright/test';

const port = 4199;
// Use the system Chromium when there is one, so no browser download is needed.
const executablePath = process.env.CHROMIUM_PATH ?? (existsSync('/usr/bin/chromium') ? '/usr/bin/chromium' : undefined);

export default defineConfig({
  testDir: 'tests',
  workers: 1,
  // In CI a test that fails and then passes is reported as flaky, not failed; the report still lists it for fixing.
  retries: process.env.CI ? 1 : 0,
  fullyParallel: true,
  timeout: 45_000,
  expect: { timeout: 15_000 },
  reporter: [['list']],
  outputDir: 'test-results',
  use: {
    baseURL: `http://localhost:${port}`,
    headless: true,
    launchOptions: { executablePath },
    trace: 'retain-on-failure',
  },
  webServer: {
    command: `node mock/server.mjs --port ${port} --static build --delay 25`,
    url: `http://localhost:${port}/api/v1/repos`,
    reuseExistingServer: false,
  },
  projects: [
    { name: 'e2e', testMatch: 'e2e.spec.ts', use: { ...devices['Desktop Chrome'] } },
    // The scrolling tests again in Firefox, whose scroll and resize timing differ from Chromium's (the browser the
    // user reads the chat in); only the tests tagged `@scroll`. Needs the browser's system libraries, so it runs in CI.
    {
      name: 'firefox',
      testMatch: 'e2e.spec.ts',
      grep: /@scroll/,
      use: { ...devices['Desktop Firefox'], launchOptions: {} },
    },
    { name: 'screenshots', testMatch: 'screenshots.spec.ts' },
  ],
});
