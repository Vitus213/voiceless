import { api, btn, confirmDialog, group, h, humanError, live, note, pageHead, row } from './console.js';
import { t } from './i18n';

type Source = { node: string; label: string };
type Track = { kind: string; label: string; status: string; received_bytes: number; job_id: string | null };
type Meeting = { id: string; started_at: string; state: keyof typeof states; data_location: string; tracks: Track[]; actions: string[]; failure: null | { kind: string; node?: string; reason?: string; remedy?: string } };
const states = { recording: t('录制中'), stopping: t('正在停止'), completed: t('已完成'), interrupted: t('录制中断'), recovered: t('已恢复') };

export function MeetingsPage() {
  let mounted = false;
  let runtime = '';
  let busy = false;
  let recording = false;
  let generation = 0;
  let timer: number | undefined;
  let unsubscribe: (() => void) | undefined;
  const mic = h('select', { class: 'inpt', 'aria-label': t('麦克风') }) as HTMLSelectElement;
  const system = h('select', { class: 'inpt', 'aria-label': t('系统音频') }) as HTMLSelectElement;
  const errorBox = h('div', { role: 'status' });
  const devicesError = h('div', { role: 'status' });
  const list = h('div', { class: 'stack' });
  const locationText = h('p', { class: 'lab' });
  const start = btn('mic', t('开始会议录制'), { cls: 'primary', disabled: true, onclick: () => void act('start') }) as HTMLButtonElement;
  function controls() { start.disabled = busy || recording || !runtime || !mic.value; }
  mic.onchange = controls;

  async function devices() {
    try {
      const sources: { microphones: Source[]; system_outputs: Source[] } = await api('/api/meetings/sources', { quiet: true });
      if (!mounted) return;
      const options = (items: Source[]) => items.map(s => h('option', { value: s.node }, s.label));
      mic.replaceChildren(h('option', { value: '' }, t('选择麦克风')), ...options(sources.microphones));
      system.replaceChildren(h('option', { value: '' }, t('不录制系统音频')), ...options(sources.system_outputs));
      devicesError.replaceChildren();
    } catch (e) {
      mic.replaceChildren(); system.replaceChildren();
      devicesError.replaceChildren(note('circle-alert', humanError(e), 'err'));
    }
    controls();
  }

  async function act(command: string, id?: string) {
    if (busy || !runtime) return;
    busy = true; controls();
    try {
      if (command === 'discard' && !await confirmDialog(t('丢弃此会议已保存的音频？'), { danger: true })) return;
      const path = id ? `${encodeURIComponent(id)}/${command}` : command;
      const body = command === 'start' ? { microphone: mic.value, system: system.value || null, runtime_id: runtime } : { runtime_id: runtime };
      await api(`/api/meetings/${path}`, { method: 'POST', body, quiet: true });
      errorBox.replaceChildren();
    } catch (e) { errorBox.replaceChildren(note('circle-alert', humanError(e), 'err')); }
    finally { busy = false; if (mounted) await refresh(); controls(); }
  }

  function card(m: Meeting) {
    const action = (cmd: string, label: string) => btn('chevron-right', label, { onclick: () => void act(cmd, m.id) });
    const review = new URL(location.href);
    // Review links use ?meeting=<id>#meeting-review so the fragment remains a plain route name.
    review.searchParams.set('meeting', m.id); review.hash = 'meeting-review';
    const ready = m.state === 'completed' || m.state === 'recovered';
    return group('mic', `${m.started_at} · ${states[m.state]}`, null,
      ...m.tracks.map(track => row(track.kind === 'system' ? t('系统音频') : t('麦克风'),
        h('span', {}, `${track.label} · ${(track.received_bytes / 32000).toFixed(1)} s`))),
      h('p', { class: 'lab' }, m.data_location),
      m.failure && note('circle-alert', [m.failure.kind, m.failure.node, m.failure.reason, m.failure.remedy].filter(Boolean).join(' · '), 'err'),
      m.state === 'recording' && action('stop', t('停止录制')),
      m.actions.includes('recover') && action('recover', t('恢复已保存音频')),
      m.actions.includes('discard') && action('discard', t('丢弃会议')),
      ready && m.tracks.some(track => track.status === 'finalized' && !track.job_id) && action('retry', t('重试加入本地转写队列')),
      ready && h('a', { href: review.href }, t('校对会议')));
  }

  async function refresh() {
    const request = ++generation;
    try {
      const result: { runtime_id: string; data_location: string; meetings: Meeting[] } = await api('/api/meetings', { quiet: true });
      if (!mounted || request !== generation) return;
      runtime = result.runtime_id;
      recording = result.meetings.some(m => m.state === 'recording' || m.state === 'stopping');
      locationText.textContent = result.data_location;
      list.replaceChildren(...result.meetings.reverse().map(card));
    } catch (e) {
      if (!mounted || request !== generation) return;
      runtime = ''; errorBox.replaceChildren(note('circle-alert', humanError(e), 'err'));
    }
    controls();
  }
  return {
    async mount(view: HTMLElement) {
      mounted = true;
      view.append(pageHead(t('会议录制')), group('mic', t('采集范围'), null,
        h('p', { class: 'lab' }, t('Linux PipeWire 录制需要 pw-dump、pw-record 和所选来源的访问权限。停止后在本地转写。')),
        row(t('麦克风'), mic), row(t('系统音频'), system), btn('refresh-cw', t('刷新来源'), { onclick: () => void devices() }),
        devicesError, start, locationText, errorBox), list);
      unsubscribe = live.subscribe((event: { type: string; error?: string }) => {
        if (event.type === 'meeting' || event.type === 'snapshot') {
          if (event.error) errorBox.replaceChildren(note('circle-alert', event.error, 'err'));
          void refresh();
        }
      });
      await Promise.all([devices(), refresh()]);
      if (mounted) timer = window.setInterval(() => { if (recording || !live.connected) void refresh(); }, 1200);
    },
    unmount() { mounted = false; unsubscribe?.(); window.clearInterval(timer); },
  };
}
