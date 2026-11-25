import type { Page } from '@playwright/test';
import { Buffer } from 'node:buffer';
import { execFileSync } from 'node:child_process';
import { constants, cp, mkdir, mkdtemp, readFile, readlink, rm, symlink } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { basename, join } from 'node:path';
import { expect, test, withConsole } from './console';

const store = process.env.VOICELESS_E2E_MODEL_STORE;
const senseVoice = 'sense-voice-int8';
const pcm = new URL('./fixtures/jfk-16k-mono-s16le.pcm', import.meta.url);

type Segment = { start: number; end: number; text: string };
type Job = { id: string; status: string; language: string; transport: string; input: { name: string };
  progress: { segments: number }; error: { code: string } | null; segments: Segment[] };

async function seed(data: string) {
  const src = join(store!, 'voiceless', 'models');
  const dest = join(data, 'voiceless', 'models');
  for (const id of [senseVoice, 'silero-vad']) {
    const link = await readlink(join(src, id, 'current'));
    const version = basename(link);
    expect(link).toBe(`../versions/${id}/${version}`);
    await mkdir(join(dest, 'versions', id), { recursive: true });
    await cp(join(src, 'versions', id, version), join(dest, 'versions', id, version), {
      recursive: true, verbatimSymlinks: true, mode: constants.COPYFILE_FICLONE,
    });
    await mkdir(join(dest, id));
    await symlink(link, join(dest, id, 'current'));
  }
}

async function speech(repetitions = 1) {
  const sample = await readFile(pcm);
  const block = Buffer.concat([sample, Buffer.alloc(32_000)]);
  const body = Buffer.concat(Array<Buffer>(repetitions).fill(block));
  const header = Buffer.alloc(44);
  header.write('RIFF'); header.writeUInt32LE(36 + body.length, 4); header.write('WAVEfmt ', 8);
  header.writeUInt32LE(16, 16); header.writeUInt16LE(1, 20); header.writeUInt16LE(1, 22);
  header.writeUInt32LE(16_000, 24); header.writeUInt32LE(32_000, 28);
  header.writeUInt16LE(2, 32); header.writeUInt16LE(16, 34);
  header.write('data', 36); header.writeUInt32LE(body.length, 40);
  return { name: 'jfk.wav', mimeType: 'audio/wav', buffer: Buffer.concat([header, body]) };
}

async function api<T>(page: Page, path: string): Promise<T> {
  return page.evaluate(async (path) => {
    const token = document.querySelector<HTMLMetaElement>('meta[name=voiceless-token]')!.content;
    const response = await fetch(path, { headers: { 'X-Voiceless-Token': token } });
    if (!response.ok) throw Error(`${path} returned ${response.status}`);
    return response.json();
  }, path);
}

const card = (page: Page, name: string) => page.locator('section.group').filter({ has: page.getByRole('heading', { name, exact: true }) }).first();
const add = (page: Page) => page.getByRole('button', { name: '加入转写队列' });
const picker = (page: Page) => page.getByLabel('选择音视频文件');

async function exported(page: Page, format: string) {
  const pending = page.waitForEvent('download');
  await page.getByRole('button', { name: format, exact: true }).click();
  const file = await pending;
  expect(file.suggestedFilename()).toBe(`jfk.${format.toLowerCase()}`);
  return readFile(await file.path(), 'utf8');
}

function clock(samples: number, separator: string) {
  const ms = Math.floor(samples / 16);
  return `${new Date(ms).toISOString().slice(11, 19)}${separator}${String(ms % 1000).padStart(3, '0')}`;
}

test('media-import cloud consent is independent of local model availability', async ({ page, daemonUrl }) => {
  await page.goto(`${daemonUrl}#jobs`);
  await picker(page).setInputFiles(await speech());
  await expect(page.getByLabel('本地模型', { exact: true })).toHaveValue('');
  await expect(add(page)).toBeDisabled();
  await page.getByLabel('转写方式').selectOption('cloud');
  const consent = page.getByRole('checkbox', { name: /允许将这些文件/ });
  await expect(add(page)).toBeDisabled();
  await consent.check();
  await expect(add(page)).toBeEnabled();
  await consent.uncheck();
  const denied = await page.evaluate(async () => {
    const token = document.querySelector<HTMLMetaElement>('meta[name=voiceless-token]')!.content;
    const response = await fetch('/api/jobs?name=denied.wav&model=cloud&transport=cloud', {
      method: 'POST', headers: { 'X-Voiceless-Token': token }, body: new Uint8Array([0]),
    });
    if (!response.ok) throw Error(`submission returned ${response.status}`);
    return response.json() as Promise<{ id: string }>;
  });
  expect(await api<Job>(page, `/api/jobs/${denied.id}`)).toMatchObject({
    status: 'failed', transport: 'cloud', error: { code: 'cloud-not-authorized' },
  });
  await expect(card(page, 'denied.wav')).toContainText('转写失败');
});

test.describe('real media jobs', () => {
  test.skip(!store, 'requires VOICELESS_E2E_MODEL_STORE with SenseVoice and Silero VAD');
  test.setTimeout(300_000);

  test('media-import recognizes local speech, isolates a broken file, and exports the actual media timeline', async ({ page }) => {
    await withConsole<string>(async (url) => {
      await page.goto(`${url}#jobs`);
      const models = await api<{ id: string; kind: string; installed: boolean; languages: string[] }[]>(page, '/api/models');
      expect(models.find(({ id }) => id === senseVoice)).toMatchObject({ kind: 'sherpa', installed: true, languages: expect.arrayContaining(['en']) });
      const file = await speech();
      const files = [{ name: 'broken.wav', mimeType: 'audio/wav', buffer: Buffer.from('RIFF0000WAVE') }, file];
      const capabilities = await api<{ additional_formats: string | null }>(page, '/api/jobs');
      if (capabilities.additional_formats) {
        files.push({ name: 'jfk.flac', mimeType: 'audio/flac', buffer: execFileSync('ffmpeg', [
          '-loglevel', 'error', '-i', 'pipe:0', '-f', 'flac', 'pipe:1',
        ], { input: file.buffer }) });
      }
      await picker(page).setInputFiles(files);
      const transport = page.getByLabel('转写方式');
      await transport.selectOption('cloud');
      await expect(add(page)).toBeDisabled();
      const consent = page.getByRole('checkbox', { name: /允许将这些文件/ });
      await consent.check();
      await expect(add(page)).toBeEnabled();
      await transport.selectOption('local');
      await transport.selectOption('cloud');
      await expect(consent).not.toBeChecked();
      await transport.selectOption('local');
      await page.getByLabel('本地模型', { exact: true }).selectOption(senseVoice);
      await page.getByLabel('语言', { exact: true }).selectOption('en');
      await add(page).click();
      await expect(card(page, 'broken.wav')).toContainText('转写失败');
      for (const valid of files.slice(1)) await expect(card(page, valid.name)).toContainText('已完成', { timeout: 120_000 });
      const { jobs } = await api<{ jobs: Job[] }>(page, '/api/jobs');
      expect(jobs.map((job) => job.input.name)).toEqual(files.map((item) => item.name));
      const job = await api<Job>(page, `/api/jobs/${jobs[1].id}`);
      expect(job).toMatchObject({ status: 'completed', language: 'en', transport: 'local' });
      const texts = job.segments.map((s) => s.text);
      expect(texts.join(' ')).toMatch(/country/i);
      await card(page, file.name).getByRole('button', { name: '查看结果' }).click();
      expect(await exported(page, 'TXT')).toBe(`${texts.join('\n')}\n`);
      const json = JSON.parse(await exported(page, 'JSON'));
      expect(json.job).toMatchObject({ model: senseVoice, language: 'en', input: file.name });
      expect(json.segments).toEqual(job.segments.map((s, i) => ({ index: i + 1, start: s.start / 16_000, end: s.end / 16_000, text: s.text })));
      const srt = await exported(page, 'SRT');
      const vtt = await exported(page, 'VTT');
      expect(vtt).toMatch(/^WEBVTT\n/);
      for (const s of job.segments) {
        expect(s.start).toBeGreaterThanOrEqual(0);
        expect(s.end).toBeGreaterThan(s.start);
        expect(s.end * 2).toBeLessThanOrEqual(file.buffer.length - 44);
        for (const [content, sep] of [[srt, ','], [vtt, '.']]) {
          expect(content).toContain(`${clock(s.start, sep)} --> ${clock(s.end, sep)}\n${s.text}`);
        }
      }
    }, { daemon: true, prepare: (root) => seed(join(root, 'data')) });
  });

  test('job-recovery keeps confirmed speech across daemon restart, source loss and reselection', async ({ page }) => {
    const data = await mkdtemp(join(tmpdir(), 'voiceless-media-restart-'));
    const launch = { daemon: true, env: { XDG_DATA_HOME: data } };
    try {
      await seed(data);
      const file = await speech(8);
      let id = '';
      let prefix: Segment[] = [];
      await withConsole<string>(async (url) => {
        await page.goto(`${url}#jobs`);
        await page.getByLabel('本地模型', { exact: true }).selectOption(senseVoice);
        await picker(page).setInputFiles(file);
        await add(page).click();
        await expect.poll(async () => {
          const { jobs } = await api<{ jobs: Job[] }>(page, '/api/jobs');
          id = jobs[0]?.id ?? '';
          return jobs[0]?.progress.segments ?? 0;
        }, { timeout: 120_000 }).toBeGreaterThan(0);
        await card(page, file.name).getByRole('button', { name: '暂停' }).click();
        await expect(card(page, file.name)).toContainText('已暂停');
        prefix = (await api<Job>(page, `/api/jobs/${id}`)).segments;
      }, launch);
      // Remove only this test daemon's owned media while the daemon is stopped.
      await rm(join(data, 'voiceless', 'jobs', id, 'media'));
      await withConsole<string>(async (url) => {
        await page.goto(`${url}#jobs`);
        const entry = card(page, file.name);
        await expect(entry).toContainText('已暂停');
        expect((await api<Job>(page, `/api/jobs/${id}`)).segments).toEqual(prefix);
        await entry.getByRole('button', { name: '继续' }).click();
        await expect(entry).toContainText('转写失败');
        await entry.getByRole('button', { name: '重试' }).click();
        await expect.poll(async () => (await api<Job>(page, `/api/jobs/${id}`)).error?.code).toBe('source-unavailable');
        const replace = entry.getByLabel('重新选择源文件');
        await replace.setInputFiles(await speech(1));
        await expect.poll(async () => (await api<Job>(page, `/api/jobs/${id}`)).error?.code).toBe('source-mismatch');
        await replace.setInputFiles(file);
        await expect(entry).toContainText('已完成', { timeout: 180_000 });
        const final = await api<Job>(page, `/api/jobs/${id}`);
        expect(final.segments.slice(0, prefix.length)).toEqual(prefix);
        expect(final.segments.length).toBeGreaterThan(prefix.length);
        for (let i = 1; i < final.segments.length; i++) expect(final.segments[i].start).toBeGreaterThanOrEqual(final.segments[i - 1].end);
        expect(final.segments.at(-1)!.end).toBeGreaterThan((file.buffer.length - 44) / 2 - 64_000);
      }, launch);
    } finally {
      await rm(data, { recursive: true, force: true });
    }
  });
});
