import { Buffer } from 'node:buffer';
import { expect, test } from './console';

test('vocabulary imports only confirmed entries and applies them to the selected project', async ({ page, consoleUrl }) => {
  await page.goto(`${consoleUrl}#words`);
  await page.getByRole('button', { name: '添加词库范围' }).click();
  await page.getByRole('combobox', { name: '词库模式' }).selectOption('quick');
  await page.getByRole('textbox', { name: '词库项目' }).fill('docs');
  await page.getByRole('dialog').getByRole('button', { name: '添加', exact: true }).click();
  await page.getByLabel('导入词库').setInputFiles({
    name: 'terms.json',
    mimeType: 'application/json',
    buffer: Buffer.from(JSON.stringify({
      hotwords: ['张三丰', '张三丰', 'OpenAPI'],
      replacements: [{ from: 'SDK', to: 'Client API' }, { from: 'SDK', to: 'Should not win' }],
    })),
  });
  const dialog = page.getByRole('dialog');
  await expect(dialog).toContainText('新增 3 条，重复 1 条，冲突 1 条');
  await dialog.getByRole('button', { name: '导入无冲突词条' }).click();
  await page.getByRole('button', { name: '保存', exact: true }).click();
  await expect(page.getByText('词库已保存', { exact: true })).toBeVisible();

  await page.goto(`${consoleUrl}#modes`);
  await page.getByRole('textbox', { name: '示例文本' }).fill('张三峰看 SDK');
  const preview = page.getByRole('button', { name: '预览输出' });
  const result = page.getByLabel('预览结果');
  await preview.click();
  await expect(result).toHaveText('张三峰看 SDK');
  const project = page.getByRole('textbox', { name: '预览项目' });
  await project.fill('docs');
  await preview.click();
  await expect(result).toHaveText('张三丰看 Client API');
  await project.fill('other');
  await preview.click();
  await expect(result).toHaveText('张三峰看 SDK');
});
