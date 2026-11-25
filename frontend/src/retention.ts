import { api, btn, fmtBytes, h, humanError, openDialog, toast, withToken } from './console.js';
import { t } from './i18n';

const cancel = () => ({ label: t('取消'), value: '' });
const check = (checked = false) => h('input', { type: 'checkbox', checked }) as HTMLInputElement;
const label = (name: string, control: HTMLElement) => h('label', { class: 'inline' }, control, name);

export async function exportHistory(params: URLSearchParams, rows: HTMLElement[]) {
  const format = h('select', { class: 'inpt', 'aria-label': t('导出格式') }, ...['txt', 'md', 'csv', 'json'].map(value => h('option', { value }, value.toUpperCase()))) as HTMLSelectElement;
  const scope = h('select', { class: 'inpt', 'aria-label': t('导出范围') },
    h('option', { value: 'filter' }, t('全部筛选结果')), h('option', { value: 'ids' }, t('手动选择记录'))) as HTMLSelectElement;
  const audio = check();
  const items = rows.map(row => ({ id: row.dataset.id!, text: row.querySelector('.txt')?.textContent || '', box: check() }));
  const list = h('div', { hidden: true, class: 'stack' }, ...items.map(item => label(item.text, item.box)));
  scope.onchange = () => { list.hidden = scope.value !== 'ids'; };
  const action = await openDialog({ title: t('导出记录'), body: h('div', { class: 'stack' }, scope, format, label(t('包含录音文件'), audio), list),
    actions: [cancel(), { label: t('下载'), value: 'export' }], onOpen: undefined });
  if (action !== 'export') return;
  try {
    const ids = scope.value === 'ids' ? items.filter(item => item.box.checked).map(item => item.id) : undefined;
    if (ids && !ids.length) throw new Error(t('请选择要导出的记录'));
    const response = await fetch(withToken('/api/history/export'), { method: 'POST', headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ filter: Object.fromEntries(params), format: format.value, ids, include_audio: audio.checked }) });
    if (!response.ok) throw new Error((await response.json()).error || `HTTP ${response.status}`);
    const url = URL.createObjectURL(await response.blob());
    const name = response.headers.get('Content-Disposition')?.match(/filename="([^"]+)"/)?.[1];
    const link = h('a', { href: url, download: name || `history.${audio.checked ? 'zip' : format.value}` });
    link.click();
    setTimeout(() => URL.revokeObjectURL(url), 1000);
  } catch (error) { toast(humanError(error), 'err'); }
}

type Preview = { snapshot: string; text: { records: number; recordings: number; bytes: number; samples: { text: string }[] };
  audio: { recordings: number; bytes: number; missing: number }; recovery: { recordings: number; bytes: number } };

export async function retentionSettings(refresh: () => void) {
  try {
    const config = await api('/api/config', { quiet: true });
    const labels = { text_days: t('文本保留天数'), audio_days: t('录音保留天数'), failed_audio_days: t('失败录音保留天数'), failed_audio_mb: t('失败录音容量上限（MiB）') };
    const fields = Object.entries(labels).map(([key, name]) => ({ key, name, input: h('input', {
      type: 'number', required: true, min: 0, max: key === 'failed_audio_mb' ? 1048576 : 36500, step: 1, class: 'inpt', value: config.history[key],
    }) as HTMLInputElement }));
    const failed = check(config.history.save_failed_audio);
    const action = await openDialog({ title: t('保留策略'), body: h('div', { class: 'stack' },
      h('p', {}, t('0 表示不限期限或容量。')), fields.map(f => label(f.name, f.input)), label(t('保留失败录音'), failed)),
      actions: [cancel(), { label: t('预览清理'), value: 'preview' }], onOpen: undefined });
    if (action !== 'preview') return;
    const policy = Object.fromEntries(fields.map(f => [f.key, f.input.valueAsNumber]));
    const changes = Object.fromEntries(Object.entries({ ...policy, save_failed_audio: failed.checked })
      .filter(([key, value]) => value !== config.history[key]).map(([key, value]) => [`history.${key}`, value]));
    let preview: Preview = await api('/api/history/retention/preview', { method: 'POST', body: policy, quiet: true });
    for (;;) {
      const { text, audio, recovery } = preview;
      const answer = await openDialog({ title: t('确认清理'), body: h('div', { class: 'stack' },
        h('p', {}, t(`将删除 ${text.records} 条文本、${text.recordings + audio.recordings} 段录音和 ${recovery.recordings} 段失败录音`)),
        h('p', {}, fmtBytes(text.bytes + audio.bytes + recovery.bytes)), text.samples.map(s => h('p', {}, s.text))),
        actions: [cancel(), { label: t('确认清理并保存策略'), value: 'apply', class: 'danger' }], onOpen: undefined });
      if (answer !== 'apply') return;
      try {
        const { report } = await api('/api/history/retention/confirm', { method: 'POST', body: { snapshot: preview.snapshot }, quiet: true });
        refresh();
        toast(t(`已删除 ${report.records} 条文本、${report.recordings} 段录音和 ${report.failed_recordings} 段失败录音`), 'ok');
        if (report.failed) toast(t(`有 ${report.failed} 个文件未能删除`), 'warn');
        break;
      } catch (error) {
        const conflict = error as { status?: number; data?: { preview?: Preview } };
        if (conflict.status !== 409 || !conflict.data?.preview) throw error;
        preview = conflict.data.preview;
        toast(t('待删除的记录已变化，请重新确认'), 'warn');
      }
    }
    if (Object.keys(changes).length) await api('/api/config', { method: 'PUT', body: { revision: config.revision, fields: changes }, quiet: true });
    toast(t('记录设置已保存'), 'ok');
  } catch (error) { toast(humanError(error), 'err'); }
}

type FailedAudio = { id: string; reason: string; saved_at: string; bytes: number };
export async function recoveryList() {
  try {
    const { recordings, can_reopen }: { recordings: FailedAudio[]; can_reopen: boolean } = await api('/api/history/recovery', { quiet: true });
    const entries = recordings.map(record => {
      const reopen = btn('plus', t('加入本地转写队列'), { cls: 'ghost', disabled: !can_reopen, onclick: async () => {
        reopen.disabled = true;
        try {
          const { id } = await api(`/api/history/recovery/${encodeURIComponent(record.id)}/reopen`, { method: 'POST', quiet: true });
          reopen.textContent = t(`已加入转写队列：${id}`);
        } catch (error) { toast(humanError(error), 'err'); reopen.disabled = false; }
      } }) as HTMLButtonElement;
      return h('div', { class: 'stack' }, h('span', {}, `${record.saved_at} · ${record.reason} · ${fmtBytes(record.bytes)}`), reopen);
    });
    await openDialog({ title: t('失败录音'), body: h('div', { class: 'stack' }, entries.length ? entries : t('没有失败录音'),
      entries.length && !can_reopen ? t('启动守护进程后可重新转写') : null), actions: [{ label: t('关闭'), value: '' }], onOpen: undefined });
  } catch (error) { toast(humanError(error), 'err'); }
}
