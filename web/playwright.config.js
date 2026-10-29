// End-to-end tests of the review UI against a running `rowbit serve`.
// e2e/run.sh starts one on the test database and runs them.
import { defineConfig } from '@playwright/test';

export default defineConfig({
  testDir: 'e2e',
  timeout: 30000,
  use: { baseURL: process.env.ROWBIT_URL ?? 'http://127.0.0.1:8099' },
  projects: [
    {
      name: 'chromium',
      use: {
        browserName: 'chromium',
        // A Chromium already on the machine, instead of Playwright's own.
        launchOptions: process.env.ROWBIT_E2E_CHROMIUM
          ? { executablePath: process.env.ROWBIT_E2E_CHROMIUM }
          : {},
      },
    },
  ],
});
