import { readFile } from 'node:fs/promises';
import { expect, test } from './console';

const preview = { engine: 'sherpa-onnx', requested: 'cpu', api: 'cpu', backend: 'cpu', cpu_fallback: null };
const vulkan = {
  engine: 'transcribe-cpp', requested: 'auto', api: 'vulkan', backend: 'Vulkan0',
  description: 'NVIDIA GeForce RTX 3070', cpu_fallback: null,
};

function status(overrides: Record<string, unknown> = {}) {
  return {
    runtime_id: 'runtime', event_sequence: 5, state: 'recording', session_id: 4,
    backend: 'local-offline', mode: 'quick', gpu: [], version: '0.1.3',
    session: {
      id: 4, phase: 'recording',
      backend_selection: { backend: 'local-offline', model: 'qwen3-asr-1.7b-gguf', requested_device: 'auto' },
      devices: { primary: vulkan, preview },
    },
    ...overrides,
  };
}

test('effective-device shows actual Vulkan and independent CPU preview, then an explicit load failure', async ({ page, consoleUrl }) => {
  let runtime = status();
  await page.route('**/api/status', (route) => route.fulfill({ json: runtime }));
  await page.goto(`${consoleUrl}#live`);
  await expect(page.getByText('Vulkan · NVIDIA GeForce RTX 3070')).toBeVisible();
  await expect(page.getByText('SenseVoice · CPU')).toBeVisible();
  await page.getByRole('link', { name: '识别引擎', exact: true }).click();
  await expect(page.getByText('Vulkan · NVIDIA GeForce RTX 3070')).toBeVisible();
  await expect(page.getByText('SenseVoice · CPU')).toBeVisible();

  runtime = status({
    state: 'idle', session_id: null, session: null,
    engine: {
      loaded: false, model: 'qwen3-asr-1.7b-gguf', requested_device: 'vulkan', devices: null,
      error: 'cannot load qwen3-asr-1.7b-gguf on vulkan: no usable driver',
    },
  });
  await page.getByRole('link', { name: '实时', exact: true }).click();
  await expect(page.getByText('加载失败', { exact: true })).toBeVisible();
  await expect(page.getByText(/no usable driver/u)).toBeVisible();
  await expect(page.getByText('Vulkan · NVIDIA GeForce RTX 3070')).toHaveCount(0);
});

test('effective-device distinguishes service and compute fallback in the redacted download', async ({ page, consoleUrl }) => {
  await page.route('**/api/status', (route) => route.fulfill({ json: status({
    backend: 'cloud-realtime',
    session: {
      id: 4, phase: 'recording', transcript: { authoritative_text: 'PRIVATE_TRANSCRIPT' },
      backend_selection: {
        backend: 'local-offline', model: 'qwen3-asr-1.7b-gguf', requested_device: 'auto',
        fallback: { primary: 'cloud-realtime', reason: 'provider refused PRIVATE_REASON' },
      },
      devices: {
        primary: { ...vulkan, api: 'cpu', backend: 'CPU', description: null, cpu_fallback: 'no_gpu_device' },
        preview,
      },
    },
  }) }));
  await page.goto(`${consoleUrl}#diagnostics`);
  await expect(page.getByText('计算设备回退到 CPU（未找到有可用驱动的 GPU）')).toBeVisible();
  await expect(page.getByText(/provider refused PRIVATE_REASON/u)).toBeVisible();
  const pendingDownload = page.waitForEvent('download');
  await page.getByRole('button', { name: '导出诊断' }).click();
  const download = await pendingDownload;
  const text = await readFile(await download.path(), 'utf8');
  expect(JSON.parse(text)).toMatchObject({
    configured_backend: 'cloud-realtime', effective_backend: 'local-offline',
    fallback_from: 'cloud-realtime', primary_device: { api: 'cpu', requested: 'auto', cpu_fallback: 'no_gpu_device' },
    preview_device: { api: 'cpu', engine: 'sherpa-onnx' },
  });
  expect(text).not.toContain('PRIVATE_');
});
