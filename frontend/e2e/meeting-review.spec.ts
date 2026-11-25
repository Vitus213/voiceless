import { createServer } from 'node:http';
import { mkdir, readFile, writeFile } from 'node:fs/promises';
import { join } from 'node:path';
import { once } from 'node:events';
import { test, expect, withConsole } from './console';

// A real console over an isolated capture journal and a previously saved
// review. The provider binds loopback only and receives no credentials.
test('meeting-review corrects saved paragraphs and sends only the explicitly selected excerpt', async ({ page }) => {
  const requests: string[] = [];
  const service = createServer(async (req, res) => {
    const chunks = [];
    for await (const chunk of req) chunks.push(chunk);
    requests.push(Buffer.concat(chunks).toString());
    res.writeHead(200, { 'Content-Type': 'application/json' });
    res.end(JSON.stringify({ message: { content: 'Reviewed remote result' } }));
  });
  const ready = once(service, 'listening');
  service.listen(0, '127.0.0.1');
  await ready;
  try {
    const address = service.address();
    if (!address || typeof address === 'string') throw Error('Missing loopback provider');
    const endpoint = `http://127.0.0.1:${address.port}`;
    const id = 'abcdef01-2345-6789-abcd-ef0123456789';
    await withConsole<string>(async url => {
      await page.addInitScript(() => localStorage.setItem('voiceless.locale', JSON.stringify('zh')));
      await page.goto(`${url}#meeting-review`);
      const title = page.getByLabel('会议标题');
      await expect(title).toHaveValue('Draft');
      const audio = page.locator('audio');
      await page.getByRole('button', { name: '麦克风 0:01.00–0:02.00' }).click();
      await expect.poll(() => audio.evaluate(el => (el as HTMLAudioElement).currentTime)).toBeGreaterThanOrEqual(1);
      await title.fill('Private project title');
      await page.getByLabel('段落文本').nth(1).fill('Approved excerpt');
      await page.getByRole('textbox', { name: '说话人 · 人工标注' }).nth(1).fill('Robin');
      await page.getByRole('button', { name: '保存校对' }).click();
      await expect(page.getByRole('button', { name: '保存校对' })).toBeDisabled();
      const select = page.getByLabel('选择此段用于整理').nth(1);
      await select.check();
      await page.getByRole('button', { name: '从所选段落提取本地要点' }).click();
      await expect(page.getByLabel('校对要点')).toHaveValue('• Approved excerpt');
      const code = await page.evaluate(async ({ id, endpoint }) => {
        const path = `/api/meetings/${id}/review`;
        const headers = { 'X-Voiceless-Token': document.querySelector<HTMLMetaElement>('meta[name="voiceless-token"]')!.content, 'Content-Type': 'application/json' };
        const { review } = await (await fetch(path, { headers })).json();
        return (await fetch(path, { method: 'POST', headers, body: JSON.stringify({ revision: review.revision, action: 'remote', segments: ['microphone-1'], consent: false, destination: { provider: 'ollama', endpoint, model: 'review' } }) })).status;
      }, { id, endpoint });
      expect(code).toBe(403);
      expect(requests).toEqual([]);
      await select.check();
      page.once('dialog', d => d.dismiss());
      await page.getByRole('button', { name: '发送所选段落并生成要点' }).click();
      expect(requests).toEqual([]);
      page.once('dialog', async d => { expect(d.message()).toContain('Approved excerpt'); expect(d.message()).toContain(endpoint); await d.accept(); });
      await page.getByRole('button', { name: '发送所选段落并生成要点' }).click();
      const highlights = page.getByLabel('校对要点');
      await expect(highlights).toHaveValue('Reviewed remote result');
      expect(requests).toHaveLength(1);
      expect(requests[0]).toContain('Approved excerpt');
      for (const privateText of ['Private project title', 'Private paragraph', 'Before editing', 'Alice']) expect(requests[0]).not.toContain(privateText);
      await highlights.fill('Final highlights');
      await page.getByRole('button', { name: '保存校对' }).click();
      const download = page.waitForEvent('download');
      await page.getByRole('button', { name: '导出会议 JSON' }).click();
      const file = await download;
      const saved = JSON.parse(await readFile((await file.path())!, 'utf8'));
      expect(saved.highlights).toMatchObject({ text: 'Final highlights', generated: 'Reviewed remote result', segments: ['microphone-1'], destination: { endpoint } });
      expect(saved.paragraphs[1]).toMatchObject({ start: 16000, original: 'Before editing', text: 'Approved excerpt', speaker: 'Robin', speaker_source: 'manual' });
      const source = await audio.getAttribute('src');
      const slice = await page.request.get(new URL(source!, url).href, { headers: { Range: 'bytes=32044-32045' } });
      expect(slice.status()).toBe(206);
      expect(await slice.body()).toEqual(Buffer.from([0x34, 0x12]));
    }, { daemon: false, prepare: async root => {
      const dir = join(root, 'data', 'voiceless', 'meetings', id);
      const config = join(root, 'config', 'voiceless');
      await mkdir(dir, { recursive: true, mode: 0o700 });
      await mkdir(config, { recursive: true });
      await writeFile(join(config, 'config.toml'), `[processing]\nprovider = "ollama"\nmodel = "review"\nbase_url = "${endpoint}"\n`, { mode: 0o600 });
      const bytes = 64000;
      const wav = Buffer.alloc(44 + bytes);
      wav.write('RIFF'); wav.writeUInt32LE(bytes+36,4); wav.write('WAVEfmt ',8); wav.writeUInt32LE(16,16);
      wav.writeUInt16LE(1,20); wav.writeUInt16LE(1,22); wav.writeUInt32LE(16000,24); wav.writeUInt32LE(32000,28);
      wav.writeUInt16LE(2,32); wav.writeUInt16LE(16,34); wav.write('data',36); wav.writeUInt32LE(bytes,40); wav.writeUInt16LE(0x1234,32044);
      await writeFile(join(dir,'microphone.wav'), wav, {mode:0o600});
      await writeFile(join(dir,'meeting.json'), JSON.stringify({version:1,id,state:'completed',pid:1,started_at:'2026-01-01T00:00:00Z',tracks:[{kind:'microphone',node:'mic',label:'Mic',status:'finalized',committed_bytes:bytes}]}));
      await writeFile(join(dir,'review.json'), JSON.stringify({ meeting_id:id,revision:1,title:'Draft',sources:[{track:'microphone',job:'seed',model:'fixture',transport:'local'}],highlights:null,
        paragraphs:['Private paragraph','Before editing'].map((text,i) => ({id:`microphone-${i}`,track:'microphone',start:16000*i,end:16000*(i+1),original:text,text,speaker:'Alice',speaker_source:'manual'})) }), {mode:0o600});
    } });
  } finally {
    const closed = once(service, 'close');
    service.close();
    await closed;
  }
});
