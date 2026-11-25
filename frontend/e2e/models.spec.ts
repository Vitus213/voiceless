import { Buffer } from 'node:buffer';
import { mkdir, readFile, writeFile } from 'node:fs/promises';
import { join } from 'node:path';
import type { APIRequestContext, Page } from '@playwright/test';
import { expect, test as base, withConsole } from './console';

type Model = { id: string; kind: string; devices: string[]; installed: boolean };

/** Local models of the catalog: the GGUF one a console starts with, another GGUF one, and a Sherpa one. The two families take different devices. */
const gpuModel = 'qwen3-asr-1.7b-gguf';
const otherGpuModel = 'qwen3-asr-0.6b-gguf';
const cpuModel = 'sense-voice-int8';

/** The console's own catalog with `changes` applied by model id: the devices a build with other engines reports, the models its store holds. */
async function serveCatalog(page: Page, changes: Record<string, Partial<Model>>) {
  await page.route('**/api/models', async (route) => {
    const catalog: Model[] = await (await route.fetch()).json();
    await route.fulfill({ json: catalog.map((model) => ({ ...model, ...changes[model.id] })) });
  });
}

/** The `[local]` settings. Their `设备` picker is scoped to them on purpose: it is not the catalog's CPU/GPU filter further down the page. */
const localSettings = (page: Page) => page.locator('#page section.group').filter({ has: page.getByRole('heading', { name: '本地模型', exact: true }) });
/** A tile of the `设备` picker, by its accessible name. */
const choice = (page: Page, name: string) => localSettings(page).getByRole('group', { name: '设备', exact: true }).getByRole('button', { name, exact: true });
/** What the picker offers, by accessible name, in its order. */
const offered = (page: Page) => localSettings(page).getByRole('group', { name: '设备', exact: true }).getByRole('button').evaluateAll((buttons) => buttons.map((button) => button.getAttribute('aria-label')));
const modelMenu = (page: Page) => localSettings(page).getByRole('combobox', { name: '模型', exact: true });
const save = (page: Page) => page.getByRole('region', { name: '保存更改' }).getByRole('button', { name: '保存', exact: true });

/** `[local]` as the console holds it saved. */
async function savedLocal(page: Page, request: APIRequestContext, url: string): Promise<{ model: string; provider?: string }> {
  const headers = { 'X-Voiceless-Token': (await page.locator('meta[name="voiceless-token"]').getAttribute('content'))! };
  const response = await request.get(`${url}api/config`, { headers });
  expect(response.ok()).toBe(true);
  return (await response.json()).local;
}

/** A saved `[local]` that asks for a device its model's catalog entry does not list. */
const explicitVulkan = '[local]\nmodel = "qwen3-asr-1.7b-gguf"\nprovider = "vulkan"\n';

const test = base.extend<{ saved: { url: string; config: string } }>({
  /** A console over that configuration, with the file at hand to see whether the page rewrote it. */
  saved: ({}, use) => withConsole(use, {
    daemon: false,
    async prepare(root) {
      const directory = join(root, 'config', 'voiceless');
      await mkdir(directory, { recursive: true });
      await writeFile(join(directory, 'config.toml'), explicitVulkan, { mode: 0o600 });
    },
  }, ({ url, root }) => ({ url, config: join(root, 'config', 'voiceless', 'config.toml') })),
});

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

test('models offers 默认 and exactly the devices the selected model advertises; the catalog CPU/GPU filter picks no device', async ({ page, consoleUrl }) => {
  // A Vulkan build's catalog, with both local models downloaded so that the model menu can switch between them.
  await serveCatalog(page, { [gpuModel]: { installed: true, devices: ['cpu', 'vulkan', 'auto'] }, [cpuModel]: { installed: true } });
  await page.goto(`${consoleUrl}#engine`);

  // The GGUF model: the model default and what its entry advertises. CUDA is not among them.
  await expect.poll(() => offered(page)).toEqual(['默认', '自动', 'Vulkan', 'CPU']);
  await expect(choice(page, '默认')).toHaveAttribute('aria-pressed', 'true');

  // The catalog's CPU/GPU filter is another control: it hides rows, picks no device and edits nothing.
  await page.getByRole('group', { name: 'CPU 或 GPU', exact: true }).getByRole('button', { name: 'GPU', exact: true }).click();
  await expect(page.getByText('SenseVoice Small (ONNX int8)', { exact: true })).toHaveCount(0);
  await expect(choice(page, '默认')).toHaveAttribute('aria-pressed', 'true');
  await expect(save(page)).toBeHidden();

  // An advertised device is a choice, and so is the model default after it.
  await choice(page, 'Vulkan').click();
  await expect(choice(page, 'Vulkan')).toHaveAttribute('aria-pressed', 'true');
  await expect(choice(page, '默认')).toHaveAttribute('aria-pressed', 'false');
  await choice(page, '默认').click();
  await expect(choice(page, '默认')).toHaveAttribute('aria-pressed', 'true');

  // The Sherpa model advertises the CPU alone; the GGUF model its devices again.
  await modelMenu(page).selectOption(cpuModel);
  await expect.poll(() => offered(page)).toEqual(['默认', 'CPU']);
  await modelMenu(page).selectOption(gpuModel);
  await expect.poll(() => offered(page)).toEqual(['默认', '自动', 'Vulkan', 'CPU']);
});

test('models keeps a saved device the selected model does not advertise selected and disabled until the user replaces it', async ({ page, saved }) => {
  // The catalog of a CUDA build: it offers CUDA, and does not list the saved Vulkan for this model.
  await serveCatalog(page, { [gpuModel]: { devices: ['cpu', 'cuda', 'auto'] } });
  const writes: string[] = [];
  page.on('request', (request) => {
    if (request.method() !== 'GET') writes.push(`${request.method()} ${new URL(request.url()).pathname}`);
  });
  await page.goto(`${saved.url}#engine`);

  const vulkan = choice(page, 'Vulkan');
  await expect(vulkan).toBeVisible();
  await expect(vulkan).toHaveAttribute('aria-pressed', 'true');
  await expect(vulkan).toBeDisabled();
  await expect(vulkan.getByText('不可用', { exact: true })).toBeVisible();
  await expect(choice(page, '默认')).toHaveAttribute('aria-pressed', 'false');
  await expect(choice(page, 'CUDA')).toBeEnabled();

  // Looking the model up is no edit: nothing waits to be saved, and the file is as it was written.
  await expect(page.locator('.savebar')).toHaveAttribute('aria-hidden', 'true');
  expect(writes).toEqual([]);
  expect(await readFile(saved.config, 'utf8')).toBe(explicitVulkan);

  // An explicit choice replaces it: the model default saves, and takes the device out of the file.
  await choice(page, '默认').click();
  await save(page).click();
  await expect(save(page)).toBeHidden();
  expect(writes).toEqual(['PUT /api/config']);
  const stored = await readFile(saved.config, 'utf8');
  expect(stored).toContain(`model = "${gpuModel}"`);
  expect(stored).not.toContain('provider');
});

/** The two ways to choose a model. */
const choosing = [
  { how: 'the model menu', choose: (page: Page) => modelMenu(page).selectOption(cpuModel) },
  { how: '选用 on the catalog row', choose: (page: Page) => page.locator('#page .item').filter({ has: page.getByText(cpuModel, { exact: true }) }).getByRole('button', { name: '选用', exact: true }).click() },
];

for (const { how, choose } of choosing) {
  test(`models resets a device the newly chosen model cannot run on to that model's default from ${how}, and that saves`, async ({ page, request, consoleUrl }) => {
    // A Vulkan build's catalog, both local models downloaded: Vulkan is a device of the GGUF model, and the Sherpa model runs on the CPU alone.
    await serveCatalog(page, { [gpuModel]: { installed: true, devices: ['cpu', 'vulkan', 'auto'] }, [cpuModel]: { installed: true } });
    await page.goto(`${consoleUrl}#engine`);
    await choice(page, 'Vulkan').click();
    await expect(choice(page, 'Vulkan')).toHaveAttribute('aria-pressed', 'true');

    await choose(page);
    await expect(modelMenu(page)).toHaveValue(cpuModel);
    await expect.poll(() => offered(page)).toEqual(['默认', 'CPU']);
    await expect(choice(page, '默认')).toHaveAttribute('aria-pressed', 'true');

    // What the server holds is what the page shows when it is opened again: the model, and no device of its own.
    await save(page).click();
    await expect(save(page)).toBeHidden();
    const local = await savedLocal(page, request, consoleUrl);
    expect(local).toMatchObject({ model: cpuModel });
    expect(local).not.toHaveProperty('provider');
    await page.reload();
    await expect(modelMenu(page)).toHaveValue(cpuModel);
    await expect(choice(page, '默认')).toHaveAttribute('aria-pressed', 'true');
    await expect(choice(page, 'Vulkan')).toHaveCount(0);
  });
}

test('models keeps a chosen device when the model changes to one that advertises it too, and saves both', async ({ page, request, consoleUrl }) => {
  // A Vulkan build's catalog, both GGUF models downloaded: each advertises Vulkan.
  await serveCatalog(page, { [gpuModel]: { installed: true, devices: ['cpu', 'vulkan', 'auto'] }, [otherGpuModel]: { installed: true, devices: ['cpu', 'vulkan', 'auto'] } });
  await page.goto(`${consoleUrl}#engine`);
  await choice(page, 'Vulkan').click();
  await modelMenu(page).selectOption(otherGpuModel);
  await expect(choice(page, 'Vulkan')).toHaveAttribute('aria-pressed', 'true');

  // The device stays the explicit choice it was, next to the new model.
  await save(page).click();
  await expect(save(page)).toBeHidden();
  expect(await savedLocal(page, request, consoleUrl)).toMatchObject({ model: otherGpuModel, provider: 'vulkan' });
});
