import { Buffer } from 'node:buffer';
import { expect, test } from './console';

test('models filters the catalog by language and device, and refuses files that are no models', async ({ page, consoleUrl }) => {
  const catalogResponse = page.waitForResponse(response => new URL(response.url()).pathname === '/api/models' && response.ok());
  await page.goto(`${consoleUrl}#engine`);
  const catalog: { id: string; devices: string[] }[] = await (await catalogResponse).json();
  const ggufDevices = catalog.find(model => model.id === 'qwen3-asr-0.6b-gguf')?.devices;
  expect(ggufDevices).toBeDefined();
  const sensevoice = page.getByText('SenseVoice Small (ONNX int8)', { exact: true });
  const gguf = page.getByText('Qwen3-ASR 0.6B (GGUF Q8_0)', { exact: true });
  await expect(sensevoice).toBeVisible();

  // A language keeps the models that speak it.
  await page.getByLabel('语言', { exact: true }).selectOption({ label: '阿拉伯语' });
  await expect(gguf).toBeVisible();
  await expect(sensevoice).toHaveCount(0);

  // A device keeps the models this build can run on it.
  await page.getByLabel('语言', { exact: true }).selectOption({ label: '全部' });
  const device = page.getByRole('group', { name: 'CPU 或 GPU', exact: true });
  await device.getByRole('button', { name: 'GPU', exact: true }).click();
  await expect(sensevoice).toHaveCount(0);
  if (ggufDevices?.some(device => device === 'cuda' || device === 'vulkan')) {
    await expect(gguf).toBeVisible();
  } else {
    await expect(gguf).toHaveCount(0);
    await expect(page.getByText('没有符合筛选的模型', { exact: true })).toBeVisible();
  }
  await device.getByRole('button', { name: 'CPU', exact: true }).click();
  await expect(sensevoice).toBeVisible();

  // A file that is no model is refused before anything leaves the browser.
  const picker = page.locator('#page input[type=file]');
  await picker.setInputFiles({ name: 'notes.txt', mimeType: 'text/plain', buffer: Buffer.from('hello') });
  await expect(page.getByText('请选择 .gguf 文件', { exact: true })).toBeVisible();
  await picker.setInputFiles({ name: 'empty.gguf', mimeType: 'application/octet-stream', buffer: Buffer.from('') });
  await expect(page.getByText('文件是空的', { exact: true })).toBeVisible();
});
