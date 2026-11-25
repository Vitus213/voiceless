import { expect, test } from './console';
import type { Page } from '@playwright/test';

const device = (page: Page, name: string) => page.getByRole('group', { name: '设备' }).getByRole('button', { name, exact: true });
const threads = (page: Page) => page.getByRole('spinbutton', { name: '线程', exact: true });
const save = (page: Page) => page.getByRole('region', { name: '保存更改' }).getByRole('button', { name: '保存', exact: true });

test('configuration-conflict merges distinct edits and preserves a conflicting draft until confirmed', async ({ page, context, consoleUrl }) => {
  const other = await context.newPage();
  await Promise.all([page.goto(`${consoleUrl}#engine`), other.goto(`${consoleUrl}#engine`)]);
  await expect(device(page, 'Vulkan')).toBeVisible();
  await expect(threads(other)).toHaveValue('4');

  await device(page, 'Vulkan').click();
  await threads(other).fill('8');
  await save(page).click();
  await expect(save(page)).toBeHidden();
  await save(other).click();
  await expect(save(other)).toBeHidden();
  await expect(device(other, 'Vulkan')).toHaveAttribute('aria-pressed', 'true');
  await expect(threads(other)).toHaveValue('8');

  await threads(page).fill('6');
  await save(page).click();
  const conflict = page.getByRole('dialog', { name: '设置已被修改' });
  await expect(conflict.getByText('local.num_threads', { exact: true })).toBeVisible();
  await expect(conflict.getByText('8', { exact: true })).toBeVisible();
  await expect(conflict.getByText('6', { exact: true })).toBeVisible();
  await conflict.getByRole('button', { name: '保留草稿' }).click();
  await expect(threads(page)).toHaveValue('6');
  await other.reload();
  await expect(threads(other)).toHaveValue('8');

  await save(page).click();
  await conflict.getByRole('button', { name: '用我的值覆盖' }).click();
  await expect(save(page)).toBeHidden();
  await other.reload();
  await expect(threads(other)).toHaveValue('6');
  await expect(device(other, 'Vulkan')).toHaveAttribute('aria-pressed', 'true');
});
