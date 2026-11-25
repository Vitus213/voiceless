import { defineConfig } from '@playwright/test';

export default defineConfig({
  testDir: 'e2e',
  retries: 0,
  // The interface follows the browser's language; the tests are written against the Chinese one.
  use: {
    browserName: 'chromium',
    locale: 'zh-CN',
    headless: true,
    launchOptions: process.env.PLAYWRIGHT_CHROMIUM_EXECUTABLE_PATH
      ? {
          executablePath: process.env.PLAYWRIGHT_CHROMIUM_EXECUTABLE_PATH,
          args: ['--ozone-platform=headless'],
        }
      : undefined,
  },
});
