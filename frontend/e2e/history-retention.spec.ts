import { mkdir, readFile, writeFile } from 'node:fs/promises';
import { join } from 'node:path';
import { DatabaseSync } from 'node:sqlite';
import { expect, test, withConsole } from './console';

const id = '00000000-0000-4000-8000-000000000001';

function wave() {
  const bytes = Buffer.alloc(48);
  bytes.write('RIFF'); bytes.writeUInt32LE(40, 4); bytes.write('WAVEfmt ', 8);
  bytes.writeUInt32LE(16, 16); bytes.writeUInt16LE(1, 20); bytes.writeUInt16LE(1, 22);
  bytes.writeUInt32LE(16000, 24); bytes.writeUInt32LE(32000, 28); bytes.writeUInt16LE(2, 32);
  bytes.writeUInt16LE(16, 34); bytes.write('data', 36); bytes.writeUInt32LE(4, 40);
  return bytes;
}

test('history-retention exports filtered and selected records, then expires audio before text', async ({ page, request }) => {
  await withConsole(async ({ url, root }: { url: string; root: string }) => {
    const dir = join(root, 'data', 'voiceless');
    const recording = join(dir, 'audio', `${id}.wav`);
    await mkdir(join(dir, 'audio'), { recursive: true, mode: 0o700 });
    await writeFile(recording, wave(), { mode: 0o600 });
    const db = new DatabaseSync(join(dir, 'history.sqlite3'));
    const row = db.prepare(`INSERT INTO recognition_history(id,created_at,raw_text,final_text,status,character_count,audio_path)
      VALUES(?, '2020-01-01T00:00:00Z', ?, ?, 'completed', 5, ?)`);
    row.run(id, 'alpha one', 'alpha one', recording);
    row.run('second', 'alpha two', 'alpha two', null);
    row.run('third', 'other', 'other', null);
    db.close();
    await page.goto(`${url}#history`);
    const search = page.getByRole('searchbox', { name: '搜索记录' });
    await search.fill('alpha'); await search.press('Enter');
    await expect(page.locator('article.rec')).toHaveCount(2);
    const exportButton = page.getByRole('button', { name: '导出记录', exact: true });
    await exportButton.click();
    await expect(page.getByLabel('包含录音文件')).not.toBeChecked();
    const downloading = page.waitForEvent('download');
    await page.getByRole('button', { name: '下载', exact: true }).click();
    const download = await downloading;
    expect(download.suggestedFilename()).toMatch(/\.txt$/);
    const document = await readFile((await download.path())!, 'utf8');
    expect(document).toContain('alpha one'); expect(document).toContain('alpha two'); expect(document).not.toContain('other');
    await exportButton.click();
    await page.getByLabel('导出范围').selectOption('ids');
    await page.getByLabel('alpha one', { exact: true }).check();
    await page.getByLabel('导出格式').selectOption('json');
    await page.getByLabel('包含录音文件').check();
    const zipping = page.waitForEvent('download');
    await page.getByRole('button', { name: '下载', exact: true }).click();
    const archive = await readFile((await (await zipping).path())!);
    expect(archive.readUInt32LE()).toBe(0x04034b50);
    expect(archive.includes(wave())).toBe(true);
    expect(archive.includes(Buffer.from('alpha two'))).toBe(false);

    const headers = { 'X-Voiceless-Token': (await page.locator('meta[name="voiceless-token"]').getAttribute('content'))! };
    const invalid = await request.post(`${url}api/history/retention/confirm`, { headers, data: { snapshot: 'unissued' } });
    expect(invalid.status()).toBe(400);
    const policy = page.getByRole('button', { name: '保留策略', exact: true });
    await policy.click();
    await page.getByLabel('录音保留天数', { exact: true }).fill('7');
    await page.getByRole('button', { name: '预览清理' }).click();
    await expect(page.getByRole('dialog')).toContainText('0 条文本、1 段录音');
    await page.getByRole('button', { name: '取消' }).click();
    expect(await readFile(recording)).toEqual(wave());
    await policy.click();
    await page.getByLabel('录音保留天数', { exact: true }).fill('7');
    await page.getByRole('button', { name: '预览清理' }).click();
    await page.getByRole('button', { name: '确认清理并保存策略' }).click();
    await expect(page.getByText('记录设置已保存', { exact: true })).toBeVisible();
    await expect(page.locator(`article[data-id="${id}"] .play`)).toBeDisabled();
    await expect(page.locator('article.rec')).toHaveCount(2);
    await policy.click();
    await expect(page.getByLabel('录音保留天数', { exact: true })).toHaveValue('7');
    await page.getByLabel('文本保留天数').fill('30');
    await page.getByRole('button', { name: '预览清理' }).click();
    await expect(page.getByRole('dialog')).toContainText('3 条文本');
    await page.getByRole('button', { name: '确认清理并保存策略' }).click();
    await expect(page.getByText('没有符合筛选的记录', { exact: true })).toBeVisible();
  }, { daemon: false }, running => running);
});

test('history-retention reopens a failed recording in the daemon local job queue without adding a dictation', async ({ page, request }) => {
  await withConsole(async (url: string) => {
    await page.goto(`${url}#history`);
    await page.getByRole('button', { name: '失败录音', exact: true }).click();
    await expect(page.getByRole('dialog')).toContainText('connection lost');
    const posted = page.waitForResponse(response => response.url().endsWith(`/recovery/${id}/reopen`));
    await page.getByRole('button', { name: '加入本地转写队列' }).click();
    const result = await posted;
    expect(result.ok()).toBe(true);
    const job = await result.json();
    const headers = { 'X-Voiceless-Token': (await page.locator('meta[name="voiceless-token"]').getAttribute('content'))! };
    expect(await (await request.get(`${url}api/jobs/${job.id}`, { headers })).json()).toMatchObject({ transport: 'local', input: { name: `${id}.wav` } });
    expect(await (await request.get(`${url}api/records`, { headers })).json()).toMatchObject({ records: [] });
  }, { daemon: true, async prepare(root) {
    const dir = join(root, 'data', 'voiceless', 'recovery');
    await mkdir(dir, { recursive: true, mode: 0o700 });
    await writeFile(join(dir, `${id}.wav`), wave(), { mode: 0o600 });
    await writeFile(join(dir, `${id}.json`), JSON.stringify({ version: 1, id, saved_at: new Date().toISOString(), reason: 'connection lost', backend: 'local-offline', mode: 'quick', app_id: null, duration_ms: 1, bytes: 48 }), { mode: 0o600 });
  } });
});
