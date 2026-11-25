import { expect, test } from './console';

test('apply-config activates a saved revision and restores the running one after a model error', async ({ page, daemonUrl }) => {
  await page.goto(`${daemonUrl}#engine`);
  const threads = page.getByRole('spinbutton', { name: '线程', exact: true });
  const save = page.getByRole('region', { name: '保存更改' }).getByRole('button', { name: '保存', exact: true });
  const applied = page.getByText('设置已生效', { exact: true });
  const applyDialog = page.getByRole('dialog', { name: '应用设置' });
  await expect(applied).toBeVisible();

  await threads.fill('8');
  await save.click();
  await applyDialog.getByRole('button', { name: '稍后' }).click();
  await expect(page.getByText('设置已保存，等待应用', { exact: true })).toBeVisible();
  await page.getByRole('button', { name: '应用设置', exact: true }).click();
  await expect(applied).toBeVisible();

  await page.getByRole('button', { name: '本地流式', exact: true }).first().click();
  await save.click();
  await applyDialog.getByRole('button', { name: '应用', exact: true }).click();
  await expect(page.getByText(/应用失败.*paraformer/i)).toBeVisible();
  await expect(page.getByRole('button', { name: '重试应用' })).toBeVisible();
  await page.getByRole('button', { name: '恢复运行中的设置' }).click();
  await page.getByRole('dialog', { name: '请确认' }).getByRole('button', { name: '恢复', exact: true }).click();
  await expect(applied).toBeVisible();
  await expect(page.getByRole('button', { name: '百炼实时', exact: true }).first()).toHaveAttribute('aria-pressed', 'true');
  await expect(threads).toHaveValue('8');
});
