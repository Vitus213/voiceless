import { readFile } from 'node:fs/promises';
import { join } from 'node:path';
import type { Page } from '@playwright/test';
import { expect, test as base, withConsole } from './console';

type Standalone = { url: string; config: string };

const test = base.extend<{ standalone: Standalone }>({
  standalone: ({}, use) => withConsole(use, { daemon: false }, ({ url, root }) => ({
    url,
    config: join(root, 'config', 'voiceless', 'config.toml'),
  })),
});

/** The row of one shortcut on 快捷键与上屏, by the gesture it starts. */
const shortcut = (page: Page, gesture: string) => page.locator(`#page [data-setting="${gesture}"]`);

test('hotkey recording takes Ctrl+Win in either press order, a right Win key alone, and still ends at a chord\'s key', async ({ page, standalone }) => {
  await page.goto(`${standalone.url}#hotkey`);
  const shown = (gesture: string) => shortcut(page, gesture).locator('kbd');
  const record = (gesture: string) => shortcut(page, gesture).getByRole('button', { name: `录制${gesture}快捷键` });

  // A chord of modifiers is known once one of them is released: Win, then Ctrl, then Ctrl released first.
  await record('按住或点按').click();
  await page.keyboard.down('Meta');
  await page.keyboard.down('Control');
  await expect(shown('按住或点按')).toHaveText('按键…');
  await page.keyboard.up('Control');
  await expect(shown('按住或点按')).toHaveText('Ctrl+Win');

  // The same keys in the other order and released in the other order are the same chord, so they cannot start a second gesture.
  await record('取消录音').click();
  await page.keyboard.down('Control');
  await page.keyboard.down('Meta');
  await page.keyboard.up('Meta');
  await page.keyboard.up('Control');
  await expect(page.getByText('Ctrl+Win 已用于按住或点按', { exact: true })).toBeVisible();
  await expect(shown('取消录音')).toHaveText('—');

  // Ctrl with Shift is recorded the same way.
  await record('取消录音').click();
  await page.keyboard.down('Control');
  await page.keyboard.down('Shift');
  await page.keyboard.up('Shift');
  await expect(shown('取消录音')).toHaveText('Ctrl+ShiftRight');

  // A chord that ends in a key is still recorded when that key goes down, before anything is released.
  await record('原文直出').click();
  await page.keyboard.down('Control');
  await page.keyboard.down('F9');
  await expect(shown('原文直出')).toHaveText('Ctrl+F9');
  await page.keyboard.up('F9');
  await page.keyboard.up('Control');

  // One right Win key is a key of its own; the left one belongs to the desktop.
  await record('按住说话').click();
  await page.keyboard.press('MetaRight');
  await expect(shown('按住说话')).toHaveText('Win');
  await record('点按切换').click();
  await page.keyboard.press('MetaLeft');
  await expect(page.getByText('无法录制 MetaLeft', { exact: true })).toBeVisible();
  await expect(shown('点按切换')).toHaveText('—');

  // Ctrl on its own only modifies the key still to come.
  await record('点按切换').click();
  await page.keyboard.press('Control');
  await expect(shown('点按切换')).toHaveText('按键…');
  await page.keyboard.press('F10');
  await expect(shown('点按切换')).toHaveText('F10');

  // What was recorded is what the daemon is configured with, and what the page shows again.
  await page.getByRole('region', { name: '保存更改' }).getByRole('button', { name: '保存', exact: true }).click();
  await expect(page.getByText('已保存', { exact: true })).toBeVisible();
  const saved = await readFile(standalone.config, 'utf8');
  const recorded = saved
    .slice(saved.indexOf('[hotkey]'))
    .split('\n[')[0]
    .split('\n')
    .map((line) => line.replaceAll(/['"]/gu, ''))
    .slice(0, 6);
  // The file keeps the keys of a section in alphabetical order.
  expect(recorded).toEqual([
    '[hotkey]',
    'cancel_key = Ctrl+KEY_RIGHTSHIFT',
    'hold_key = KEY_RIGHTMETA',
    'key = Ctrl+KEY_RIGHTMETA',
    'raw_key = Ctrl+KEY_F9',
    'toggle_key = KEY_F10',
  ]);
  await page.reload();
  await expect(shown('按住或点按')).toHaveText('Ctrl+Win');
  await expect(shown('取消录音')).toHaveText('Ctrl+ShiftRight');
});
