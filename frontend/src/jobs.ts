import { t } from './i18n';
import { api, btn, confirmDialog, downloadText, group, h, humanError, note, pageHead, row, uploadFile } from './console.js';

type JobState = 'queued' | 'running' | 'paused' | 'cancelled' | 'completed' | 'failed';
type Job = {
  id: string;
  status: JobState;
  activity: string | null;
  input: { name: string };
  progress: { position_seconds: number; fraction: number | null; segments: number };
  model: string;
  language: string;
  transport: 'local' | 'cloud';
  error: { code: string; message: string; remedy: string | null } | null;
};
type JobView = Job & { segments: { start: number; end: number; text: string }[] };
type Model = { id: string; name: string; installed: boolean; kind: string; languages: string[] };
type JobList = { jobs: Job[]; native_formats: string; additional_formats: string | null; max_upload_bytes: number };

const states: Record<JobState, () => string> = {
  queued: () => t('排队中'), running: () => t('正在转写'), paused: () => t('已暂停'),
  cancelled: () => t('已取消'), completed: () => t('已完成'), failed: () => t('转写失败'),
};

function stamp(samples: number) {
  const seconds = Math.floor(samples / 16000);
  return `${Math.floor(seconds / 60)}:${String(seconds % 60).padStart(2, '0')}`;
}

export function JobsPage() {
  let mounted = false;
  let polling: number | undefined;
  let uploadLimit = 0;
  let cloudModel = '';
  let uploadActive = false;
  let lastList = '';
  let selected = '';
  const formatNote = h('p', { class: 'lab' });
  const uploadStatus = h('div', { class: 'stack', role: 'status' });
  const listing = h('div', { class: 'stack' });
  const details = h('div', { class: 'stack' });
  const transport = h('select', { class: 'inpt', 'aria-label': t('转写方式') },
    h('option', { value: 'local' }, t('本地转写')), h('option', { value: 'cloud' }, t('云端转写')),
  ) as HTMLSelectElement;
  const model = h('select', { class: 'inpt', 'aria-label': t('本地模型') }) as HTMLSelectElement;
  const language = h('select', { class: 'inpt', 'aria-label': t('语言') }) as HTMLSelectElement;
  let installed: Model[] = [];
  function updateLanguages() {
    const chosen = installed.find((item) => item.id === model.value);
    const options = transport.value === 'local' && chosen?.id === 'sense-voice-int8' ? chosen.languages : [];
    language.replaceChildren(h('option', { value: 'auto' }, t('自动检测语言')),
      ...options.map((code) => h('option', { value: code }, code)));
  }
  const allowCloud = h('input', { type: 'checkbox' }) as HTMLInputElement;
  const cloudGrant = h('label', { hidden: true }, allowCloud, t('允许将这些文件发送给已配置的云端识别服务'));
  const selection = h('input', { type: 'file', multiple: true, 'aria-label': t('选择音视频文件') }) as HTMLInputElement;
  const add = btn('plus', t('加入转写队列'), { cls: 'primary', disabled: true, onclick: () => void submit([...selection.files ?? []]) }) as HTMLButtonElement;

  function updateControls() {
    const cloud = transport.value === 'cloud';
    cloudGrant.hidden = !cloud;
    model.disabled = cloud || uploadActive;
    language.disabled = uploadActive || language.options.length < 2;
    transport.disabled = uploadActive;
    allowCloud.disabled = uploadActive;
    selection.disabled = uploadActive;
    add.disabled = !uploadLimit || uploadActive || !selection.files?.length || (cloud ? !allowCloud.checked || !cloudModel : !model.value);
  }

  transport.addEventListener('change', () => { allowCloud.checked = false; updateLanguages(); updateControls(); });
  model.addEventListener('change', () => { updateLanguages(); updateControls(); });
  selection.addEventListener('change', updateControls);
  allowCloud.addEventListener('change', updateControls);

  async function submit(files: File[]) {
    if (!files.length || uploadActive || !uploadLimit) return;
    const destination = transport.value;
    const cloud = destination === 'cloud';
    const modelId = cloud ? cloudModel : model.value;
    if (!modelId || (cloud && !allowCloud.checked)) return;
    uploadActive = true;
    updateControls();
    uploadStatus.replaceChildren();
    const meter = h('progress', { max: 1, value: 0 }) as HTMLProgressElement;
    const name = h('span');
    const progress = h('div', { class: 'inline' }, name, meter);
    uploadStatus.append(progress);
    try {
      for (const file of files) {
        name.textContent = file.name;
        meter.value = 0;
        const params = new URLSearchParams({ name: file.name, model: modelId, transport: destination, cloud_authorized: String(cloud) });
        if (!cloud && language.value !== 'auto') params.set('language', language.value);
        try {
          if (file.size === 0 || file.size > uploadLimit) throw new Error(t('文件大小超出转写范围'));
          await uploadFile(`/api/jobs?${params}`, file, (loaded: number, total: number) => { meter.value = loaded / total; }).done;
        } catch (error) {
          uploadStatus.append(note('circle-alert', `${file.name} · ${humanError(error)}`, 'err'));
        }
      }
    } finally {
      progress.remove();
      uploadActive = false;
      allowCloud.checked = false;
      selection.value = '';
      updateControls();
      if (mounted) await refresh();
    }
  }

  async function action(id: string, command: string) {
    try {
      await api(`/api/jobs/${id}/${command}`, { method: 'POST' });
      await refresh();
    } catch { /* the API shows its error */ }
  }

  async function showSegments(id: string) {
    try {
      const job: JobView = await api(`/api/jobs/${id}`);
      if (!mounted || selected !== id) return;
      const exportButton = (format: string) => btn('download', format.toUpperCase(), { onclick: async () => {
        try {
          const result = await api(`/api/jobs/${id}/export/${format}`);
          downloadText(result.name, result.text, result.type);
        } catch { /* the API shows its error */ }
      } });
      details.replaceChildren(group('captions', job.input.name, null,
        h('div', { class: 'inline', style: { flexWrap: 'wrap' } }, ...['txt', 'json', 'srt', 'vtt'].map(exportButton)),
        h('ol', { class: 'stack' }, ...job.segments.map((segment) => h('li', {},
          h('span', { class: 'mono', text: `${stamp(segment.start)} – ${stamp(segment.end)} ` }), segment.text,
        ))),
      ));
    } catch { /* the API shows its error */ }
  }

  function jobCard(job: Job) {
    const id = encodeURIComponent(job.id);
    const command = (icon: string, label: string, name: string) => btn(icon, label, { onclick: () => void action(id, name) });
    const active = job.status === 'queued' || job.status === 'running';
    const replace = ['source-unavailable', 'source-mismatch', 'unreadable', 'corrupt', 'unsupported', 'no-audio'].includes(job.error?.code ?? '');
    return group('file-audio', job.input.name, null,
      h('p', { class: 'lab', text: [states[job.status](), job.transport === 'cloud' ? t('云端转写') : t('本地转写'), job.model,
        job.language === 'auto' ? t('自动检测语言') : job.language].join(' · ') }),
      job.activity === 'waiting-for-resource' ? note('mic', t('等待实时听写释放识别资源')) : null,
      job.progress.fraction != null ? h('progress', { max: 1, value: job.progress.fraction, 'aria-label': t('转写进度') }) : null,
      job.error ? note('circle-alert', [job.error.message, job.error.remedy].filter(Boolean).join(' '), 'err') : null,
      h('div', { class: 'inline', style: { flexWrap: 'wrap' } },
        active ? command('pause', t('暂停'), 'pause') : null,
        job.status === 'paused' ? command('play', t('继续'), 'resume') : null,
        active || job.status === 'paused' ? command('square', t('取消转写'), 'cancel') : null,
        (job.status === 'failed' && job.error?.code !== 'cloud-not-authorized') || job.status === 'cancelled' ? command('rotate-ccw', t('重试'), 'retry') : null,
        replace ? h('label', {}, t('重新选择源文件'), h('input', { type: 'file', 'aria-label': t('重新选择源文件'), onchange: async (event: Event) => {
          const file = (event.target as HTMLInputElement).files?.[0];
          if (!file) return;
          try {
            await uploadFile(`/api/jobs/${id}/reselect`, file, () => {}).done;
            await refresh();
          } catch (error) { uploadStatus.append(note('circle-alert', humanError(error), 'err')); }
        } })) : null,
        job.progress.segments || job.status === 'completed' ? btn('captions', t('查看结果'), { onclick: () => { selected = id; void showSegments(id); } }) : null,
        !active ? btn('trash-2', t('移除任务'), { onclick: async () => {
          if (!await confirmDialog(t('移除此任务及其导入的副本？'), { okText: t('移除'), danger: true })) return;
          try {
            await api(`/api/jobs/${id}`, { method: 'DELETE' });
            if (selected === id) { selected = ''; details.replaceChildren(); }
            await refresh();
          } catch { /* the API shows its error */ }
        } }) : null,
      ),
    );
  }

  async function refresh() {
    try {
      const snapshot: JobList = await api('/api/jobs', { quiet: true });
      if (!mounted) return;
      uploadLimit = snapshot.max_upload_bytes;
      formatNote.textContent = [snapshot.native_formats, snapshot.additional_formats ?? t('安装 FFmpeg 可识别更多格式')].join(' · ');
      const next = JSON.stringify(snapshot.jobs);
      if (next !== lastList) {
        lastList = next;
        listing.replaceChildren(...(snapshot.jobs.length ? snapshot.jobs.map(jobCard) : [note('file-audio', t('没有转写任务'))]));
        if (selected) await showSegments(selected);
      }
    } catch (error) {
      lastList = '';
      uploadLimit = 0;
      if (mounted) listing.replaceChildren(note('circle-alert', humanError(error), 'err'));
    }
    updateControls();
  }

  return {
    async mount(view: HTMLElement) {
      mounted = true;
      const drop = h('div', { class: 'stack', ondragover: (event: DragEvent) => event.preventDefault(), ondrop: (event: DragEvent) => {
        event.preventDefault();
        void submit([...event.dataTransfer?.files ?? []]);
      } }, row(t('转写方式'), transport), row(t('本地模型'), model), row(t('语言'), language), cloudGrant,
        h('label', {}, t('选择或拖入音视频'), selection), add, formatNote, uploadStatus,
      );
      view.append(pageHead(t('文件转写')), group('file-audio', t('导入音视频'), null, drop), listing, details);
      const [models, config] = await Promise.allSettled([api('/api/models'), api('/api/config')]);
      if (!mounted) return;
      if (config.status === 'fulfilled') cloudModel = config.value.cloud.model || '';
      if (models.status === 'fulfilled') {
        installed = (models.value as Model[]).filter((item) => item.installed && ['gguf', 'sherpa'].includes(item.kind));
      }
      for (const result of [models, config]) {
        if (result.status === 'rejected') uploadStatus.append(note('circle-alert', humanError(result.reason), 'err'));
      }
      model.replaceChildren(...installed.map((item) => h('option', { value: item.id, text: item.name })));
      updateLanguages();
      if (!model.options.length) model.append(h('option', { value: '' }, t('请先安装本地模型')));
      await refresh();
      if (mounted) polling = window.setInterval(() => void refresh(), 1200);
    },
    unmount() {
      mounted = false;
      window.clearInterval(polling);
    },
  };
}
