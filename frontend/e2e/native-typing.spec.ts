import { mkdir, readFile, writeFile } from 'node:fs/promises';
import { join } from 'node:path';
import type { Page } from '@playwright/test';
import { expect, test as base, withConsole } from './console';

/** The configuration as a Linux installation leaves it: text is committed through Fcitx5. */
const legacyConfig = '[inject]\nprefer = "fcitx5"\n';
type Legacy = { url: string; config: string };

const test = base.extend<{ legacy: Legacy }>({
  legacy: ({}, use) => withConsole(use, {
    daemon: false,
    async prepare(root) {
      const directory = join(root, 'config', 'voiceless');
      await mkdir(directory, { recursive: true });
      await writeFile(join(directory, 'config.toml'), legacyConfig, { mode: 0o600 });
    },
  }, ({ url, root }) => ({ url, config: join(root, 'config', 'voiceless', 'config.toml') })),
});

/**
 * The page of a Windows runtime. A console names its own system in the shell
 * it serves, so the shell this one serves is rewritten to name Windows, and
 * the overlay probe answers as a native runtime does.
 */
async function openWindowsPage(page: Page, url: string) {
  let named: string | undefined;
  await page.route(url, async (route) => {
    const response = await route.fetch();
    const html = await response.text();
    named = /name="voiceless-platform" content="([^"]+)"/u.exec(html)?.[1];
    await route.fulfill({ response, body: html.replace(/(name="voiceless-platform" content=")[^"]+/u, '$1windows') });
  });
  await page.route('**/api/desktop/overlay', (route) => route.fulfill({
    json: { available: false, reason: 'On this platform, the Voiceless window shows the recording status.' },
  }));
  await page.goto(`${url}#hotkey`);
  // The console named its own system in the shell it served; this one runs on Linux.
  expect(named).toMatch(/^(linux|macos|windows)$/u);
}

test('native-typing offers the paste and the copy alone, shows the paste in effect for a Linux preference and leaves it saved', async ({ page, legacy }) => {
  const writes: string[] = [];
  page.on('request', (request) => {
    if (request.method() !== 'GET') writes.push(`${request.method()} ${new URL(request.url()).pathname}`);
  });
  await openWindowsPage(page, legacy.url);
  const typing = page.locator('#page section.group').filter({ has: page.getByRole('heading', { name: '上屏', exact: true }) });

  // A native runtime pastes, or only copies: the Linux tools are not offered, and the
  // paste shows as the one in effect for the saved Linux preference.
  await expect(typing.getByRole('button')).toHaveCount(2);
  await expect(typing.getByRole('button', { name: '剪贴板', exact: true })).toHaveAttribute('aria-pressed', 'true');
  await expect(typing.getByRole('button', { name: '复制到剪贴板', exact: true })).toHaveAttribute('aria-pressed', 'false');
  for (const label of ['输入法', '模拟按键', '失败时回退剪贴板', '输入框内实时预编辑', '粘贴前聚焦']) {
    await expect(page.getByText(label, { exact: true })).toHaveCount(0);
  }

  // Nor are their rows in the settings search; the others still are.
  await page.keyboard.press('Control+k');
  const search = page.locator('#dialog input[type=search]');
  await search.fill('粘贴');
  await expect(page.getByText('没有匹配的设置')).toBeVisible();
  await search.fill('按住');
  await expect(page.getByRole('option', { name: '按住阈值 按键 · 快捷键与上屏' })).toBeVisible();
  await page.keyboard.press('Escape');

  // The window that shows the recording status is named in the interface's language, not the runtime's.
  await expect(page.getByText('录音状态显示在 Voiceless 窗口中')).toBeVisible();
  await expect(page.getByText(/Voiceless window shows the recording status/u)).toHaveCount(0);

  // The Linux preference is neither rewritten nor marked as changed.
  await expect(page.locator('.savebar')).toHaveAttribute('aria-hidden', 'true');
  expect(writes).toEqual([]);
  expect(await readFile(legacy.config, 'utf8')).toBe(legacyConfig);
});
