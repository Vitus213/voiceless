import { expect, test as base, withConsole } from './console';
import { mkdir, writeFile } from 'node:fs/promises';
import { join } from 'node:path';

const test = base.extend<{ outputUrl: string }>({
  outputUrl: ({}, use) => withConsole(use, {
    daemon: false,
    prepare: async (root) => {
      const directory = join(root, 'config', 'voiceless');
      await mkdir(directory, { recursive: true });
      await writeFile(join(directory, 'config.toml'), `
[output]
rules = [{ app_id = "kitty", mode = "quick" }, { app_id = "kitty", mode = "voice-polish" }]
pipelines = [
  { mode = "quick", steps = [] },
  { mode = "voice-polish", steps = ["replace", "regex"] },
]
[text]
replacements = [{ from = "SDK", to = "API" }]
rules = [{ pattern = "API", replace = "interface" }]
`, { mode: 0o600 });
    },
  }),
});

test('output-pipeline previews priority, order and manual overrides without starting a session', async ({ page, outputUrl }) => {
  await page.goto(`${outputUrl}#modes`);
  const app = page.getByRole('textbox', { name: '预览应用 ID' });
  const mode = page.getByRole('combobox', { name: '预览模式' });
  const result = page.getByLabel('预览结果');
  const preview = page.getByRole('button', { name: '预览输出' });
  const input = 'SDK\n/tmp/v1';
  await app.fill('kitty');
  await page.getByRole('textbox', { name: '示例文本' }).fill(input);
  await preview.click();
  await expect(result).toHaveText(input, { useInnerText: true });
  await expect(page.getByText(/应用规则 · kitty/u)).toBeVisible();

  await mode.selectOption('voice-polish');
  await preview.click();
  await expect(result).toHaveText('interface\n/tmp/v1', { useInnerText: true });
  await expect(page.getByText(/手动选择 · kitty/u)).toBeVisible();
  await expect(page.getByText('处理前', { exact: true })).toHaveCount(2);

  await page.getByRole('combobox', { name: '编辑步骤的模式' }).selectOption('voice-polish');
  await page.getByRole('button', { name: '上移 应用正则' }).click();
  await page.getByRole('button', { name: '下移', exact: true }).first().click();
  await page.getByRole('button', { name: '保存', exact: true }).click();
  await page.reload();
  await app.fill('kitty');
  await page.getByRole('textbox', { name: '示例文本' }).fill(input);
  await preview.click();
  await expect(result).toHaveText('API\n/tmp/v1', { useInnerText: true });
  await expect(page.getByText(/应用规则 · kitty/u)).toBeVisible();
});
