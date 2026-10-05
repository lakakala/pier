import { defineConfig } from '@playwright/test';
const scheme = process.env.PIER_E2E_SCHEME ?? 'https';
if (!['http', 'https'].includes(scheme)) throw new Error('PIER_E2E_SCHEME must be http or https');
const baseURL = scheme === 'http' ? 'http://pier-http.test:18084' : 'https://localhost:8444';
export default defineConfig({
  testDir: './tests',
  workers: 1,
  timeout: 45000,
  outputDir: `test-results/${scheme}`,
  use: { baseURL, ignoreHTTPSErrors: scheme === 'https', trace: 'retain-on-failure' },
  webServer: {
    command: 'node scripts/e2e-server.mjs',
    url: `${baseURL}/v1/auth/status`,
    ignoreHTTPSErrors: scheme === 'https',
    reuseExistingServer: false,
    timeout: 30000,
  },
});
