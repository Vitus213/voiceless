import type { Page } from '@playwright/test';
import { cp, mkdir, readFile, writeFile } from 'node:fs/promises';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { expect, test, withConsole } from './console';

const sourceTitle = 'Voiceless annotation test source';
const fixtures = fileURLToPath(new URL('./fixtures/', import.meta.url));
const modelStore = process.env.VOICELESS_E2E_MODEL_STORE;
const button = (page: Page, name: string) => page.locator('#page').getByRole('button', { name: new RegExp(`^${name}(?: |$)`, 'u') });
const drawing = '<svg xmlns="http://www.w3.org/2000/svg" width="1000" height="500"><rect width="1000" height="500" fill="#203050"/><rect x="320" y="120" width="160" height="80" fill="#40c060"/></svg>';

// Chromium grants display consent only to the matching tab in this disposable browser.
// Headless Ozone also isolates the system clipboard and offers no physical desktop.
test.use({ channel: 'chromium', launchOptions: {
  executablePath: process.env.PLAYWRIGHT_CHROMIUM_EXECUTABLE_PATH,
  args: ['--ozone-platform=headless', `--auto-select-tab-capture-source-by-title=${sourceTitle}`],
} });

test.beforeEach(async ({ page }) => {
  await page.addInitScript(() => localStorage.setItem('voiceless.locale', JSON.stringify('zh')));
});

async function openImage(page: Page) {
  await page.getByLabel('选择本地图片').setInputFiles({ name: 'synthetic.svg', mimeType: 'image/svg+xml', buffer: Buffer.from(drawing) });
}

test('annotation captures only the dedicated browser tab and releases its track', async ({ page, context, consoleUrl }, info) => {
  const source = await context.newPage();
  await source.route(`${consoleUrl}source`, route => route.fulfill({ contentType: 'text/html', body: `<title>${sourceTitle}</title><body style="margin:0">${drawing}</body>` }));
  await source.goto(`${consoleUrl}source`);
  // The static source must have painted before Chromium can capture its background tab.
  await source.bringToFront();
  await source.locator('svg').screenshot({ path: info.outputPath('source.png') });
  await page.goto(`${consoleUrl}#annotation`);
  await page.bringToFront();
  await button(page, '采集窗口').click();
  await expect(page.getByRole('status').filter({ hasText: '标签页' })).toBeVisible();
  const video = page.locator('video[aria-label="实时采集画面"]');
  await expect(button(page, '拍摄静帧')).toBeEnabled({ timeout: 15_000 });
  const track = await video.evaluateHandle(el => ((el as HTMLVideoElement).srcObject as MediaStream).getVideoTracks()[0]);
  // Wait for the fixture color to arrive, beyond metadata/the first black frame.
  await expect.poll(() => video.evaluate(el => {
    const ctx = document.createElement('canvas').getContext('2d')!;
    ctx.drawImage(el as HTMLVideoElement, 360, 140, 1, 1, 0, 0, 1, 1);
    return ctx.getImageData(0, 0, 1, 1).data[1];
  })).toBeGreaterThan(155); // tab capture converts RGB through the video color space
  await button(page, '拍摄静帧').click();
  expect(await track.evaluate(t => t.readyState)).toBe('ended');
  for (const [label, value] of [['左','300'], ['上','100'], ['宽','200'], ['高','100']]) {
    await page.getByRole('spinbutton', { name: label, exact: true }).fill(value);
  }
  await button(page, '标注此区域').click();
  const pixels = await page.getByLabel('标注画布').evaluate(el => {
    const canvas = el.previousElementSibling as HTMLCanvasElement;
    const ctx = canvas.getContext('2d')!;
    return [canvas.width, canvas.height, ctx.getImageData(5,5,1,1).data[1], ctx.getImageData(25,25,1,1).data[1]];
  });
  expect(pixels.slice(0,2)).toEqual([200,100]);
  expect(pixels[2]).toBeLessThan(75);
  expect(pixels[3]).toBeGreaterThan(155);
  await page.getByLabel('标注画布').locator('..').screenshot({ path: info.outputPath('captured-region.png') });
  await button(page, '取消').click();
  await expect(page.getByLabel('选择本地图片')).toBeVisible();
});

test('annotation denied by browser policy can use an image and cancel without delivery', async ({ page, context, consoleUrl }) => {
  await page.route(consoleUrl, async route => {
    const response = await route.fetch();
    await route.fulfill({ response, headers: { ...response.headers(), 'Permissions-Policy': 'display-capture=()' } });
  });
  const shares: string[] = [];
  await page.exposeFunction('takeShare', (data: ShareData) => shares.push(String(data.text)));
  await page.addInitScript(() => Object.defineProperty(navigator, 'share', { value: Reflect.get(window, 'takeShare') }));
  const downloads: string[] = [];
  page.on('download', d => downloads.push(d.suggestedFilename()));
  await context.grantPermissions(['clipboard-read', 'clipboard-write'], { origin: consoleUrl });
  await page.goto(`${consoleUrl}#annotation`);
  await page.evaluate(() => navigator.clipboard.writeText('test sentinel'));
  await button(page, '采集屏幕').click();
  await expect(page.getByText('屏幕采集被拒绝或未选择来源', { exact: true })).toBeVisible();
  await openImage(page);
  await button(page, '标注整张').click();
  await page.getByRole('textbox', { name: '解说文字' }).fill('discarded annotation');
  await button(page, '取消').click();
  await expect(page.getByText('已取消，未保存也未发送任何内容')).toBeVisible();
  await openImage(page);
  await button(page, '标注整张').click();
  await expect(page.getByRole('textbox', { name: '解说文字' })).toHaveValue('');
  await expect(button(page, '撤销')).toBeDisabled();
  expect(await page.evaluate(() => navigator.clipboard.readText())).toBe('test sentinel');
  expect(downloads).toEqual([]);
  expect(shares).toEqual([]);
});

test('annotation previews local speech and exports the selected annotated pixels and approved words', async ({ page, context }, info) => {
  test.skip(!modelStore, 'needs SenseVoice and Silero VAD in VOICELESS_E2E_MODEL_STORE');
  test.setTimeout(150_000);
  const shares: string[] = [];
  await page.exposeFunction('takeShare', (data: ShareData) => shares.push(data.text!));
  await page.addInitScript(() => Object.defineProperty(navigator, 'share', { value: Reflect.get(window, 'takeShare') }));
  const env: Record<string,string | undefined> = { DISPLAY: undefined, WAYLAND_DISPLAY: undefined, VOICELESS_E2E_PCM: join(fixtures, 'jfk-16k-mono-s16le.pcm') };
  await withConsole<string>(async url => {
    await context.grantPermissions(['clipboard-read','clipboard-write'], { origin: url });
    await page.goto(`${url}#annotation`);
    await openImage(page);
    for (const [label, value] of [['左','300'], ['上','100'], ['宽','200'], ['高','100']]) {
      await page.getByRole('spinbutton', { name: label, exact: true }).fill(value);
    }
    await button(page, '标注此区域').click();
    const text = page.getByRole('textbox', { name: '解说文字' });
    const status = () => page.evaluate(async () => (await fetch('/api/status', {
      headers: { 'X-Voiceless-Token': document.querySelector<HTMLMetaElement>('meta[name=voiceless-token]')!.content },
    })).json());
    expect((await status()).session).toBeNull();
    await expect(text).toHaveValue('');
    await button(page, '开始解说').click();
    await expect(page.getByText('正在聆听', { exact: true })).toBeVisible({ timeout: 60_000 });
    await expect.poll(() => page.getByLabel('麦克风电平').evaluate(el => (el as HTMLMeterElement).value)).toBeGreaterThan(0.01);
    // The recorder streams the entire eleven-second fixture at real time.
    await page.waitForTimeout(11_500);
    await button(page, '完成解说').click();
    await expect(text).toBeEnabled({ timeout: 60_000 });
    await expect(text).toHaveValue(/my fellow american.*your country can do for you.*you can do for your country/i);
    const transcript = await text.inputValue();
    expect((await status()).session).toMatchObject({ purpose: 'narration', phase: 'completed', injection: null,
      backend_selection: { backend: 'local-offline' }, transcript: { is_final: true, authoritative_text: transcript } });
    await info.attach('local-transcript', { body: transcript, contentType: 'text/plain' });
    const canvas = page.getByLabel('标注画布');
    await canvas.scrollIntoViewIfNeeded();
    const image = await canvas.evaluate(el => {
      const box = el.previousElementSibling!.getBoundingClientRect();
      return { x: box.x, y: box.y, width: box.width, height: box.height };
    });
    await page.mouse.move(image.x + 30, image.y + 50);
    await page.mouse.down();
    await page.mouse.move(image.x + 150, image.y + 50, { steps: 4 });
    await page.mouse.up();
    await expect(button(page, '撤销')).toBeEnabled();
    expect(shares).toEqual([]);
    const downloading = page.waitForEvent('download');
    await button(page, '保存图片').click();
    const file = await downloading;
    await file.saveAs(info.outputPath('annotated.png'));
    const png = await readFile((await file.path())!);
    await info.attach('annotated.png', { body: png, contentType: 'image/png' });
    const pixels = await page.evaluate(async data => {
      const img = new Image(); img.src = `data:image/png;base64,${data}`; await img.decode();
      const canvas = document.createElement('canvas'); canvas.width = img.width; canvas.height = img.height;
      const ctx = canvas.getContext('2d')!; ctx.drawImage(img, 0, 0);
      const pixel = (x: number, y: number) => Array.from(ctx.getImageData(x,y,1,1).data).slice(0,3);
      return { width: img.width, height: img.height, outside: pixel(5,5), inside: pixel(25,25), tip: pixel(149,50), caption: pixel(0,101) };
    }, png.toString('base64'));
    expect(pixels).toMatchObject({ width: 200, outside: [32,48,80], inside: [64,192,96], tip: [255,59,48], caption: [20,21,23] });
    expect(pixels.height).toBeGreaterThan(100);
    await button(page, '复制文字').click();
    expect(await page.evaluate(() => navigator.clipboard.readText())).toBe(transcript);
    await button(page, '分享文字').click();
    expect(shares).toEqual([transcript]);
    await button(page, '开始解说').click();
    await expect(page.getByText('正在聆听', { exact: true })).toBeVisible({ timeout: 60_000 });
    await button(page, '取消').click();
    await expect.poll(async () => (await status()).session.phase).toBe('cancelled');
    expect((await status()).session.injection).toBeNull();
    expect(shares).toEqual([transcript]);
    console.log(JSON.stringify({ file: file.suggestedFilename(), pixels, transcript, receipts: shares.length }));
  }, { daemon: true, bin: join(fixtures, 'bin'), env, async prepare(root) {
    env.XDG_RUNTIME_DIR = root;
    env.XDG_DATA_DIRS = root;
    env.GIO_USE_VFS = 'local';
    env.VOICELESS_E2E_RECORDER = join(root, 'recorder');
    await writeFile(env.VOICELESS_E2E_RECORDER, 'ok');
    await writeFile(join(root, 'config.toml'), '[console]\nport=0\n[audio]\npause_media=false\ncues=false\n[local]\nmodel="sense-voice-int8"\n');
    const target = join(root, 'data/voiceless/models');
    await mkdir(target, { recursive: true });
    await cp(join(modelStore!, 'voiceless/models'), target, { recursive: true, verbatimSymlinks: true });
  } });
});
