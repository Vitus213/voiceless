import { t } from './i18n';
import { api, downloadText, group, h, humanError, pageHead, withToken } from './console.js';

type Track = 'microphone' | 'system';
type Destination = { provider: string; endpoint: string; model: string };
type Paragraph = { id: string; track: Track; start: number; end: number; original: string; text: string; speaker: string | null; speaker_source: 'unknown' | 'local-pitch' | 'manual' };
type Highlights = { text: string; generated: string; excerpts: string; segments: string[]; destination: Destination | null; source_revision: number };
type Review = { title: string; revision: number; paragraphs: Paragraph[]; highlights: Highlights | null; sources: { track: Track; job: string; model: string; transport: string }[] };
type View = { review: Review; jobs: { id: string; track: Track; model: string }[]; tracks: { kind: Track; label: string; path: string | null }[]; data_location: string; destination: Destination | null };
const trackName = (track: Track) => track === 'microphone' ? t('麦克风') : t('系统音频');
const stamp = (sample: number) => `${Math.floor(sample / 960000)}:${(sample / 16000 % 60).toFixed(2).padStart(5, '0')}`;
const button = (text: string, onclick: () => void) => h('button', { type: 'button', class: 'btn', onclick }, text) as HTMLButtonElement;
const field = (label: string, control: HTMLElement) => h('label', { class: 'stack' }, label, control);
const provider = (d: Destination) => `${d.provider} · ${d.endpoint} · ${d.model}`;

export function MeetingReviewPage() {
  const contents = h('div', { class: 'stack' });
  const status = h('div', { role: 'status' });
  const picker = h('select', { class: 'inpt' }) as HTMLSelectElement;
  const panel = h('fieldset', { class: 'stack' }, contents) as HTMLFieldSetElement;
  let dirty = false;
  let busy = false;
  let active = true;
  let selected = '';
  let audio: HTMLAudioElement | undefined;
  const url = () => `/api/meetings/${encodeURIComponent(selected)}/review`;
  async function run(work: () => Promise<void>) {
    if (busy || !active) return;
    busy = true;
    panel.disabled = picker.disabled = true;
    status.textContent = '';
    try { await work(); } catch (e) { status.textContent = humanError(e); }
    finally { busy = false; panel.disabled = picker.disabled = false; }
  }
  async function open(id: string) {
    const data: View = await api(`/api/meetings/${encodeURIComponent(id)}/review`, { quiet: true });
    if (!active) return;
    selected = id;
    draw(data);
  }
  function draw(data: View) {
    audio?.pause(); audio = undefined;
    dirty = false;
    const r = data.review;
    const title = h('input', { class: 'inpt', value: r.title }) as HTMLInputElement;
    const players = new Map<Track, HTMLAudioElement>();
    const tracks = data.tracks.filter(track => track.path).map(track => {
      const player = h('audio', { controls: true, preload: 'metadata', src: withToken(`${url()}/audio/${track.kind}`) }) as HTMLAudioElement;
      players.set(track.kind, player);
      player.onplay = () => { if (audio !== player) audio?.pause(); audio = player; };
      return field(`${trackName(track.kind)} · ${track.label}`, player);
    });
    const selectedIds = new Set<string>();
    const editors = r.paragraphs.map(p => {
      const text = h('textarea', { class: 'inpt', value: p.text }) as HTMLTextAreaElement;
      const speaker = h('input', { class: 'inpt', value: p.speaker || '' }) as HTMLInputElement;
      const confirm = h('input', { type: 'checkbox' }) as HTMLInputElement;
      speaker.oninput = () => { confirm.checked = true; };
      confirm.oninput = () => { if (!confirm.checked) speaker.value = p.speaker || ''; };
      const select = h('input', { type: 'checkbox' }) as HTMLInputElement;
      select.onchange = () => { if (select.checked) selectedIds.add(p.id); else selectedIds.delete(p.id); sync(); };
      const play = button(`${trackName(p.track)} ${stamp(p.start)}–${stamp(p.end)}`, () => {
        const player = players.get(p.track);
        if (!player) return;
        audio?.pause(); player.currentTime = p.start / 16000;
        player.ontimeupdate = () => { if (player.currentTime >= p.end / 16000) { player.pause(); player.ontimeupdate = null; } };
        void player.play().catch(e => { status.textContent = humanError(e); });
      });
      const origin = p.speaker_source === 'local-pitch' ? t('本地音高估计，待确认') : p.speaker_source === 'manual' ? t('人工标注') : t('说话人未确认');
      const element = h('article', { class: 'stack' }, play, field(t('段落文本'), text),
        h('details', {}, h('summary', {}, t('识别原文')), p.original), field(`${t('说话人')} · ${origin}`, speaker),
        h('label', {}, confirm, t('保存此段的人工标注')), h('label', {}, select, t('选择此段用于整理')));
      return { p, text, speaker, confirm, select, element };
    });
    const highlights = h('textarea', { class: 'inpt', value: r.highlights?.text || '' }) as HTMLTextAreaElement;
    async function mutate(action: object) {
      const review: Review = await api(url(), { method: 'POST', body: { revision: r.revision, ...action }, quiet: true });
      if (active) draw({ ...data, review });
    }
    const save = button(t('保存校对'), () => void run(() => mutate({ action: 'edit', title: title.value,
      paragraphs: editors.filter(e => e.text.value !== e.p.text || e.confirm.checked).map(e => ({ id: e.p.id, text: e.text.value, speaker: e.speaker.value, confirm_speaker: e.confirm.checked })), highlights: r.highlights ? highlights.value : null })));
    const controls: HTMLButtonElement[] = [];
    const attach = data.jobs.filter(j => !r.sources.some(s => s.track === j.track)).map(j => {
      const b = button(`${t('使用已完成的转写')} · ${trackName(j.track)} · ${j.model} · ${j.id}`, () => void run(() => mutate({ action: 'attach', job: j.id })));
      controls.push(b); return b;
    });
    const separate = button(t('本地估计说话人'), () => void run(() => mutate({ action: 'separate' })));
    const local = button(t('从所选段落提取本地要点'), () => void run(() => mutate({ action: 'local', segments: [...selectedIds] })));
    const remote = button(t('发送所选段落并生成要点'), () => {
      if (!data.destination) return;
      const excerpts = r.paragraphs.filter(p => selectedIds.has(p.id)).map(p => p.text).join('\n\n');
      if (window.confirm(`${t('仅将以下段落发送至')} ${provider(data.destination)}\n\n${excerpts}`)) {
        void run(() => mutate({ action: 'remote', consent: true, destination: data.destination, segments: [...selectedIds] }));
      }
    });
    const exportFile = button(t('导出会议 JSON'), () => void run(async () => {
      const output = await api(`${url()}/export`, { method: 'POST', quiet: true });
      downloadText(output.name, output.text, 'application/json');
    }));
    controls.push(exportFile);
    function sync() {
      dirty = title.value !== r.title || editors.some(e => e.text.value !== e.p.text || e.confirm.checked || e.speaker.value !== (e.p.speaker || '')) || highlights.value !== (r.highlights?.text || '');
      save.disabled = !dirty || !title.value.trim();
      for (const b of controls) b.disabled = dirty;
      separate.disabled = dirty || !editors.length;
      local.disabled = dirty || !selectedIds.size;
      remote.disabled = local.disabled || !data.destination;
    }
    const rename = (old: string) => {
      const value = window.prompt(t('修改所有同名说话人标注'), old);
      if (!value?.trim()) return;
      for (const e of editors) if (e.speaker.value === old) { e.speaker.value = value; e.confirm.checked = true; }
      sync();
    };
    const labels = [...new Set(r.paragraphs.map(p => p.speaker).filter((s): s is string => !!s))];
    contents.oninput = sync;
    contents.replaceChildren(field(t('会议标题'), title), h('p', {}, data.data_location), ...tracks,
      ...r.sources.map(s => h('p', {}, `${trackName(s.track)} · ${s.transport} · ${s.model} · ${s.job}`)), ...attach,
      ...(!editors.length ? [h('p', {}, t('完成轨道转写后可在此选择结果。'))] : []), separate,
      ...labels.map(s => button(`${t('重命名说话人')} · ${s}`, () => rename(s))), ...editors.map(e => e.element),
      group('sparkles', t('会议要点'), null, local, data.destination ? h('p', {}, provider(data.destination)) : h('p', {}, t('尚未配置远程整理服务')), remote,
        ...(r.highlights ? [
          h('p', {}, r.highlights.destination ? provider(r.highlights.destination) : t('本地摘录'), ` · v${r.highlights.source_revision} · ${r.highlights.segments.join(', ')}`),
          h('details', {}, h('summary', {}, t('整理使用的段落')), h('pre', {}, r.highlights.excerpts)),
          field(t('校对要点'), highlights),
          h('details', {}, h('summary', {}, t('生成的原始要点')), r.highlights.generated),
        ] : [])), save, exportFile);
    sync();
  }
  picker.onchange = () => {
    if (dirty && !window.confirm(t('放弃未保存的校对？'))) { picker.value = selected; return; }
    void run(async () => { try { await open(picker.value); } finally { picker.value = selected; } });
  };
  async function load() {
    const items: {id: string; started_at: string}[] = await api('/api/meetings/review', { quiet: true });
    picker.replaceChildren(...items.map(m => h('option', { value: m.id }, m.started_at)));
    if (!items.length) { contents.textContent = t('暂无已结束的会议'); return; }
    picker.value = items.some(m => m.id === selected) ? selected : items[0].id;
    await open(picker.value);
  }
  return { async mount(view: HTMLElement) {
    active = true;
    view.replaceChildren(pageHead(t('会议校对')), field(t('选择会议'), picker), button(t('刷新'), () => {
      if (!dirty || window.confirm(t('放弃未保存的校对？'))) void run(load);
    }), status, panel);
    await run(load);
  }, isDirty: () => dirty || busy, unmount() { active = false; audio?.pause(); } };
}
