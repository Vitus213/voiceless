import { appendFile, cp, mkdir, readFile, writeFile } from 'node:fs/promises';
import { join } from 'node:path';
import { DatabaseSync } from 'node:sqlite';
import { expect, test, withConsole } from './console';

const id = '00000000-0000-4000-8000-000000000056';
const original = 'Unedited transcript.';
const rowSelector = `article.rec[data-id="${id}"]`;
test.use({ locale: 'zh-CN' });

function seed(root: string, audio: string | null) {
  const db = new DatabaseSync(join(root, 'data/voiceless/history.sqlite3'));
  try {
    db.exec('PRAGMA busy_timeout = 5000');
    db.prepare(`INSERT INTO recognition_history
      (id,created_at,raw_text,final_text,status,character_count,asr_provider,asr_model,audio_path)
      VALUES (?, ?, ?, ?, 'completed', ?, 'cloud-realtime', 'previous-model', ?)`)
      .run(id, new Date().toISOString(), original, original, original.length, audio);
  } finally { db.close(); }
}

test('history-revisions corrects, copies and restores a record whose audio disappeared', async ({ page, context }) => {
  await withConsole(async ({ url, root }) => {
    seed(root, join(root, 'removed.wav'));
    await context.grantPermissions(['clipboard-read', 'clipboard-write'], { origin: url });
    await page.goto(`${url}#history`);
    const row = page.locator(rowSelector);
    await row.locator('.txt').click();
    await expect(row.getByRole('button', { name: '重新识别' })).toBeDisabled();
    await row.getByRole('button', { name: '纠错' }).click();
    const dialog = page.getByRole('dialog');
    await dialog.getByRole('textbox', { name: '正确的文字' }).fill('Edited transcript.');
    await dialog.getByRole('button', { name: '对比' }).click();
    await expect(row.locator('.compare ins').first()).toBeVisible();
    await expect(row.locator('.txt')).toHaveText(original);
    await row.getByRole('button', { name: '确认替换' }).click();
    await expect(row.locator('.txt')).toHaveText('Edited transcript.');
    await row.getByRole('button', { name: '复制' }).click();
    await expect.poll(() => page.evaluate(() => navigator.clipboard.readText())).toBe('Edited transcript.');
    await page.reload();
    await row.locator('.txt').click();
    await row.getByRole('button', { name: '恢复此版本' }).click();
    await expect(row.locator('.final')).toHaveText(original);
    await expect(row.locator('.compare').filter({ hasText: '纠错' }).getByRole('button', { name: '恢复此版本' })).toBeVisible();
  }, { daemon: false }, running => running);
});

const store = process.env.VOICELESS_E2E_MODEL_STORE;

test('history-revisions plays the original WAV and compares a selected offline model before adoption', async ({ page }) => {
  test.skip(!store, 'requires the offline model and silero-vad in VOICELESS_E2E_MODEL_STORE');
  test.setTimeout(240_000);
  await withConsole(async ({ url, root }) => {
    const audio = join(root, 'speech.wav');
    const pcm = await readFile(new URL('./fixtures/jfk-16k-mono-s16le.pcm', import.meta.url));
    const header = Buffer.alloc(44);
    header.write('RIFF'); header.writeUInt32LE(36 + pcm.length, 4); header.write('WAVEfmt ', 8);
    header.writeUInt32LE(16, 16); header.writeUInt16LE(1, 20); header.writeUInt16LE(1, 22);
    header.writeUInt32LE(16000, 24); header.writeUInt32LE(32000, 28); header.writeUInt16LE(2, 32);
    header.writeUInt16LE(16, 34); header.write('data', 36); header.writeUInt32LE(pcm.length, 40);
    await writeFile(audio, Buffer.concat([header, pcm]));
    seed(root, audio);
    await page.goto(`${url}#history`);
    const row = page.locator(rowSelector);
    await row.locator('.txt').click();
    await row.getByRole('button', { name: '播放', exact: true }).click();
    await expect.poll(() => row.locator('.t-cur').innerText()).not.toBe('0:00');
    await row.getByRole('button', { name: '重新识别' }).click();
    const dialog = page.getByRole('dialog');
    const model = 'sense-voice-int8';
    await dialog.getByRole('combobox').selectOption(model);
    await dialog.getByRole('button', { name: '重新识别' }).click();
    const proposal = row.locator('.compare').filter({ hasText: model });
    await expect(proposal.getByRole('button', { name: '确认替换' })).toBeVisible({ timeout: 160_000 });
    await expect(row.locator('.txt')).toHaveText(original);
    await proposal.getByRole('button', { name: '确认替换' }).click();
    await expect(row.locator('.txt')).toContainText(/country/i);
    await row.getByRole('button', { name: '恢复此版本' }).click();
    await expect(row.locator('.final')).toHaveText(original);

    await writeFile(audio, 'damaged WAV');
    await row.getByRole('button', { name: '重新识别' }).click();
    await dialog.getByRole('button', { name: '重新识别' }).click();
    await expect(row.getByRole('status')).toContainText(/damaged|WAV|end|failed/i);
    await expect(row.locator('.txt')).toHaveText(original);
  }, { daemon: true, async prepare(root) {
    // The live GGUF device must not be passed to the CPU-only history model.
    await appendFile(join(root, 'config.toml'), '\n[local]\nmodel = "qwen3-asr-1.7b-gguf"\nprovider = "vulkan"\nmax_segment_seconds = 45\n');
    const target = join(root, 'data/voiceless');
    await mkdir(target, { recursive: true });
    await cp(join(store!, 'voiceless/models'), join(target, 'models'), { recursive: true, verbatimSymlinks: true });
  } }, running => running);
});
