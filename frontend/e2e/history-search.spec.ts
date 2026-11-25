// The history search over a real console. The library is what an upgraded
// installation holds: rows the release before the search left (in its
// schema, the same month and day in two years, nine rows in one second,
// more meetings than one page holds) and rows the current release writes
// (an application, a model, a recording). The history page searches,
// filters, pages and exports them; the authenticated loopback API pages
// the same cursors; a dictation lands between two pages.
import { mkdirSync } from 'node:fs';
import { readFile, writeFile } from 'node:fs/promises';
import { join } from 'node:path';
import { DatabaseSync } from 'node:sqlite';
import { expect, test as base, withConsole } from './console';

type Row = {
  id: number;
  createdAt: string;
  text: string;
  mode?: string;
  model?: string | null;
  audio?: string | null;
  app?: string | null;
};

type History = { url: string; store: string };

/** One page as `/api/records` answers it. */
type Page = { records: { id: string }[]; next_cursor: string | null };

/** More meetings than one page of the history holds. */
const FILLER = 110;
/** The nine rows that share `2026-01-01T00:00:00Z`. */
const TIED = Array.from({ length: 9 }, (_, index) => 10 + index);
/** Every row with "会议" in it: three the earlier release left, nine in one second, the filler, three of the current release. */
const MEETINGS = 6 + TIED.length + FILLER;

/** A row of the fixture as a UUID-shaped id: the console only serves the audio of such ids. */
const idOf = (n: number) => `00000000-0000-4000-8000-${String(n).padStart(12, '0')}`;

/** The table as the release before the search left it: no application was recorded. */
const PREVIOUS_SCHEMA = `CREATE TABLE recognition_history (
  id TEXT PRIMARY KEY,
  created_at TEXT NOT NULL,
  duration_seconds REAL,
  raw_text TEXT NOT NULL,
  processing_mode TEXT,
  processed_text TEXT,
  final_text TEXT NOT NULL,
  status TEXT NOT NULL,
  character_count INTEGER,
  asr_provider TEXT,
  asr_model TEXT,
  audio_path TEXT
);
PRAGMA user_version = 2;`;

/** A 16 kHz mono PCM16 WAV of 256 silent samples: enough for the player and the waveform. */
function wav(): Buffer {
  const header = Buffer.alloc(44);
  header.write('RIFF', 0);
  header.writeUInt32LE(44, 4);
  header.write('WAVE', 8);
  header.write('fmt ', 12);
  header.writeUInt32LE(16, 16);
  header.writeUInt16LE(1, 20);
  header.writeUInt16LE(1, 22);
  header.writeUInt32LE(16_000, 24);
  header.writeUInt32LE(32_000, 28);
  header.writeUInt16LE(2, 32);
  header.writeUInt16LE(16, 34);
  header.write('data', 36);
  header.writeUInt32LE(512, 40);
  return Buffer.concat([header, Buffer.alloc(512)]);
}

/** Leave behind what an upgraded installation holds, in the previous schema. */
function seed(store: string): void {
  mkdirSync(join(store, '..'), { recursive: true });
  const db = new DatabaseSync(store);
  db.exec(PREVIOUS_SCHEMA);
  const insert = db.prepare(`INSERT INTO recognition_history
    (id, created_at, duration_seconds, raw_text, processing_mode, processed_text, final_text,
     status, character_count, asr_provider, asr_model, audio_path)
    VALUES (?, ?, NULL, ?, 'quick', NULL, ?, 'completed', 0, 'cloud-realtime', NULL, NULL)`);
  const rows: Row[] = [
    { id: 1, createdAt: '2026-03-14T02:00:00.123456Z', text: '张三丰的 PostgreSQL 会议' },
    { id: 2, createdAt: '2025-03-14T02:00:00.123456Z', text: '张三丰的 PostgreSQL 会议' },
    // 01:00 on 15 March 2025 by the clock the tests give the page; 14 March in UTC.
    { id: 3, createdAt: '2025-03-14T17:00:00Z', text: '张三丰的深夜会议' },
    { id: 4, createdAt: '2026-06-01T00:00:00Z', text: '别的事情' },
    ...TIED.map((id) => ({ id, createdAt: '2026-01-01T00:00:00Z', text: '同一秒的会议' })),
    ...Array.from({ length: FILLER }, (_, n) => ({
      id: 1000 + n,
      createdAt: `2026-02-01T${String(Math.floor(n / 60)).padStart(2, '0')}:${String(n % 60).padStart(2, '0')}:00Z`,
      text: `第 ${n + 1} 次会议纪要`,
    })),
  ];
  db.exec('BEGIN');
  for (const row of rows) insert.run(idOf(row.id), row.createdAt, row.text, row.text);
  db.exec('COMMIT');
  db.close();
}

/** Rows a session that finished in the background leaves, written into the running console's database. */
function append(store: string, rows: Row[]): void {
  const db = new DatabaseSync(store);
  const insert = db.prepare(`INSERT INTO recognition_history
    (id, created_at, duration_seconds, raw_text, processing_mode, processed_text, final_text,
     status, character_count, asr_provider, asr_model, audio_path, app_id)
    VALUES (?, ?, NULL, ?, ?, NULL, ?, 'completed', 0, 'local-offline', ?, ?, ?)`);
  db.exec('BEGIN');
  for (const row of rows) {
    insert.run(idOf(row.id), row.createdAt, row.text, row.mode ?? 'quick', row.text, row.model ?? null, row.audio ?? null, row.app ?? null);
  }
  db.exec('COMMIT');
  db.close();
}

/** One console over the upgraded library: `store` is its history database, to append to while it runs. */
const test = base.extend<{ history: History }, object>({
  history: async ({}, use) => withConsole(
    use,
    {
      daemon: false,
      async prepare(root) {
        await writeFile(join(root, 'sample.wav'), wav());
        seed(join(root, 'data', 'voiceless', 'history.sqlite3'));
      },
    },
    // The console has brought the file to its schema by the time it answers: what the current release records goes in on top.
    (running) => {
      const store = join(running.root, 'data', 'voiceless', 'history.sqlite3');
      append(store, [
        { id: 2001, createdAt: '2026-07-01T00:00:00Z', text: 'kitty 里的会议', mode: 'prompt-optimize', model: 'qwen3-asr-0.6b-gguf', app: 'kitty' },
        { id: 2002, createdAt: '2026-07-02T00:00:00Z', text: 'Firefox 的会议', audio: join(running.root, 'sample.wav'), app: 'org.mozilla.firefox' },
        { id: 2003, createdAt: '2026-07-03T00:00:00Z', text: '没有应用的会议' },
      ]);
      return { url: running.url, store };
    },
  ),
});

// The days the page shows and the ones it sends are the page's own.
test.use({ timezoneId: 'Asia/Shanghai' });

test('history-search finds matches beyond the first page, keeps years apart and pages on while a dictation lands', async ({ page, history }) => {
  await page.goto(`${history.url}#history`);
  const rows = page.locator('article.rec');
  const search = page.getByRole('searchbox', { name: '搜索记录' });
  await expect(rows).toHaveCount(100);
  await page.getByRole('button', { name: '更早的记录', exact: true }).click();
  await expect(rows).toHaveCount(126);
  await expect(page.getByText('已经到最早的一条', { exact: true })).toBeVisible();

  // An English API name in either letter case, and both years it was said in.
  await search.fill('POSTGRESQL');
  await expect(rows).toHaveCount(2);
  await expect(page.locator('.rec .txt')).toHaveText(['张三丰的 PostgreSQL 会议', '张三丰的 PostgreSQL 会议']);
  // The same month and day in two years are two days.
  await expect(page.locator('.day-head')).toHaveText(['2026年3月14日周六', '2025年3月14日周五']);
  // What the earlier release never recorded says so.
  await expect(page.getByText('应用未记录', { exact: true })).toHaveCount(2);

  // A Chinese word with more matches than one page holds.
  await search.fill('会议');
  await expect(rows).toHaveCount(100);
  // A dictation finishes in the background while the reader is on the first page.
  append(history.store, [{ id: 3001, createdAt: '2099-01-01T00:00:00Z', text: '后台完成的会议', app: 'kitty' }]);
  await page.getByRole('button', { name: '更早的记录', exact: true }).click();
  await expect(rows).toHaveCount(MEETINGS);
  await expect(page.getByText('后台完成的会议', { exact: true })).toHaveCount(0);
  expect(await rows.evaluateAll((els) => new Set(els.map((el) => el.dataset.id)).size)).toBe(MEETINGS);
  await expect(page.getByText('没有更多符合条件的记录', { exact: true })).toBeVisible();
  // Running the search again lists what landed first.
  await search.press('Enter');
  await expect(rows).toHaveCount(100);
  await expect(rows.first().locator('.txt')).toHaveText('后台完成的会议');
  await page.getByRole('button', { name: '更早的记录', exact: true }).click();
  await expect(rows).toHaveCount(MEETINGS + 1);
});

test('history-search filters by the local day, application, mode, model and recording, and exports what it shows', async ({ page, history }) => {
  await page.goto(`${history.url}#history`);
  const rows = page.locator('article.rec');
  const search = page.getByRole('searchbox', { name: '搜索记录' });
  const filters = page.getByRole('search', { name: '筛选记录', exact: true });
  const from = page.getByLabel('开始日期', { exact: true });
  const to = page.getByLabel('结束日期', { exact: true });
  await expect(rows).toHaveCount(100);

  // 15 March 2025 by this clock: 01:00 that day, which is still 14 March in UTC.
  await search.fill('张三丰');
  await expect(rows).toHaveCount(3);
  await from.fill('2025-03-15');
  await to.fill('2025-03-15');
  await expect(rows).toHaveCount(1);
  await expect(rows.locator('.txt')).toHaveText(['张三丰的深夜会议']);
  await expect(page.locator('.day-head')).toHaveText(['2025年3月15日周六']);
  // Last year in full.
  await from.fill('2025-01-01');
  await to.fill('2025-12-31');
  await expect(rows).toHaveCount(2);
  // A range that ends before it begins changes nothing and says why.
  await from.fill('2026-01-01');
  await expect(page.getByText('结束日期不能早于开始日期', { exact: true })).toBeVisible();
  await expect(rows).toHaveCount(2);

  // Application, mode, model and recording: only what the current release recorded.
  await page.getByRole('button', { name: '清除筛选', exact: true }).click();
  await expect(rows).toHaveCount(100);
  await search.fill('会议');
  await search.press('Enter');
  await filters.getByRole('combobox', { name: '应用程序', exact: true }).selectOption('kitty');
  await expect(rows).toHaveCount(1);
  await expect(rows.locator('.txt')).toHaveText(['kitty 里的会议']);
  await expect(page.locator('.rec .chip.mono')).toHaveText(['kitty']);
  await filters.getByRole('combobox', { name: '模式', exact: true }).selectOption('prompt-optimize');
  await filters.getByRole('combobox', { name: '模型', exact: true }).selectOption('qwen3-asr-0.6b-gguf');
  await expect(rows).toHaveCount(1);
  await filters.getByRole('combobox', { name: '录音', exact: true }).selectOption('with');
  await expect(rows).toHaveCount(0);
  await expect(page.getByText('没有符合筛选的记录', { exact: true })).toBeVisible();

  // A saved recording: the one row whose WAV is there, with its player.
  await page.getByRole('button', { name: '清除筛选', exact: true }).click();
  await expect(rows).toHaveCount(100);
  await filters.getByRole('combobox', { name: '录音', exact: true }).selectOption('with');
  await expect(rows).toHaveCount(1);
  await expect(rows.locator('.txt')).toHaveText(['Firefox 的会议']);
  await expect(rows.getByRole('button', { name: '播放', exact: true })).toBeEnabled();

  // The download holds the rows the list shows, under the same search.
  await search.fill('postgresql');
  await expect(rows).toHaveCount(0);
  await filters.getByRole('combobox', { name: '录音', exact: true }).selectOption('without');
  await expect(rows).toHaveCount(2);
  const pending = page.waitForEvent('download');
  await page.getByRole('button', { name: '导出 CSV', exact: true }).click();
  const download = await pending;
  expect(download.suggestedFilename()).toBe('voiceless-history.csv');
  const lines = (await readFile((await download.path())!, 'utf8')).split('\n');
  expect(lines[0]).toBe('id,created_at,duration_seconds,raw_text,processing_mode,processed_text,final_text,status,character_count,asr_provider,asr_model,audio_path,app_id');
  expect(lines.slice(1, 3).map((line) => line.split(',')[0])).toEqual([idOf(1), idOf(2)]);
  await expect(page.getByText('已导出 2 条记录', { exact: true })).toBeVisible();
});

test('history-search pages exact cursors over the authenticated loopback API', async ({ page, request, history }) => {
  await page.goto(`${history.url}#history`);
  const headers = { 'X-Voiceless-Token': (await page.locator('meta[name="voiceless-token"]').getAttribute('content'))! };
  const get = async (path: string, parameters: Record<string, string> = {}, authed = true) => {
    const query = new URLSearchParams(parameters).toString();
    const response = await request.get(`${history.url}api${path}${query && `?${query}`}`, authed ? { headers } : {});
    return { status: response.status(), body: await response.json() };
  };
  const idsOf = (body: Page) => body.records.map((record) => record.id);

  // A call from before the search: every row, newest first.
  const first = await get('/records', { limit: '4' });
  expect(first.status).toBe(200);
  expect(idsOf(first.body)).toEqual([2003, 2002, 2001, 4].map(idOf));

  // A cursor the earlier release wrote: the array of a row's time and id.
  const writtenEarlier = Buffer.from(JSON.stringify(['2026-06-01T00:00:00Z', idOf(4)])).toString('base64url');
  const next = await get('/records', { limit: '2', cursor: writtenEarlier });
  // Below "别的事情": the earlier year's API-name row, then the newest of the filler meetings.
  expect(idsOf(next.body)).toEqual([1, 1000 + FILLER - 1].map(idOf));

  // Nine rows in one second, four at a time: each once, in id order.
  const seen: string[] = [];
  let cursor: string | null = null;
  for (const expected of [4, 4, 1]) {
    const ties: Page = (await get('/records', { q: '同一秒', limit: '4', ...(cursor && { cursor }) })).body;
    expect(ties.records).toHaveLength(expected);
    seen.push(...idsOf(ties));
    cursor = ties.next_cursor;
  }
  expect(cursor).toBeNull();
  expect(seen).toEqual(TIED.toReversed().map(idOf));

  // A cursor belongs to its search; a value that does not read is 400; without the token nothing is read.
  const issued = (await get('/records', { q: '同一秒', limit: '4' })).body;
  const reused = await get('/records', { q: '别的事情', cursor: issued.next_cursor });
  expect(reused.status).toBe(500);
  expect(reused.body).toEqual({ error: 'history cursor invalid' });
  expect((await get('/records', { from: 'yesterday' })).status).toBe(400);
  expect((await get('/records', {}, false)).status).toBe(403);
  expect((await get('/records/export', { q: 'x' }, false)).status).toBe(403);

  // What the menus offer, and an export of the same rows the pages list.
  const facets = await get('/records/facets');
  expect(facets.body).toEqual({
    modes: [{ value: 'quick', records: 125 }, { value: 'prompt-optimize', records: 1 }],
    apps: [{ value: 'kitty', records: 1 }, { value: 'org.mozilla.firefox', records: 1 }],
    models: [{ value: 'qwen3-asr-0.6b-gguf', records: 1 }],
  });
  const listed: string[] = [];
  cursor = null;
  for (;;) {
    const body: Page = (await get('/records', { q: '会议', limit: '50', ...(cursor && { cursor }) })).body;
    listed.push(...idsOf(body));
    cursor = body.next_cursor;
    if (!cursor) break;
  }
  const exported = (await get('/records/export', { q: '会议', limit: '1' })).body;
  expect(exported.exported).toBe(MEETINGS);
  const csvIds = exported.csv.split('\n').slice(1, -1).map((line: string) => line.split(',')[0]);
  expect(csvIds).toEqual(listed);
});

test('vocabulary saves checked history corrections only to their chosen project', async ({ page, request, history }) => {
  await page.goto(`${history.url}#history`);
  const card = page.locator('article.rec').filter({ hasText: '别的事情' });
  await card.getByRole('button', { name: '展开记录：别的事情', exact: true }).click();
  await card.getByRole('button', { name: '纠错' }).click();
  const dialog = page.getByRole('dialog');
  await dialog.getByRole('textbox', { name: '正确的文字' }).fill('别的事项');
  await dialog.getByRole('textbox', { name: '词库项目' }).fill('review');
  await dialog.getByRole('button', { name: '生成建议' }).click();
  await expect(dialog.getByText('勾选要加入的条目：')).toBeVisible();
  await dialog.getByRole('button', { name: '加入词库' }).click();
  await expect(page.getByText('已加入词库（1 条）', { exact: true })).toBeVisible();
  const token = await page.locator('meta[name="voiceless-token"]').getAttribute('content');
  const preview = async (project: string) => {
    const response = await request.post(`${history.url}api/text/preview`, {
      headers: { 'X-Voiceless-Token': token! },
      data: { text: '别的事情', project },
    });
    expect(response.ok()).toBe(true);
    return (await response.json()).text;
  };
  expect(await preview('review')).toBe('别的事项');
  expect(await preview('other')).toBe('别的事情');
});
