import { existsSync } from 'node:fs';
import { defineConfig } from '@playwright/test';

// Runs the built UI against a real runtime (default: the local kind cluster's port-forward).
// `npm run test:real` only reads; SLOPPOD_REAL_TURN=1 also runs one small prompt, which costs model usage.

const port = 4198;
const executablePath = process.env.CHROMIUM_PATH ?? (existsSync('/usr/bin/chromium') ? '/usr/bin/chromium' : undefined);

export default defineConfig({
  testDir: 'tests/real',
  workers: 1,
  timeout: 5 * 60_000,
  expect: { timeout: 30_000 },
  reporter: [['list']],
  outputDir: 'test-results/real',
  use: {
    baseURL: `http://localhost:${port}`,
    headless: true,
    launchOptions: { executablePath },
    trace: 'retain-on-failure',
  },
  webServer: {
    command: `vite preview --port ${port} --strictPort`,
    env: { SLOPPOD_URL: process.env.SLOPPOD_URL ?? 'http://localhost:18080' },
    url: `http://localhost:${port}/`,
    reuseExistingServer: false,
  },
});
