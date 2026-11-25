import { expect, test } from './console';

const existingRoutes = [
  ['live', '实时'],
  ['engine', '识别引擎'],
  ['words', '词库'],
  ['modes', '模式与提示词'],
  ['hotkey', '快捷键与上屏'],
  ['history', '历史'],
  ['diagnostics', '诊断'],
] as const;

test.use({ viewport: { width: 390, height: 844 } });

test('navigation preserves existing links, device choices and keyboard focus', async ({ page, consoleUrl }) => {
  const errors: Error[] = [];
  page.on('pageerror', (error) => errors.push(error));
  // The picker offers a device only where the catalog lists it for the selected model, so this is
  // the catalog of a build with both GPU engines, whatever the build under test has.
  await page.route('**/api/models', async (route) => {
    const response = await route.fetch();
    const catalog: { kind: string; devices: string[] }[] = await response.json();
    await route.fulfill({ json: catalog.map((model) => model.kind === 'gguf' ? { ...model, devices: ['cpu', 'cuda', 'vulkan', 'auto'] } : model) });
  });
  await page.goto(`${consoleUrl}#live`);
  const nav = page.getByRole('navigation', { name: '控制台' });
  const session = page.getByRole('region', { name: '语音输入' });
  for (const [route, name] of existingRoutes) {
    await page.goto(`${consoleUrl}#${route}`);
    await expect(nav.getByRole('link', { name, exact: true })).toHaveAttribute('aria-current', 'page');
    await expect(page.locator('main h1')).toBeVisible();
    await expect(session).toBeVisible();
    expect(await page.evaluate(() => document.documentElement.scrollWidth)).toBeLessThanOrEqual(390);
  }
  await nav.getByRole('link', { name: '识别引擎' }).focus();
  await page.keyboard.press('Enter');
  await expect(page).toHaveURL(/#engine$/u);
  await expect(page.locator('#page')).toBeFocused();
  await expect(page.getByRole('heading', { name: '本地模型', exact: true })).toBeVisible();
  const devices = page.getByRole('group', { name: '设备' });
  for (const name of ['默认', '自动', 'CUDA', 'Vulkan', 'CPU']) {
    await expect(devices.getByRole('button', { name, exact: true })).toBeVisible();
  }
  await devices.getByRole('button', { name: 'Vulkan' }).click();
  await expect(devices.getByRole('button', { name: 'Vulkan' })).toHaveAttribute('aria-pressed', 'true');
  await page.getByRole('link', { name: '跳到主要内容' }).focus();
  await page.keyboard.press('Enter');
  await expect(page.locator('#page')).toBeFocused();
  await expect(page).toHaveURL(/#engine$/u);
  expect(errors).toEqual([]);
});
