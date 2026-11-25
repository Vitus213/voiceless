import type { Page } from '@playwright/test';
import { expect, test } from './console';
import { fakeConsole } from './fake-api';

/** Ctrl+K opens the display menu with the keyboard in its search field, whatever the language. */
const openMenu = async (page: Page) => {
  await page.keyboard.press('Control+k');
  await expect(page.locator('#dialog input[type=search]')).toBeFocused();
};

test.describe('accessibility', () => {
  test.use({ viewport: { width: 390, height: 844 } });

  test('accessibility finds a setting by name from the keyboard, and changes and saves it', async ({ page, daemonUrl }) => {
    const errors: string[] = [];
    page.on('pageerror', (error) => errors.push(error.message));
    await page.goto(`${daemonUrl}#live`);
    const search = page.locator('#dialog input[type=search]');

    // Ctrl+K opens the display menu with the keyboard in its search field; Enter takes the best match.
    await openMenu(page);
    await search.fill('设备');
    await expect(page.getByRole('option', { name: '设备 本地模型 · 识别引擎' })).toHaveAttribute('aria-selected', 'true');
    await page.keyboard.press('Enter');
    await expect(page).toHaveURL(/#engine$/u);
    await expect(page.getByRole('dialog')).toHaveCount(0);
    await expect(page.locator('#page [data-setting="设备"] button').first()).toBeFocused();

    // Another setting, reached the same way, is changed and saved without the pointer.
    await openMenu(page);
    await search.fill('线程');
    await page.keyboard.press('Enter');
    const threads = page.getByRole('spinbutton', { name: '线程', exact: true });
    await expect(threads).toBeFocused();
    await page.keyboard.press('Control+a');
    await page.keyboard.type('3');
    const save = page.getByRole('region', { name: '保存更改' }).getByRole('button', { name: '保存', exact: true });
    await save.focus();
    await page.keyboard.press('Enter');
    const applyDialog = page.getByRole('dialog', { name: '应用设置' });
    await applyDialog.getByRole('button', { name: '稍后' }).focus();
    await page.keyboard.press('Enter');
    await expect(page.getByText('设置已保存，等待应用', { exact: true })).toBeVisible();
    await expect(threads).toHaveValue('3');
    expect(errors).toEqual([]);
  });

  test('accessibility keeps the keyboard on the setting found while the first page was still loading', async ({ page, daemonUrl }) => {
    // The first page's last read takes a while: the search is answered before it is done.
    await page.route('**/api/logs', async (route) => {
      await new Promise((resolve) => setTimeout(resolve, 1500));
      await route.continue();
    });
    await page.goto(`${daemonUrl}#live`);
    await openMenu(page);
    await page.locator('#dialog input[type=search]').fill('线程');
    await page.keyboard.press('Enter');
    await expect(page).toHaveURL(/#engine$/u);
    await expect(page.locator('#page [data-setting="线程"] input')).toBeFocused({ timeout: 10_000 });
  });

  test('accessibility offers English, a theme and less motion, and each survives a reload', async ({ page, daemonUrl }) => {
    await page.goto(`${daemonUrl}#live`);
    const root = page.locator('html');
    const search = page.locator('#dialog input[type=search]');
    const menu = page.getByRole('dialog', { name: '显示与搜索' });

    await openMenu(page);
    await menu.getByRole('group', { name: '动画' }).getByRole('button', { name: '减少' }).click();
    await expect(root).toHaveAttribute('data-motion', 'reduced');
    // With less motion, nothing on the page animates or transitions.
    expect(await page.locator('.lamp-dot').evaluate((dot) => getComputedStyle(dot).transitionDuration)).toBe('0s');
    await menu.getByRole('group', { name: '主题' }).getByRole('button', { name: '浅色' }).click();
    await expect(root).toHaveAttribute('data-theme', 'light');
    // Both are set before the app runs after a reload.
    await page.reload();
    await expect(root).toHaveAttribute('data-motion', 'reduced');
    await expect(root).toHaveAttribute('data-theme', 'light');

    // Unpinned, motion follows the system's own reduced-motion setting.
    await page.evaluate(() => localStorage.removeItem('voiceless.motion'));
    await page.emulateMedia({ reducedMotion: 'reduce' });
    await page.reload();
    await expect(root).toHaveAttribute('data-motion', 'reduced');
    await page.emulateMedia({ reducedMotion: 'no-preference' });
    await page.reload();
    await expect(root).toHaveAttribute('data-motion', 'full');

    // English is a whole interface, found by English names too.
    await openMenu(page);
    await menu.getByRole('group', { name: '语言' }).getByRole('button', { name: 'English' }).click();
    await expect(root).toHaveAttribute('lang', 'en');
    await expect(page.getByRole('navigation', { name: 'Console' }).getByRole('link', { name: 'Recognition engine' })).toBeVisible();
    await openMenu(page);
    await expect(search).toHaveAttribute('aria-label', 'Search settings');
    await search.fill('threads');
    await page.keyboard.press('Enter');
    await expect(page.locator('#page [data-setting="Threads"] input')).toBeFocused();
    await expect(page.locator('#page h1')).toHaveText('Recognition engine');
    await expect(page.getByRole('spinbutton', { name: 'Threads', exact: true })).toBeVisible();

    // And Chinese is one again.
    await openMenu(page);
    await page.getByRole('dialog', { name: 'Display and search' }).getByRole('button', { name: '中文' }).click();
    await expect(root).toHaveAttribute('lang', 'zh-CN');
    await expect(page.locator('#page h1')).toHaveText('识别引擎');
  });

  test('accessibility announces phases and the result once, never the live transcript, and a dialog returns the focus', async ({ page, consoleUrl }) => {
    let status = {
      runtime_id: 'runtime', event_sequence: 1, state: 'idle', mode: 'quick', session_id: null as number | null,
      session: null as object | null,
    };
    const api = await fakeConsole(consoleUrl, {
      token: () => 'accessibility-token',
      snapshot: () => status,
      answer: (path) => path === '/api/status' ? { ...status, default_mode: 'quick', gpu: [] }
        : path === '/api/modes' ? [{ id: 'quick', name: '原文' }]
        : path === '/api/logs' ? { lines: [] } : { ok: true },
    });
    try {
      await page.goto(`${api.url}#live`);
      const announcer = page.locator('#announcer');
      await expect(announcer).toHaveAttribute('aria-live', 'polite');
      // The transcript is not a live region any more, anywhere on the page.
      expect(await page.locator('.ticker').getAttribute('aria-live')).toBeNull();
      expect(await page.locator('.sheet').getAttribute('aria-live')).toBeNull();
      expect(await page.getByRole('log').count()).toBe(0);

      // Closing the menu with Escape leaves the keyboard where it was.
      const start = page.getByRole('button', { name: '开始录音', exact: true });
      await start.focus();
      await page.keyboard.press('Control+k');
      await expect(page.getByRole('dialog', { name: '显示与搜索' })).toBeVisible();
      await page.keyboard.press('Escape');
      await expect(page.getByRole('dialog', { name: '显示与搜索' })).toHaveCount(0);
      await expect(start).toBeFocused();

      status = { ...status, event_sequence: 2, state: 'recording', session_id: 7, session: { id: 7, phase: 'recording' } };
      const event = { runtime_id: 'runtime', session_id: 7 };
      api.push({ ...event, type: 'state', event_sequence: 2, state: 'recording', mode: 'quick', phase: 'recording' });
      await expect(announcer).toHaveText('开始录音');
      for (const [sequence, text] of [[3, '今天的'], [4, '今天的会议']]) {
        api.push({ ...event, type: 'transcript', event_sequence: sequence, transcript: { partial_text: text, confirmed_segments: [], is_final: false } });
      }
      await expect(page.locator('.ticker .partial')).toHaveText('今天的会议');
      await expect(announcer).toHaveText('开始录音');

      api.push({ ...event, type: 'transcript', event_sequence: 5, transcript: { authoritative_text: '今天的会议记录', confirmed_segments: ['今天的会议记录'], is_final: true } });
      api.push({ ...event, type: 'finalized', event_sequence: 6, injection: { ok: true, method: 'wtype', message: '', delivery: 'unconfirmed' } });
      await expect(announcer).toHaveText('今天的会议记录，上屏未确认，请检查目标输入框 · 模拟按键');

      // A practice never types: its result is announced as finished instead.
      api.push({ ...event, type: 'snapshot', event_sequence: 7, status: { ...status, event_sequence: 7, state: 'stopping', session_id: 7, session: { id: 7, phase: 'stopping', practice: true } }, default_mode: 'quick' });
      api.push({ ...event, type: 'transcript', event_sequence: 8, transcript: { authoritative_text: '再试一次', confirmed_segments: ['再试一次'], is_final: true } });
      api.push({ ...event, type: 'finalized', event_sequence: 9, injection: null });
      await expect(announcer).toHaveText('再试一次，试说完成');
    } finally {
      await api.close();
    }
  });
});
