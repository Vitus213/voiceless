import { t } from './i18n';
import { api, app, btn, deliveryText, fetchStatus, group, h, humanError, live, note, pageHead, row, setChildren } from './console.js';

type Microphone = { id: string; label: string };
type AudioDevices = { microphones: Microphone[]; system_default: string | null; effective: string | null; error?: string };
type Model = { id: string; name: string; kind: string; devices: string[]; installed: boolean; installing: boolean; progress: number | null; error: string | null };
type Config = {
  revision: string;
  audio: { source?: string };
  asr: { streaming_backend: string };
  local: { model: string; provider?: string; denoise: boolean; fast_preview: boolean };
  cloud: { has_api_key?: boolean };
  doubao: { has_access_key?: boolean };
};

const PHASE = {
  preparing: t('正在准备麦克风'),
  recording: t('正在聆听'),
  recognizing: t('正在识别'),
  completed: t('试说完成'),
  cancelled: t('已取消'),
  'no-speech': t('没有识别到语音，请再试一次'),
} as const;

/** The step a failed practice stopped at, as the page names it. */
const FAILED_STEP: Record<string, string> = {
  preparing: t('麦克风启动失败'),
  recording: t('录音中断'),
  recognizing: t('识别失败'),
};
/** Steps a missing or lost microphone explains; the others are retried as they are. */
const MICROPHONE_STEPS: Record<string, true> = { preparing: true, recording: true };

function option(value: string, label: string) {
  return h('option', { value, text: label });
}

export function SetupPage() {
  let config: Config | null = null;
  let audio: AudioDevices | null = null;
  let models: Model[] = [];
  let mounted = false;
  let busy = false;
  let testingOutput = false;
  let actionError = '';
  let unsubscribe: (() => void) | undefined;
  let poll: number | undefined;
  let armTimer: number | undefined;
  let armInstance: string | null = null;
  let lastResult: string | undefined;
  let failedStep: string | undefined;
  let initialized = false;
  let modelOptions = '';
  let microphoneOptions = '';
  let shownShortcut: string | null | undefined;

  const input = h('select', { class: 'inpt' }) as HTMLSelectElement;
  const recognition = h('select', { class: 'inpt' }, option('current', t('当前配置')), option('local-offline', t('本地离线'))) as HTMLSelectElement;
  const model = h('select', { class: 'inpt' }) as HTMLSelectElement;
  const micStatus = h('div', { role: 'status' });
  const modelStatus = h('div', { role: 'status' });
  const shortcutStatus = h('div');
  // Plain text: the page's one live region (`announce`) already names each phase.
  const sessionStatus = h('p');
  const outputStatus = h('p', { role: 'status' });
  const actionStatus = h('div', { role: 'status' });
  const meter = h('meter', { min: 0, max: 1, value: 0, 'aria-label': t('麦克风电平') }) as HTMLMeterElement;
  const result = h('textarea', { class: 'inpt', rows: 4, 'aria-label': t('练习结果') }) as HTMLTextAreaElement;
  const install = btn('download', t('准备模型'), { onclick: () => void perform(installModels) }) as HTMLButtonElement;
  const apply = btn('check', t('应用设置'), { onclick: () => void perform(applyConfig) }) as HTMLButtonElement;
  const start = btn('mic', t('开始试说'), { cls: 'primary', onclick: () => void practice(false) }) as HTMLButtonElement;
  const arm = btn('keyboard', t('用快捷键试说'), { onclick: () => void practice(true) }) as HTMLButtonElement;
  const stop = btn('square', t('完成试说'), { onclick: () => void endPractice('stop') }) as HTMLButtonElement;
  const cancel = btn('x', t('取消试说'), { onclick: () => void endPractice('cancel') }) as HTMLButtonElement;
  const inject = btn('corner-down-left', t('测试上屏'), { onclick: () => void tryInjection() }) as HTMLButtonElement;

  function updateOptions() {
    const microphones = audio?.microphones ?? [];
    const micKey = JSON.stringify(microphones);
    if (micKey !== microphoneOptions) {
      microphoneOptions = micKey;
      const selected = initialized ? input.value : config?.audio.source ?? '';
      const absent = selected && !microphones.some(({ id }) => id === selected);
      input.replaceChildren(option('', t('跟随系统默认')), ...microphones.map(({ id, label }) => option(id, label)));
      if (absent) input.append(option(selected, t(`${selected} · 不可用`)));
      input.value = selected;
    }
    const candidates = models.filter((value) => value.devices.length && ['gguf', 'sherpa'].includes(value.kind));
    const modelKey = JSON.stringify(candidates.map(({ id, name }) => [id, name]));
    if (modelKey !== modelOptions) {
      modelOptions = modelKey;
      const selected = initialized ? model.value : config?.local.model;
      model.replaceChildren(...candidates.map(({ id, name }) => option(id, name)));
      // A clean install starts from a model that is already there, else from the smallest runnable one.
      const current = candidates.find(({ id }) => id === selected);
      const initial = (current?.installed ? current : undefined)
        ?? candidates.find(({ installed }) => installed)
        ?? candidates.find(({ id }) => id === 'qwen3-asr-0.6b-gguf')
        ?? candidates.find(({ id }) => id === 'sense-voice-int8');
      if (initial) model.value = initial.id;
    }
    if (!initialized && config && app.status) {
      recognition.value = app.status.configured ? 'current' : 'local-offline';
      initialized = true;
    }
  }

  function candidate() {
    return models.find(({ id }) => id === model.value);
  }

  function requiredModels() {
    if (recognition.value !== 'local-offline') return [];
    const ids = new Set([model.value, 'silero-vad']);
    if (config?.local.denoise) ids.add('gtcrn');
    if (config?.local.fast_preview && model.value !== 'sense-voice-int8') ids.add('sense-voice-int8');
    return models.filter(({ id }) => ids.has(id));
  }

  function changes() {
    const fields: Record<string, string | null> = {};
    if (!config) return fields;
    if ((config.audio.source ?? '') !== input.value) fields['audio.source'] = input.value || null;
    if (recognition.value === 'local-offline' && model.value) {
      if (config.asr.streaming_backend !== 'local-offline') fields['asr.streaming_backend'] = 'local-offline';
      if (config.local.model !== model.value) fields['local.model'] = model.value;
      const selected = candidate();
      if (selected?.kind === 'sherpa' && config.local.provider !== 'cpu') fields['local.provider'] = 'cpu';
      else if (selected && config.local.provider && !selected.devices.includes(config.local.provider)) fields['local.provider'] = null;
    }
    return fields;
  }

  function render() {
    if (!mounted) return;
    updateOptions();
    const status = app.status;
    const session = status?.session;
    const activePractice = Boolean(session?.practice && status?.session_id === session.id);
    const mic = input.value || audio?.system_default;
    const micReady = Boolean(mic && audio?.microphones?.some(({ id }) => id === mic));
    const label = audio?.microphones?.find(({ id }) => id === mic)?.label;
    const files = requiredModels();
    const downloading = files.find(({ installing }) => installing);
    const failed = files.find(({ error }) => error);
    const missing = files.some(({ installed }) => !installed);
    const applyState = status?.config_apply;
    const pending = applyState?.state === 'queued' || applyState?.state === 'loading';
    const dirty = Object.keys(changes()).length > 0;
    const applied = Boolean(config && applyState?.active_revision === config.revision);
    const selectedBackend = recognition.value === 'local-offline' ? 'local-offline' : status?.backend;
    const backendReady = selectedBackend === 'cloud-realtime' ? config?.cloud?.has_api_key
      : selectedBackend === 'doubao-realtime' ? config?.doubao?.has_access_key : status?.engine?.loaded;
    const shortcut = status?.hotkey?.bindings?.find(({ field, keyboards }: { field: string; keyboards: number }) =>
      keyboards > 0 && ['key', 'hold_key', 'toggle_key', 'raw_key'].includes(field))?.chord || status?.hotkey?.portal?.trigger || null;
    const daemon = Boolean(status && app.state !== 'down');
    const ready = daemon && app.state === 'idle' && live.connected && !busy && !pending && micReady && !missing && !dirty && applied && Boolean(backendReady);

    const microphoneMessage = label || audio?.error || t('未找到可用麦克风');
    if (micStatus.textContent !== microphoneMessage) {
      setChildren(micStatus, note(micReady ? 'mic' : 'mic-off', microphoneMessage, micReady ? '' : 'warn'));
    }
    const modelMessage = pending ? t('等待设置生效')
      : applyState?.state === 'failed' && !applied ? applyState.error
      : downloading ? `${downloading.name} · ${t(downloading.progress == null ? '校验中' : Math.round(downloading.progress * 100) + '%')}`
      : failed?.error || (missing ? t('需要下载模型')
        : dirty || !applied ? t('点击“应用设置”后生效')
        : status?.engine?.error || (backendReady ? t('识别就绪') : t('请检查识别设置')));
    if (modelStatus.textContent !== modelMessage) setChildren(modelStatus, note('cpu', modelMessage, failed ? 'err' : ''));
    if (shownShortcut !== shortcut) {
      shownShortcut = shortcut;
      setChildren(shortcutStatus, shortcut ? h('kbd', { class: 'keycap', text: shortcut.replaceAll('KEY_', '') })
        : h('a', { href: '#hotkey', text: t('设置快捷键') }));
    }
    setChildren(actionStatus, actionError ? note('circle-alert', actionError, 'err') : null);
    model.disabled = recognition.value !== 'local-offline';
    install.hidden = recognition.value !== 'local-offline' || !missing;
    install.disabled = busy || Boolean(downloading) || !candidate();
    apply.disabled = !daemon || busy || pending || missing || !micReady || (!dirty && applied);
    start.disabled = !ready;
    arm.disabled = !ready || Boolean(armTimer);
    stop.hidden = !activePractice || app.state !== 'recording';
    cancel.hidden = !activePractice;
    inject.disabled = testingOutput || !result.value.trim() || activePractice;
    if (app.state !== 'recording') meter.value = 0;

    if (session?.practice && session.phase !== 'failed') failedStep = session.phase;
    const practiceFailed = Boolean(session?.practice && session.phase === 'failed');
    const phrase = !daemon ? t('请先启动 voiceless daemon')
      : armTimer && !activePractice ? t('现在按下快捷键，下一次录音会进入试说')
      : practiceFailed ? session?.error || t('试说失败')
      : session?.practice ? (PHASE[session.phase as keyof typeof PHASE] || t('正在识别'))
      : '';
    setChildren(sessionStatus, !phrase ? []
      : practiceFailed ? [
        note('circle-alert', `${t(FAILED_STEP[failedStep ?? ''] ?? '试说失败')}：${phrase}`, 'err'),
        failedStep && MICROPHONE_STEPS[failedStep]
          ? btn('refresh-cw', t('重新检查麦克风'), { cls: 'sm', onclick: () => void refresh() })
          : btn('rotate-ccw', t('重试'), { cls: 'sm', onclick: () => void practice(false) }),
      ]
      : [phrase]);
    if (status && session?.practice && session.phase === 'completed' && session.transcript?.is_final) {
      const identity = `${status.runtime_id}:${session.id}`;
      if (lastResult !== identity) {
        lastResult = identity;
        result.value = session.transcript.authoritative_text;
        inject.disabled = testingOutput || !result.value.trim() || activePractice;
      }
    }
  }

  async function refresh() {
    clearTimeout(poll);
    const responses = await Promise.allSettled([
      api('/api/config', { quiet: true }), api('/api/desktop/audio', { quiet: true }), api('/api/models', { quiet: true }),
    ]);
    if (!mounted) return;
    const [settings, devices, catalog] = responses;
    if (settings.status === 'fulfilled') config = settings.value;
    if (devices.status === 'fulfilled') audio = devices.value;
    if (catalog.status === 'fulfilled') models = catalog.value;
    const error = responses.find((response): response is PromiseRejectedResult => response.status === 'rejected');
    if (error) actionError = humanError(error.reason);
    render();
    poll = window.setTimeout(() => void refresh(), models.some(({ installing }) => installing) ? 700 : 2500);
  }

  async function perform(action: () => Promise<void>) {
    if (busy) return;
    busy = true;
    actionError = '';
    render();
    try {
      await action();
    } catch (error) {
      actionError = humanError(error);
    } finally {
      busy = false;
      render();
    }
  }

  async function installModels() {
    for (const { id, installed, installing } of requiredModels()) {
      if (!installed && !installing) await api(`/api/models/${encodeURIComponent(id)}/install`, { method: 'POST', quiet: true });
    }
    await refresh();
  }

  async function applyConfig() {
    if (!config) return;
    const fields = changes();
    let revision = config.revision;
    if (Object.keys(fields).length) {
      const saved = await api('/api/config', { method: 'PUT', body: { revision: config.revision, fields }, quiet: true });
      revision = saved.revision;
      config = { ...saved.config, revision: saved.revision };
    }
    await api('/api/engine/reload', { method: 'POST', body: { revision }, quiet: true });
    await fetchStatus();
  }

  function clearArm() {
    clearTimeout(armTimer);
    armTimer = undefined;
    armInstance = null;
  }

  async function practice(byShortcut: boolean) {
    await perform(async () => {
      const instance = live.runtimeId;
      const response = await api('/api/session/practice', { method: 'POST', body: { arm: byShortcut, runtime_id: instance }, quiet: true });
      if (response.armed_for_ms && app.status?.state === 'idle') {
        armInstance = instance;
        armTimer = window.setTimeout(() => {
          clearArm();
          actionError = t('没有收到快捷键，请检查键盘权限或合成器绑定');
          render();
        }, response.armed_for_ms);
      }
    });
  }

  async function endPractice(action: 'stop' | 'cancel') {
    await perform(async () => { await api(`/api/session/${action}`, { method: 'POST', body: live.scope(), quiet: true }); });
  }

  async function tryInjection() {
    if (inject.disabled || testingOutput) return;
    testingOutput = true;
    inject.disabled = true;
    outputStatus.textContent = t('请在 3 秒内切换到要测试的输入框');
    try {
      const response = await api('/api/inject/test', { method: 'POST', body: { text: result.value, delay_ms: 3000 }, quiet: true });
      outputStatus.textContent = deliveryText(response.injection);
    } catch (error) {
      outputStatus.textContent = humanError(error);
    } finally {
      testingOutput = false;
      render();
    }
  }

  for (const select of [recognition, model, input]) select.addEventListener('change', render);
  result.addEventListener('input', render);

  function releaseArm() {
    const runtimeId = armInstance;
    clearArm();
    if (runtimeId) void api('/api/session/disarm-practice', { method: 'POST', body: { runtime_id: runtimeId }, quiet: true }).catch(() => {});
  }

  return {
    async mount(view: HTMLElement) {
      mounted = true;
      view.append(
        pageHead(t('首次试说')),
        group('mic', t('麦克风'), btn('refresh-cw', t('刷新设备'), { onclick: () => void refresh() }), row(t('输入设备'), input), micStatus, meter),
        group('cpu', t('识别'), null, row(t('识别方式'), recognition), row(t('本地模型'), model), modelStatus,
          h('div', { class: 'inline' }, install, apply, h('a', { href: '#engine', text: t('更多识别设置') }))),
        group('keyboard', t('快捷键'), null, shortcutStatus),
        group('audio-lines', t('练习'), null, h('div', { class: 'inline' }, start, arm, stop, cancel), sessionStatus,
          result, h('div', { class: 'inline' }, inject, h('a', { href: '#live', text: t('开始日常听写') })), outputStatus, actionStatus),
      );
      unsubscribe = live.subscribe((event: { type: string; rms?: number }) => {
        if (event.type === 'level') {
          if (app.status?.session?.practice) meter.value = event.rms ?? 0;
          return;
        }
        if (app.status?.state === 'recording' || (armInstance && armInstance !== live.runtimeId)) clearArm();
        if (event.type !== 'transcript' && event.type !== 'log') render();
      });
      window.addEventListener('pagehide', releaseArm);
      render();
      await Promise.allSettled([refresh(), fetchStatus()]);
      render();
    },
    unmount() {
      mounted = false;
      clearTimeout(poll);
      unsubscribe?.();
      window.removeEventListener('pagehide', releaseArm);
      releaseArm();
    },
  };
}
