import { constants, copyFile, mkdir, mkdtemp, readdir, readlink, rm, stat, symlink, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { expect, test as base, withConsole } from './console';

const fixtures = fileURLToPath(new URL('./fixtures/', import.meta.url));
/** An XDG data directory whose `voiceless/models/` holds a model this build can load (fixtures/README.md). */
const modelStore = process.env.VOICELESS_E2E_MODEL_STORE;
/** What the sample says; the recognizer's own hearing of it is compared word by word. */
const reference = 'And so, my fellow Americans: ask not what your country can do for you — ask what you can do for your country.';

type Recorder = 'ok' | 'gone';
type FirstRun = { url: string; recorder: (mode: Recorder) => Promise<void>; sampleEnded: () => Promise<boolean> };

/** Copy a directory tree as it is: directories, files, and symbolic links as links (a model's `current` pointer is one). */
async function copyTree(source: string, target: string) {
  await mkdir(target, { recursive: true });
  for (const name of await readdir(source)) {
    // Exercise model preparation with any seed store.
    if (name === 'silero-vad') continue;
    const from = join(source, name);
    const to = join(target, name);
    const link = await readlink(from).catch(() => null);
    if (link !== null) await symlink(link, to);
    else if ((await stat(from)).isDirectory()) await copyTree(from, to);
    else await copyFile(from, to, constants.COPYFILE_FICLONE);
  }
}

const test = base.extend<{ firstRun: FirstRun }>({
  /** A daemon over a clean XDG root that already holds the local model, hearing the sample instead of a microphone. */
  firstRun: async ({}, use) => {
    const control = await mkdtemp(join(tmpdir(), 'voiceless-first-run-'));
    const mode = join(control, 'recorder');
    try {
      await writeFile(mode, 'ok');
      await withConsole(
        use,
        {
          daemon: true,
          bin: join(fixtures, 'bin'),
          env: { VOICELESS_E2E_PCM: join(fixtures, 'jfk-16k-mono-s16le.pcm'), VOICELESS_E2E_RECORDER: mode },
          prepare: (root) => copyTree(modelStore ?? '', join(root, 'data')),
        },
        ({ url }) => ({
          url,
          recorder: (next) => writeFile(mode, next),
          sampleEnded: () => stat(`${mode}.ended`).then(() => true, (error: NodeJS.ErrnoException) => {
            if (error.code === 'ENOENT') return false;
            throw error;
          }),
        }),
      );
    } finally {
      await rm(control, { recursive: true, force: true });
    }
  },
});

const words = (text: string) => text.toLowerCase().replace(/[^a-z0-9\s]/gu, ' ').split(/\s+/u).filter(Boolean);

/** How much of `expected` `said` keeps, as 1 minus the edit distance over their words. */
function agreement(said: string[], expected: string[]): number {
  const rows = Array.from({ length: said.length + 1 }, (_, row) =>
    Array.from({ length: expected.length + 1 }, (_, column) => (row === 0 ? column : column === 0 ? row : 0)));
  for (const [row, word] of said.entries()) {
    for (let column = 1; column <= expected.length; column += 1) {
      rows[row + 1][column] = Math.min(
        rows[row][column] + 1, rows[row + 1][column - 1] + 1,
        rows[row][column - 1] + (word === expected[column - 1] ? 0 : 1),
      );
    }
  }
  return 1 - rows[said.length][expected.length] / Math.max(said.length, expected.length);
}

test.describe('onboarding', () => {
  test.skip(!modelStore, 'needs VOICELESS_E2E_MODEL_STORE, see e2e/fixtures/README.md');
  test.setTimeout(300_000);

  test('onboarding takes a clean install from the microphone to a trusted local result', async ({ page, firstRun }) => {
    const errors: string[] = [];
    page.on('pageerror', (error) => errors.push(error.message));
    await page.goto(`${firstRun.url}#setup`);

    // The page follows the system default, and the microphone it will hear is the one it lists.
    await expect(page.getByLabel('输入设备').locator('option:checked')).toHaveText('跟随系统默认');
    await expect(page.getByLabel('输入设备').locator('option', { hasText: 'Test Microphone' })).toHaveCount(1);
    await expect(page.getByRole('status').filter({ hasText: 'Test Microphone' })).toBeVisible();
    // Local recognition is what a clean install starts from, with the model that is installed.
    await expect(page.getByLabel('识别方式')).toHaveValue('local-offline');
    const model = page.getByLabel('本地模型');
    await expect(model).not.toHaveValue('');

    // The VAD model is the one thing missing; the page fetches it.
    const prepare = page.getByRole('button', { name: '准备模型' });
    await expect(prepare).toBeVisible();
    await prepare.click();
    await expect(prepare).toBeHidden({ timeout: 90_000 });
    await expect(page.getByText('点击“应用设置”后生效', { exact: true })).toBeVisible();

    // Applying loads the engine before anything may be spoken.
    await page.getByRole('button', { name: '应用设置' }).click();
    await expect(page.getByText('识别就绪', { exact: true })).toBeVisible({ timeout: 180_000 });
    const start = page.getByRole('button', { name: '开始试说' });
    await expect(start).toBeEnabled();

    // The practice hears the sample: its level is on the page while it records.
    await start.click();
    await expect(page.getByText('正在聆听', { exact: true })).toBeVisible();
    await expect.poll(() => page.locator('meter').evaluate((meter: HTMLMeterElement) => meter.value)).toBeGreaterThan(0.01);
    await expect.poll(firstRun.sampleEnded, { timeout: 30_000 }).toBe(true);
    await page.getByRole('button', { name: '完成试说' }).click();
    await expect(page.getByText('试说完成', { exact: true })).toBeVisible({ timeout: 90_000 });

    const result = await page.getByLabel('练习结果').inputValue();
    expect(agreement(words(result), words(reference)), result).toBeGreaterThanOrEqual(0.85);
    // The box was filled from the local session; nothing was typed into any window.
    const status = await page.evaluate(async () => (await fetch('/api/status', {
      headers: { 'X-Voiceless-Token': document.querySelector<HTMLMetaElement>('meta[name=voiceless-token]')!.content },
    })).json());
    expect(status.session).toMatchObject({
      practice: true, phase: 'completed', injection: null,
      backend_selection: { backend: 'local-offline' },
      transcript: { is_final: true, authoritative_text: result },
    });
    expect(errors).toEqual([]);
  });

  test('onboarding names the step a lost microphone broke, and retrying repeats only that step', async ({ page, firstRun }) => {
    await page.goto(`${firstRun.url}#setup`);
    await expect(page.getByRole('status').filter({ hasText: 'Test Microphone' })).toBeVisible();
    await page.getByRole('button', { name: '准备模型' }).click();
    await expect(page.getByRole('button', { name: '准备模型' })).toBeHidden({ timeout: 90_000 });
    await page.getByRole('button', { name: '应用设置' }).click();
    const start = page.getByRole('button', { name: '开始试说' });
    await expect(start).toBeEnabled({ timeout: 180_000 });

    // A recorder whose source is gone fails the practice where it starts, and the page says which step.
    await firstRun.recorder('gone');
    await start.click();
    await expect(page.getByText(/^(麦克风启动失败|录音中断)：/u)).toBeVisible({ timeout: 30_000 });
    const recheck = page.getByRole('button', { name: '重新检查麦克风' });
    await expect(recheck).toBeVisible();

    // Once the microphone is back the settings and the model still hold: only the practice is repeated.
    await firstRun.recorder('ok');
    await recheck.click();
    await expect(start).toBeEnabled();
    await start.click();
    await expect(page.getByText('正在聆听', { exact: true })).toBeVisible();
    await page.getByRole('button', { name: '取消试说' }).click();
    await expect(page.getByText('已取消', { exact: true })).toBeVisible();
  });
});
