import { expect, test } from './console';

test('offline-console loads realtime help, history and settings from loopback', async ({ page, context, consoleUrl }) => {
  const origin = new URL(consoleUrl).origin;
  const external: string[] = [];
  await context.route(/^https?:/u, (route) => {
    const url = route.request().url();
    if (new URL(url).origin === origin) return route.continue();
    external.push(url);
    return route.abort();
  });

  await page.goto(`${consoleUrl}#live`);
  await expect(page.getByText('运行 voiceless daemon 开启实时听写', { exact: true })).toBeVisible();
  await expect(page.getByRole('button', { name: '开始录音', exact: true })).toBeDisabled();
  await page.getByRole('link', { name: '历史', exact: true }).click();
  await expect(page.getByText('说一句话，记录就会出现在这里', { exact: true })).toBeVisible();
  await page.getByRole('link', { name: '识别引擎', exact: true }).click();
  await expect(page.getByRole('spinbutton', { name: '线程', exact: true })).toHaveValue('4');
  expect(external).toEqual([]);
});
