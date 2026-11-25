import { expect, test } from './console';
import { fakeConsole } from './fake-api';

function transcript(text: string) {
  return { authoritative_text: text, confirmed_segments: [text], partial_text: '', is_final: false };
}

test('reconnect restores a recording and replaces it after the runtime restarts', async ({ page, consoleUrl }) => {
  let token = 'first-console-token';
  let status = {
    runtime_id: 'first-runtime', event_sequence: 5, state: 'recording', mode: 'quick',
    session_id: 1 as number | null,
    session: { id: 1, phase: 'recording', transcript: transcript('重连前的识别') } as object | null,
  };
  const api = await fakeConsole(consoleUrl, {
    token: () => token,
    snapshot: () => status,
    answer: (path) => path === '/api/status' ? { ...status, default_mode: 'quick', gpu: [] }
      : path === '/api/modes' ? [{ id: 'quick', name: '原文' }]
      : path === '/api/logs' ? { lines: [] } : { ok: true },
  });
  try {
    await page.goto(`${api.url}#live`);
    const recognized = page.locator('.ticker .confirmed');
    const stop = page.getByRole('button', { name: '停止并识别' });
    await expect(recognized).toHaveText('重连前的识别');
    await expect(stop).toBeEnabled();

    status = { ...status, event_sequence: 8, session: { id: 1, phase: 'recording', transcript: transcript('重连恢复的识别') } };
    api.drop();
    await expect(recognized).toHaveText('重连恢复的识别');
    await expect(stop).toBeEnabled();
    api.push({ type: 'transcript', runtime_id: 'first-runtime', event_sequence: 7, session_id: 1, transcript: transcript('旧的识别') });
    api.push({ type: 'phase', runtime_id: 'first-runtime', event_sequence: 9, session_id: 1, phase: 'processing' });
    await expect(page.getByText('正在处理文字', { exact: true })).toBeVisible();
    await expect(recognized).toHaveText('重连恢复的识别');

    token = 'second-console-token';
    status = { ...status, runtime_id: 'second-runtime', event_sequence: 1, state: 'idle', session_id: null, session: null };
    api.drop();
    const start = page.getByRole('button', { name: '开始录音', exact: true });
    await expect(start).toBeEnabled();
    await expect(recognized).toBeEmpty();
    await page.getByRole('combobox', { name: '模式', exact: true }).selectOption('quick');
    await start.click();
    await expect.poll(() => api.requests.filter((request) => request.path === '/api/session/start')).toEqual([
      { path: '/api/session/start', body: { runtime_id: 'second-runtime', mode: 'quick' }, token: 'second-console-token' },
    ]);
  } finally {
    await api.close();
  }
});
