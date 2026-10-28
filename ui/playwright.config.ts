import { existsSync } from 'node:fs';
import { defineConfig, devices } from '@playwright/test';

const port = 4199;
// Use the system Chromium when there is one, so no browser download is needed.
const executablePath = process.env.CHROMIUM_PATH ?? (existsSync('/usr/bin/chromium') ? '/usr/bin/chromium' : undefined);

export default defineConfig({
  testDir: 'tests',
  workers: 1,
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
    { name: 'screenshots', testMatch: 'screenshots.spec.ts' },
  ],
});
