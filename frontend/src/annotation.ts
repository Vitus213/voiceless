// The annotation page (capture-workflows 8.3): a region of the screen, or a
// local image, with strokes drawn over it and what was said about it as
// text. Both are handed out only by an explicit copy, save or share. The screen is
// captured with getDisplayMedia() and stays visible while it is; the
// display tracks end as soon as the still is taken, before any drawing or
// narration. Nothing of the work is kept once it is cancelled or the page
// is left, and nothing reaches another tool without a click.
import { t } from './i18n';
import { api, app, btn, downloadText, fetchStatus, group, h, humanError, icon, live, note, pageHead, tiles, tip } from './console.js';

type Stage = 'source' | 'live' | 'crop' | 'edit';
/** `[x, y]`, in the pixels of the image it is taken on. */
type Point = [number, number];
type Rect = { x: number; y: number; width: number; height: number };
type Tool = 'pen' | 'arrow' | 'box';
/** `weight` and the points are in the pixels of the annotated region. */
type Stroke = { tool: Tool; color: string; weight: number; points: Point[] };
/** A narration in progress: `before` is what the text box held when it began. */
type Narration = { before: string; session: number | null; seen: boolean; runtime: string };
type Notice = { kind: string; text: string };
type Session = NonNullable<NonNullable<typeof app.status>['session']>;

/** What was captured, as the track reports it. */
const SURFACE: Record<string, string> = { monitor: t('屏幕'), window: t('窗口'), browser: t('标签页') };
const PHASE: Record<string, string> = {
  preparing: t('正在准备麦克风'), recording: t('正在聆听'), recognizing: t('正在识别'),
  completed: t('解说完成'), cancelled: t('已取消'), 'no-speech': t('没有识别到语音，请再试一次'),
};
/** The step a failed narration stopped at, as the page names it. */
const FAILED_STEP: Record<string, string> = {
  preparing: t('麦克风启动失败'), recording: t('录音中断'), recognizing: t('识别失败'),
};
const TOOLS: { value: Tool; icon: string; title: string }[] = [
  { value: 'pen', icon: 'pencil', title: t('画笔') },
  { value: 'arrow', icon: 'arrow-right', title: t('箭头') },
  { value: 'box', icon: 'square', title: t('方框') },
];
const COLORS = [
  { value: '#ff3b30', name: t('红') }, { value: '#ffb020', name: t('黄') }, { value: '#30d158', name: t('绿') },
  { value: '#0a84ff', name: t('蓝') }, { value: '#ffffff', name: t('白') },
];
const GAUGES = [t('细'), t('中'), t('粗')];
/** The caption under the image: line height and padding as multiples of its font size. */
const LINE = 1.6;
const PAD = 1.25;
const SANS = '-apple-system, "PingFang SC", "Noto Sans CJK SC", "Microsoft YaHei", sans-serif';
/** An image shown: as wide as there is room, never wider than itself. */
const IMAGE = { display: 'block', width: '100%', height: 'auto' };
const OVERLAY = { position: 'absolute', inset: '0', width: '100%', height: '100%', touchAction: 'none', cursor: 'crosshair' };

const clamp = (value: number, low: number, high: number) => Math.min(high, Math.max(low, value));
/** Font size of the caption under an image `width` pixels wide: it stays readable at the zoom the image is looked at. */
const fontOf = (width: number) => Math.max(12, width / 60);
const bandHeight = (lines: number, font: number) => Math.round(lines * font * LINE + font * PAD * 2);

/** A canvas `width` by `height` with its 2D context; null when the browser cannot hold one that size. */
function surface(width: number, height: number) {
  const canvas = document.createElement('canvas');
  canvas.width = width;
  canvas.height = height;
  const ctx = canvas.getContext('2d');
  return ctx ? { canvas, ctx } : null;
}

/** The words of a narration after the text that was there before it, on a line of their own. */
function joined(before: string, words: string) {
  return before.trim() ? `${before.replace(/\s+$/u, '')}\n${words}` : words;
}

/** Name of an export: the minute it was made. */
function stamp(extension: string) {
  const now = new Date();
  const pair = (value: number) => String(value).padStart(2, '0');
  return `annotation-${now.getFullYear()}${pair(now.getMonth() + 1)}${pair(now.getDate())}-${pair(now.getHours())}${pair(now.getMinutes())}.${extension}`;
}

function downloadBlob(name: string, blob: Blob) {
  const url = URL.createObjectURL(blob);
  h('a', { href: url, download: name }).click();
  setTimeout(() => URL.revokeObjectURL(url), 1000);
}

/** Why the browser refused a write, in this page's words when it said why. */
function reason(error: unknown) {
  return error instanceof Error && error.name === 'NotAllowedError' ? t('未获得剪贴板权限') : humanError(error);
}

/** `text` in lines at most `limit` wide in `ctx`'s font; a word that does not fit whole starts the next line. */
function wrap(ctx: CanvasRenderingContext2D, text: string, limit: number): string[] {
  const lines: string[] = [];
  for (const paragraph of text.split(/\r\n?|\n/u)) {
    let line = '';
    for (const char of paragraph) {
      if (line && ctx.measureText(line + char).width > limit) {
        const space = line.lastIndexOf(' ');
        lines.push(space > 0 ? line.slice(0, space) : line);
        line = (space > 0 ? line.slice(space + 1) : '') + char;
      } else {
        line += char;
      }
    }
    lines.push(line);
  }
  return lines;
}

/** The strokes over the image, `scale` device pixels to one of the image. */
function drawStrokes(ctx: CanvasRenderingContext2D, strokes: Stroke[], scale: number) {
  ctx.setTransform(scale, 0, 0, scale, 0, 0);
  ctx.lineCap = ctx.lineJoin = 'round';
  for (const stroke of strokes) {
    const first = stroke.points[0];
    const last = stroke.points[stroke.points.length - 1];
    ctx.strokeStyle = ctx.fillStyle = stroke.color;
    ctx.lineWidth = stroke.weight;
    ctx.beginPath();
    if (stroke.tool === 'pen') {
      ctx.moveTo(first[0], first[1]);
      // Through the middle of each segment, which rounds the corners of the line.
      if (stroke.points.length < 3) ctx.lineTo(last[0] + 0.01, last[1]);
      else {
        for (let index = 1; index < stroke.points.length - 1; index += 1) {
          const [x, y] = stroke.points[index];
          const [next, after] = stroke.points[index + 1];
          ctx.quadraticCurveTo(x, y, (x + next) / 2, (y + after) / 2);
        }
        ctx.lineTo(last[0], last[1]);
      }
      ctx.stroke();
      continue;
    }
    if (stroke.tool === 'box') {
      ctx.globalAlpha = 0.2;
      ctx.fillRect(first[0], first[1], last[0] - first[0], last[1] - first[1]);
      ctx.globalAlpha = 1;
      ctx.strokeRect(first[0], first[1], last[0] - first[0], last[1] - first[1]);
      continue;
    }
    // An arrow: the shaft stops short of a head that is filled.
    const angle = Math.atan2(last[1] - first[1], last[0] - first[0]);
    const head = stroke.weight * 4;
    ctx.moveTo(first[0], first[1]);
    ctx.lineTo(last[0] - Math.cos(angle) * head * 0.6, last[1] - Math.sin(angle) * head * 0.6);
    ctx.stroke();
    ctx.beginPath();
    ctx.moveTo(last[0], last[1]);
    ctx.lineTo(last[0] - Math.cos(angle - 0.46) * head, last[1] - Math.sin(angle - 0.46) * head);
    ctx.lineTo(last[0] - Math.cos(angle + 0.46) * head, last[1] - Math.sin(angle + 0.46) * head);
    ctx.closePath();
    ctx.fill();
  }
}

/** Where the keyboard draws, as a cross that stands out on any background. */
function crosshair(ctx: CanvasRenderingContext2D, at: Point, scale: number) {
  const arm = 9 / scale;
  ctx.beginPath();
  ctx.moveTo(at[0] - arm, at[1]);
  ctx.lineTo(at[0] + arm, at[1]);
  ctx.moveTo(at[0], at[1] - arm);
  ctx.lineTo(at[0], at[1] + arm);
  ctx.lineWidth = 3 / scale;
  ctx.strokeStyle = '#000000';
  ctx.stroke();
  ctx.lineWidth = 1 / scale;
  ctx.strokeStyle = '#ffffff';
  ctx.stroke();
}

/** The caption in a band under an image `width` wide, as dark as the console with its own light ink. */
function drawCaption(ctx: CanvasRenderingContext2D, scale: number, lines: string[], width: number, font: number, top = 0) {
  ctx.setTransform(scale, 0, 0, scale, 0, top * scale);
  ctx.fillStyle = '#141517';
  ctx.fillRect(0, 0, width, bandHeight(lines.length, font));
  ctx.fillStyle = '#e8e6df';
  ctx.font = `${font}px ${SANS}`;
  ctx.textBaseline = 'top';
  lines.forEach((line, index) => ctx.fillText(line, font * PAD, font * PAD + index * font * LINE));
}

export function AnnotationPage() {
  let mounted = false;
  let stage: Stage = 'source';
  /** A narration request is with the runtime. */
  let busy = false;
  /** The work on hand was copied or saved since it last changed. */
  let delivered = false;
  const supported = window.isSecureContext && typeof navigator.mediaDevices?.getDisplayMedia === 'function';
  /** The screen while it is captured; null once the still is taken. */
  let stream: MediaStream | null = null;
  let sourceRequest = 0;
  let surfaceOf = '';
  let captureNote: Notice | null = null;
  /** The still whole, until a region of it is chosen. */
  let frame: HTMLCanvasElement | null = null;
  let crop: Rect | null = null;
  let cropDrag: Point | null = null;
  /** The region annotated, and its size in its own pixels. */
  let region: HTMLCanvasElement | null = null;
  let size: Point | null = null;
  let strokes: Stroke[] = [];
  let stroke: Stroke | null = null;
  let cursor: Point | null = null;
  let marks = true;
  let tool: Tool = 'pen';
  let color = COLORS[0].value;
  let gauge = 1;
  let spoken: Narration | null = null;
  const canShare = typeof navigator.share === 'function';
  let failedStep = '';
  let failure: Notice | null = null;
  let message = '';
  let result: Notice | null = null;
  let unsubscribe: (() => void) | undefined;
  const scratch = document.createElement('canvas');

  const video = h('video', { 'aria-label': t('实时采集画面'), style: { ...IMAGE } }) as HTMLVideoElement;
  const meter = h('meter', { min: 0, max: 1, value: 0, 'aria-label': t('麦克风电平') }) as HTMLMeterElement;
  const text = h('textarea', { class: 'inpt', rows: 4, 'aria-label': t('解说文字') }) as HTMLTextAreaElement;
  // The frame over the still while a region is chosen; keyboard users have the four numbers instead.
  const marquee = h('canvas', { 'aria-hidden': 'true', style: { ...OVERLAY, cursor: 'default' } }) as HTMLCanvasElement;
  const overlay = h('canvas', {
    role: 'img', tabindex: 0, 'aria-label': t('标注画布'),
    'aria-description': t('方向键移动，回车落笔，Esc 取消'), style: { ...OVERLAY },
  }) as HTMLCanvasElement;
  const band = h('canvas', { 'aria-hidden': 'true', style: { ...IMAGE } }) as HTMLCanvasElement;
  video.autoplay = true;
  video.muted = true;
  video.playsInline = true;

  const captureStatus = h('div', { role: 'status' });
  const cropStatus = h('div', { role: 'status' });
  const strokeStatus = h('div', { role: 'status' });
  // Plain text: the page's one live region (`announce`) already names each phase.
  const narrationStatus = h('div');
  const resultStatus = h('div', { role: 'status' });

  const captureWindow = btn('layers', t('采集窗口'), { onclick: () => void capture('window') }) as HTMLButtonElement;
  const captureScreen = btn('square', t('采集屏幕'), { onclick: () => void capture('monitor') }) as HTMLButtonElement;
  const file = h('input', { type: 'file', accept: 'image/*' }) as HTMLInputElement;
  const grabButton = btn('scan-text', t('拍摄静帧'), { cls: 'primary', onclick: grab }) as HTMLButtonElement;
  const stopCapture = btn('x', t('取消'), { onclick: () => discard(t('已取消，未保存也未发送任何内容')) }) as HTMLButtonElement;
  const annotateRegion = btn('check', t('标注此区域'), { cls: 'primary', onclick: () => { if (crop) annotate(crop); } }) as HTMLButtonElement;
  const annotateWhole = btn('scan-text', t('标注整张'), { onclick: () => { if (frame) annotate({ x: 0, y: 0, width: frame.width, height: frame.height }); } }) as HTMLButtonElement;
  const discardCrop = btn('x', t('取消'), { onclick: () => discard(t('已取消，未保存也未发送任何内容')) }) as HTMLButtonElement;
  const start = btn('mic', t('开始解说'), { cls: 'primary', onclick: () => void narrate() }) as HTMLButtonElement;
  const finish = btn('square', t('完成解说'), { onclick: () => void end('stop') }) as HTMLButtonElement;
  const cancelNarration = btn('x', t('取消解说'), { onclick: () => void end('cancel') }) as HTMLButtonElement;
  const copyImage = btn('copy', t('复制图片'), { cls: 'primary', onclick: () => void exportImage('copy') }) as HTMLButtonElement;
  const shareImage = btn('arrow-right', t('分享图片'), { onclick: () => void share('image') }) as HTMLButtonElement;
  const shareText = btn('arrow-right', t('分享文字'), { onclick: () => void share('text') }) as HTMLButtonElement;
  const showMarks = btn('eye', t('显示标注')) as HTMLButtonElement;
  const saveImage = btn('download', t('保存图片'), { onclick: () => void exportImage('save') }) as HTMLButtonElement;
  const copyText = btn('copy', t('复制文字'), { onclick: () => void exportText('copy') }) as HTMLButtonElement;
  const saveText = btn('download', t('保存文字'), { onclick: () => void exportText('save') }) as HTMLButtonElement;
  const discardAll = btn('x', t('取消'), { cls: 'danger', onclick: () => discard(t('已取消，未保存也未发送任何内容')) }) as HTMLButtonElement;
  const fileSource = h('label', {}, t('选择本地图片'), file);
  for (const button of [captureWindow, captureScreen]) tip(button, t('在浏览器的选择器里选择要采集的窗口或屏幕'));
  tip(grabButton, t('拍下当前画面后立即停止屏幕采集'));
  tip(annotateRegion, t('只留下框选的区域，其余画面丢弃'));
  tip(showMarks, t('暂时隐藏标注以对照原图'));

  const number = () => h('input', { type: 'number', class: 'inpt', step: 1 }) as HTMLInputElement;
  const lineLeft = number();
  const lineTop = number();
  const lineWidth = number();
  const lineHeight = number();
  const field = (name: string, input: HTMLInputElement) => h('label', { class: 'hist-filter' }, name, input);

  const tools = tiles(TOOLS, tool, (value) => { tool = value; }, { small: true, label: t('标注工具') });
  const palette = COLORS.map((entry) => {
    const swatch = h('span', {
      'aria-hidden': 'true',
      style: { width: '16px', height: '16px', borderRadius: '4px', background: entry.value, boxShadow: 'inset 0 0 0 1px var(--line)' },
    });
    return {
      entry,
      button: h('button', { type: 'button', class: 'tile sm', 'aria-label': entry.name, onclick: () => { color = entry.value; showPalette(); } }, swatch),
    };
  });
  const gaugeSelect = h('select', { class: 'inpt', 'aria-label': t('线宽'), style: { minWidth: '84px' }, onchange: (event: Event) => { gauge = (event.target as HTMLSelectElement).selectedIndex; } },
    ...GAUGES.map((label, index) => h('option', { value: index }, label))) as HTMLSelectElement;
  const undo = btn('undo-2', t('撤销'), { onclick: () => { strokes.pop(); delivered = false; refresh(); render(); } }) as HTMLButtonElement;
  const clearMarks = btn('trash-2', t('清除标注'), { onclick: () => { strokes = []; delivered = false; refresh(); render(); } }) as HTMLButtonElement;
  const caption = h('input', { type: 'checkbox', checked: true }) as HTMLInputElement;

  const cropView = h('div', { style: { position: 'relative', margin: '0 auto' } });
  const editView = h('div', { style: { position: 'relative', margin: '0 auto' } });
  const framed = (content: HTMLElement) => h('div', { class: 'inset', style: { padding: '0', overflow: 'hidden' } }, content);
  const actions = (...buttons: HTMLElement[]) => h('div', { class: 'inline', style: { flexWrap: 'wrap' } }, ...buttons);
  const sourceTools = actions(captureWindow, captureScreen);
  const liveView = h('div', { class: 'stack' }, framed(video), actions(grabButton, stopCapture));
  const cropGroup = group('target', t('范围'), null, cropStatus, framed(cropView),
    h('div', { class: 'hist-filters' }, field(t('左'), lineLeft), field(t('上'), lineTop), field(t('宽'), lineWidth), field(t('高'), lineHeight)),
    actions(annotateRegion, annotateWhole, discardCrop));
  const editGroup = group('pencil', t('标注'), null, framed(editView),
    h('div', { class: 'hist-filters' }, tools.el,
      h('div', { class: 'tiles sm', role: 'group', 'aria-label': t('标注颜色') }, ...palette.map(({ button }) => button)),
      h('label', { class: 'hist-filter' }, t('线宽'), gaugeSelect),
      actions(showMarks, undo, clearMarks)),
    strokeStatus);
  const narrationGroup = group('mic', t('解说'), null, actions(start, finish, cancelNarration, meter), narrationStatus, text);
  const resultGroup = group('save', t('结果'), null,
    h('label', { class: 'rocker' }, caption, h('span', { class: 'track', 'aria-hidden': 'true' }), h('span', { text: t('附上解说文字') })),
    actions(copyImage, saveImage, shareImage, copyText, saveText, shareText, discardAll), resultStatus);
  const observer = new ResizeObserver(() => refresh());

  /** A status line's new content, replaced only when it differs so that assistive technology does not repeat itself. */
  function say(line: HTMLElement, ...children: unknown[]) {
    const fresh = h('div', {}, ...children);
    if (fresh.innerHTML !== line.innerHTML) line.replaceChildren(...fresh.childNodes);
  }

  /** Keep the pointer reporting to `target` once it leaves, so a stroke does not end at the border. */
  function hold(target: HTMLCanvasElement, event: PointerEvent) {
    try {
      target.setPointerCapture(event.pointerId);
    } catch { /* the pointer is gone: the stroke ends where it was */ }
  }

  function showPalette() {
    for (const { entry, button } of palette) button.setAttribute('aria-pressed', String(entry.value === color));
  }

  function releaseDisplay() {
    const media = stream;
    stream = null;
    video.pause();
    video.srcObject = null;
    media?.getTracks().forEach((track) => track.stop());
  }

  /** The capture is over; `text` says why when it was not the still. */
  function ended(text?: string) {
    if (!stream) return;
    releaseDisplay();
    stage = 'source';
    captureNote = text ? { kind: 'warn', text } : null;
    render();
  }

  function captureError(error: unknown) {
    const name = error instanceof DOMException ? error.name : '';
    if (name === 'NotAllowedError' || name === 'AbortError') return t('屏幕采集被拒绝或未选择来源');
    if (['NotFoundError', 'NotSupportedError', 'NotReadableError', 'SecurityError'].includes(name)) {
      return t('此浏览器不支持屏幕采集，可选择本地图片');
    }
    return humanError(error);
  }

  async function capture(displaySurface: string) {
    // Straight from the click: the chooser opens only for a gesture that is still fresh.
    const request = ++sourceRequest;
    try {
      const media = await navigator.mediaDevices.getDisplayMedia({ video: { displaySurface, frameRate: 30 }, audio: false });
      const track = media.getVideoTracks()[0];
      if (!mounted || request !== sourceRequest || !track) {
        media.getTracks().forEach((item) => item.stop());
        if (!track && mounted && request === sourceRequest) throw new DOMException('no display track', 'NotSupportedError');
        return;
      }
      releaseDisplay();
      stream = media;
      surfaceOf = track.getSettings().displaySurface ?? '';
      // The browser's own stop button, or a window that closed.
      track.addEventListener('ended', () => { if (stream === media) ended(t('屏幕采集已结束')); });
      captureNote = null;
      video.srcObject = media;
      stage = 'live';
      render();
      // Interrupted when the capture ends first; anything else means the preview cannot start.
      video.play().catch((error: unknown) => {
        if (stream === media && !(error instanceof DOMException && error.name === 'AbortError')) ended(humanError(error));
      });
    } catch (error) {
      if (!mounted || request !== sourceRequest) return;
      releaseDisplay();
      if (stage === 'live') stage = 'source';
      captureNote = { kind: 'warn', text: captureError(error) };
      render();
    }
  }

  function grab() {
    if (stage !== 'live' || !video.videoWidth || !video.videoHeight) return;
    const still = surface(video.videoWidth, video.videoHeight);
    if (!still) {
      captureNote = { kind: 'err', text: t('无法生成图片') };
      render();
      return;
    }
    still.ctx.drawImage(video, 0, 0);
    releaseDisplay();
    adopt(still.canvas);
    annotateWhole.focus();
  }

  function adopt(still: HTMLCanvasElement) {
    frame = still;
    crop = null;
    cropDrag = null;
    lineLeft.max = String(still.width - 1);
    lineTop.max = String(still.height - 1);
    Object.assign(still.style, IMAGE);
    cropView.style.maxWidth = `${still.width}px`;
    cropView.replaceChildren(still, marquee);
    stage = 'crop';
    render();
  }

  /** A local image in place of a captured screen. */
  async function open(chosen: File) {
    const request = ++sourceRequest;
    const url = URL.createObjectURL(chosen);
    try {
      const image = new Image();
      image.src = url;
      await image.decode();
      if (!image.naturalWidth || !image.naturalHeight) throw new Error('empty image');
      if (!mounted || request !== sourceRequest) return;
      // Very large images are brought down to a size a canvas holds.
      const scale = Math.min(1, 8192 / Math.max(image.naturalWidth, image.naturalHeight));
      const still = surface(Math.round(image.naturalWidth * scale), Math.round(image.naturalHeight * scale));
      if (!still) throw new Error('no canvas');
      still.ctx.drawImage(image, 0, 0, still.canvas.width, still.canvas.height);
      adopt(still.canvas);
    } catch {
      if (mounted && request === sourceRequest) {
        captureNote = { kind: 'err', text: t('无法读取图片') };
        render();
      }
    } finally {
      URL.revokeObjectURL(url);
    }
  }

  function annotate(area: Rect) {
    const source = frame;
    const part = surface(area.width, area.height);
    if (!source || !part) {
      captureNote = { kind: 'err', text: t('无法生成图片') };
      render();
      return;
    }
    part.ctx.drawImage(source, area.x, area.y, area.width, area.height, 0, 0, area.width, area.height);
    // The rest of the screen is not kept.
    source.width = 0;
    source.height = 0;
    frame = null;
    crop = null;
    cropDrag = null;
    region = part.canvas;
    size = [area.width, area.height];
    strokes = [];
    stroke = null;
    cursor = null;
    Object.assign(part.canvas.style, IMAGE);
    editView.style.maxWidth = `${area.width}px`;
    const imageLayer = h('div', { style: { position: 'relative' } }, part.canvas, overlay);
    editView.replaceChildren(imageLayer, band);
    stage = 'edit';
    render();
    overlay.focus();
  }

  /** The frame over the still: the rest is dimmed, the region outlined. */
  function marque(source: HTMLCanvasElement) {
    const ctx = marquee.getContext('2d');
    const box = marquee.getBoundingClientRect();
    if (!ctx || !box.width || !box.height) return;
    const ratio = Math.min(window.devicePixelRatio || 1, 4);
    marquee.width = Math.round(box.width * ratio);
    marquee.height = Math.round(box.height * ratio);
    const scale = marquee.width / source.width;
    ctx.setTransform(scale, 0, 0, scale, 0, 0);
    ctx.clearRect(0, 0, source.width, source.height);
    if (!crop) return;
    ctx.fillStyle = 'rgba(0, 0, 0, 0.45)';
    ctx.fillRect(0, 0, source.width, crop.y);
    ctx.fillRect(0, crop.y + crop.height, source.width, source.height - crop.y - crop.height);
    ctx.fillRect(0, crop.y, crop.x, crop.height);
    ctx.fillRect(crop.x + crop.width, crop.y, source.width - crop.x - crop.width, crop.height);
    ctx.strokeStyle = '#ffb65c';
    ctx.lineWidth = 1.5 / scale;
    ctx.setLineDash([6 / scale, 4 / scale]);
    ctx.strokeRect(crop.x, crop.y, crop.width, crop.height);
  }

  /** The region's four numbers, the one being typed excepted, and the frame over the still. */
  function showCrop(source: HTMLCanvasElement, typing = false) {
    const area = crop ?? { x: 0, y: 0, width: source.width, height: source.height };
    const show = (input: HTMLInputElement, value: number) => {
      if (!typing || document.activeElement !== input) input.value = String(value);
    };
    show(lineLeft, area.x);
    show(lineTop, area.y);
    show(lineWidth, area.width);
    show(lineHeight, area.height);
    marque(source);
  }

  /** A typed edge of the region; the others follow so that it stays inside the still. */
  function typed(typing: boolean) {
    const source = frame;
    if (!source) return;
    const read = (input: HTMLInputElement, low: number, high: number) => clamp(Math.round(Number(input.value)), low, high);
    const x = read(lineLeft, 0, source.width - 1);
    const y = read(lineTop, 0, source.height - 1);
    crop = { x, y, width: read(lineWidth, 1, source.width - x), height: read(lineHeight, 1, source.height - y) };
    showCrop(source, typing);
    annotateRegion.disabled = false;
  }

  /** Where an event landed, in the pixels of the still or of the region. */
  function spot(event: PointerEvent, canvas: HTMLCanvasElement, bounds: Point, round: boolean): Point {
    const box = canvas.getBoundingClientRect();
    if (!box.width || !box.height) return [0, 0];
    const place = (along: number, start: number, room: number, limit: number) => clamp(along * limit / room, start, start + limit);
    const x = place(event.clientX - box.left, 0, box.width, bounds[0]);
    const y = place(event.clientY - box.top, 0, box.height, bounds[1]);
    return round ? [Math.round(x), Math.round(y)] : [x, y];
  }

  function begin(at: Point) {
    const short = Math.min(size?.[0] ?? 1, size?.[1] ?? 1);
    stroke = { tool, color, weight: Math.max(1.5, short / 270) * [1, 2, 4][gauge], points: [at, at] };
  }

  function extend(to: Point) {
    if (!stroke) return;
    const last = stroke.points[stroke.points.length - 1];
    if (last[0] === to[0] && last[1] === to[1]) return;
    if (stroke.tool === 'pen') stroke.points.push(to);
    else stroke.points[1] = to;
    refresh();
  }

  function lift() {
    const item = stroke;
    if (!item) return;
    stroke = null;
    const [first, last] = item.points;
    // An arrow or a box that was only tapped is no mark.
    if (item.tool === 'pen' || Math.hypot(last[0] - first[0], last[1] - first[1]) >= 2) {
      strokes.push(item);
      delivered = false;
    }
    refresh();
    render();
  }

  /** The caption under the image, or no caption. `width` is the image's own width. */
  function captionLines(width: number): string[] {
    const words = caption.checked ? text.value.trim() : '';
    const ctx = scratch.getContext('2d');
    if (!words || !ctx) return [];
    const font = fontOf(width);
    ctx.setTransform(1, 0, 0, 1, 0, 0);
    ctx.font = `${font}px ${SANS}`;
    return wrap(ctx, words, width - font * PAD * 2);
  }

  /** Paint what the page shows of the region: its strokes and, with text to add, the caption under it. */
  function refresh() {
    if (!mounted || stage !== 'edit' || !region || !size) return;
    const [width, height] = size;
    const box = region.getBoundingClientRect();
    const ratio = Math.min(window.devicePixelRatio || 1, 4);
    if (!box.width || !box.height) return;
    const scale = (box.width * ratio) / width;
    overlay.width = Math.round(box.width * ratio);
    overlay.height = Math.round(box.height * ratio);
    const ctx = overlay.getContext('2d');
    if (ctx) {
      ctx.setTransform(1, 0, 0, 1, 0, 0);
      ctx.clearRect(0, 0, overlay.width, overlay.height);
      drawStrokes(ctx, stroke ? [...strokes, stroke] : strokes, scale);
      if (cursor) crosshair(ctx, cursor, scale);
    }
    const lines = captionLines(width);
    band.hidden = !lines.length;
    const bandCtx = band.getContext('2d');
    if (!lines.length || !bandCtx) return;
    band.width = Math.round(box.width * ratio);
    band.height = Math.round(bandHeight(lines.length, fontOf(width)) * scale);
    drawCaption(bandCtx, scale, lines, width, fontOf(width));
  }

  /** The image as it is handed out: the region, its strokes and the caption, in its own pixels. */
  function renderImage(): Promise<Blob> {
    if (!region || !size) return Promise.reject(new Error(t('没有可导出的图片')));
    const [width, height] = size;
    const lines = captionLines(width);
    const font = fontOf(width);
    const output = surface(width, height + (lines.length ? bandHeight(lines.length, font) : 0));
    if (!output) return Promise.reject(new Error(t('无法生成图片')));
    output.ctx.drawImage(region, 0, 0);
    if (marks) drawStrokes(output.ctx, strokes, 1);
    if (lines.length) drawCaption(output.ctx, 1, lines, width, font, height);
    // toBlob answers by callback, and Promise.withResolvers is neither in the ES2022 library nor in older WebKit.
    return new Promise((resolve, reject) => {
      output.canvas.toBlob((blob) => {
        if (blob) resolve(blob);
        else reject(new Error(t('无法生成图片')));
      }, 'image/png');
    });
  }

  /** The words of the narration in progress are not wanted: the box gets back what it held. */
  function restore() {
    if (!spoken) return;
    text.value = spoken.before;
    spoken = null;
  }

  function lost(phrase: string) {
    restore();
    message = phrase;
    failure = null;
  }

  /** A narration whose session is over: its words go into the box, or the box gets back what it held. */
  function settle(session: Session | null, active: boolean) {
    if (!spoken || busy) return;
    if (app.status && spoken.runtime !== app.status.runtime_id) { lost(t('录音中断')); return; }
    if (session?.id === spoken.session) spoken.seen = true;
    // Nothing has come of it yet; only a runtime that was replaced meanwhile says it is gone.
    if (!spoken.seen) {
      if (session && spoken.session !== null && session.id > spoken.session) lost(t('录音中断'));
      return;
    }
    if (active) return;
    const own = session?.id === spoken.session ? session : null;
    if (own?.phase === 'completed' && own.transcript?.is_final) {
      text.value = joined(spoken.before, own.transcript.authoritative_text);
      message = t('解说完成');
      failure = null;
      delivered = false;
      spoken = null;
      return;
    }
    if (own?.phase === 'failed') {
      restore();
      failure = { kind: 'err', text: own.error || t('试说失败') };
      message = '';
      return;
    }
    lost(own?.phase === 'cancelled' ? t('已取消')
      : own?.phase === 'no-speech' ? t('没有识别到语音，请再试一次') : t('录音中断'));
  }

  async function narrate() {
    const runtime = app.status?.runtime_id;
    if (!runtime || busy || spoken) return;
    busy = true;
    message = '';
    failure = null;
    failedStep = '';
    const request: Narration = { before: text.value, session: null, seen: false, runtime };
    spoken = request;
    render();
    try {
      const response = await api('/api/session/narrate', { method: 'POST', body: { runtime_id: runtime }, quiet: true });
      if (!Number.isSafeInteger(response.session_id)) throw new Error(t('无法确认解说会话'));
      request.session = response.session_id;
      if (!mounted || spoken !== request) {
        cancelOwned(request);
        return;
      }
      // The session's first event may still be on its way: read the state instead of waiting for it.
      await fetchStatus().catch(() => {});
    } catch (error) {
      if (spoken !== request) return;
      spoken = null;
      failure = { kind: 'err', text: humanError(error) };
    } finally {
      busy = false;
      render();
    }
  }

  function cancelOwned(request: Narration) {
    if (request.session !== null) void api('/api/session/cancel', {
      method: 'POST', body: { runtime_id: request.runtime, session_id: request.session }, quiet: true, keepalive: true,
    }).catch(() => {});
  }

  async function end(action: 'stop' | 'cancel') {
    const request = spoken;
    if (busy || !request || request.session === null) return;
    busy = true;
    render();
    try {
      await api(`/api/session/${action}`, { method: 'POST', body: { runtime_id: request.runtime, session_id: request.session } });
      if (action === 'cancel' && spoken === request) lost(t('已取消'));
    } catch { /* the API displays the reason; keep ownership so the user can retry */ } finally {
      busy = false;
      render();
    }
  }

  function exportImage(kind: 'copy' | 'save') {
    if (!region || !size) return Promise.resolve();
    const copy = kind === 'copy';
    if (copy && (typeof ClipboardItem === 'undefined' || !navigator.clipboard?.write)) {
      result = { kind: 'err', text: t('此浏览器不支持复制图片，请改用“保存图片”') };
      render();
      return Promise.resolve();
    }
    // Begun here, inside the click: the clipboard takes an image only from a gesture that is still fresh.
    const pending = renderImage();
    // A failure is reported by whoever waits for the image.
    pending.catch(() => {});
    return (async () => {
      try {
        if (copy) {
          await navigator.clipboard.write([new ClipboardItem({ 'image/png': pending })]);
          result = { kind: 'ok', text: t('已复制图片') };
        } else {
          downloadBlob(stamp('png'), await pending);
          result = { kind: 'ok', text: t('已保存') };
        }
        delivered = true;
      } catch (error) {
        result = { kind: 'err', text: copy ? t(`复制图片失败：${reason(error)}，请改用“保存图片”`) : humanError(error) };
      }
      render();
    })();
  }

  function exportText(kind: 'copy' | 'save') {
    const words = text.value;
    if (!words.trim()) return Promise.resolve();
    return (async () => {
      try {
        if (kind === 'save') {
          downloadText(stamp('txt'), words, 'text/plain');
          result = { kind: 'ok', text: t('已保存') };
        } else {
          await navigator.clipboard.writeText(words);
          result = { kind: 'ok', text: t('已复制文字') };
        }
        delivered = true;
      } catch (error) {
        // The browser would not write it: the words are selected for its own copy command.
        text.select();
        result = { kind: 'err', text: typeof navigator.clipboard?.writeText === 'function'
          ? t(`复制文字失败：${reason(error)}，已选中文字，可手动复制`)
          : t('此浏览器不支持复制文字，已选中文字，可手动复制') };
      }
      render();
    })();
  }

  async function share(kind: 'image' | 'text') {
    const request = sourceRequest;
    try {
      const data: ShareData = kind === 'image'
        ? { files: [new File([await renderImage()], stamp('png'), { type: 'image/png' })] }
        : { text: text.value };
      if (!mounted || request !== sourceRequest) return;
      if (navigator.canShare && !navigator.canShare(data)) throw new Error(t('此浏览器无法分享所选内容，请复制或保存'));
      await navigator.share(data);
      if (request !== sourceRequest) return;
      result = { kind: 'ok', text: t('已分享') };
      delivered = true;
    } catch (error) {
      if (request !== sourceRequest) return;
      result = error instanceof DOMException && error.name === 'AbortError'
        ? { kind: '', text: t('已取消分享') } : { kind: 'err', text: humanError(error) };
    }
    render();
  }

  /** The work is dropped: the screen and the microphone are let go, and nothing of it stays. */
  function discard(note: string) {
    sourceRequest += 1;
    if (spoken) cancelOwned(spoken);
    restore();
    releaseDisplay();
    for (const canvas of [frame, region, overlay, band]) {
      if (canvas) {
        canvas.width = 0;
        canvas.height = 0;
      }
    }
    frame = null;
    region = null;
    size = null;
    crop = null;
    cropDrag = null;
    strokes = [];
    stroke = null;
    cursor = null;
    marks = true;
    text.value = '';
    failure = null;
    message = '';
    failedStep = '';
    result = null;
    delivered = false;
    captureNote = { kind: '', text: note };
    stage = 'source';
    cropView.replaceChildren();
    editView.replaceChildren();
    render();
    (supported ? captureScreen : file).focus();
  }

  function render() {
    if (!mounted) return;
    const status = app.status;
    const session = status?.session?.purpose === 'narration' ? status.session : null;
    const own = spoken?.runtime === status?.runtime_id && session && session.id === spoken?.session ? session : null;
    const active = Boolean(own && status?.session_id === own.id);
    if (own?.phase && own.phase !== 'failed') failedStep = own.phase;
    settle(session, active);

    sourceTools.hidden = !supported || stage !== 'source';
    liveView.hidden = stage !== 'live';
    cropGroup.hidden = stage !== 'crop';
    editGroup.hidden = resultGroup.hidden = stage !== 'edit';
    narrationGroup.hidden = stage === 'source' && !active;
    fileSource.hidden = stage !== 'source';
    grabButton.disabled = !video.videoWidth;
    annotateRegion.disabled = !crop;

    say(captureStatus,
      stage === 'live' ? h('div', { class: 'inline' },
        h('span', { class: 'chip warn' }, icon('activity'), t('正在采集')),
        [SURFACE[surfaceOf], video.videoWidth ? `${video.videoWidth} × ${video.videoHeight}` : ''].filter(Boolean).join(' · ')) : null,
      captureNote ? note(captureNote.kind === 'err' ? 'circle-alert' : 'info', captureNote.text, captureNote.kind)
        : !supported && stage === 'source' ? note('info', t('此浏览器不支持屏幕采集，可选择本地图片')) : null);

    if (stage === 'crop' && frame) {
      say(cropStatus, `${t('截图')} · ${frame.width} × ${frame.height}`);
      showCrop(frame);
    }
    if (stage === 'edit') {
      undo.disabled = clearMarks.disabled = !strokes.length;
      say(strokeStatus, strokes.length ? t(`标注：${strokes.length} 处`) : null);
      showMarks.setAttribute('aria-pressed', String(marks));
      overlay.hidden = !marks;
      caption.disabled = !text.value.trim();
    }

    // Other clients can start their own sessions while this page is open.
    const ready = stage !== 'source' && Boolean(status) && app.state === 'idle' && live.connected && !busy;
    const mine = Boolean(spoken);
    start.hidden = mine;
    finish.hidden = !(active && app.state === 'recording');
    cancelNarration.hidden = !mine;
    start.disabled = !ready;
    finish.disabled = cancelNarration.disabled = busy || spoken?.session == null;
    meter.hidden = !active;
    if (!active) meter.value = 0;
    text.disabled = Boolean(spoken);
    if (active && spoken && session?.transcript && !session.transcript.is_final) {
      text.value = joined(spoken.before, session.transcript.authoritative_text);
    }
    const phrase = spoken && (!active || busy) ? t('正在准备麦克风')
      : active && own ? (PHASE[own.phase ?? ''] || t('正在识别'))
      : failure ? failure.text : message;
    say(narrationStatus,
      !phrase ? (status && app.state !== 'down' ? null : h('p', { text: t('请先启动 voiceless daemon') }))
      : failure ? [note('circle-alert', `${FAILED_STEP[failedStep] ?? t('试说失败')}：${phrase}`, 'err'),
        btn('rotate-ccw', t('重试'), { cls: 'sm', onclick: () => void narrate() })]
      : [h('p', { text: phrase })]);

    shareImage.hidden = shareText.hidden = !canShare;
    copyImage.disabled = saveImage.disabled = shareImage.disabled = !region || mine;
    copyText.disabled = saveText.disabled = shareText.disabled = !text.value.trim() || mine;
    say(resultStatus, result ? [note(result.kind === 'ok' ? 'check' : 'circle-alert', result.text, result.kind)] : null);
    if (stage === 'edit') refresh();
  }

  marquee.addEventListener('pointerdown', (event: PointerEvent) => {
    if (stage !== 'crop' || !frame || cropDrag || (event.pointerType === 'mouse' && event.button !== 0)) return;
    hold(marquee, event);
    cropDrag = spot(event, marquee, [frame.width, frame.height], true);
    crop = null;
    marque(frame);
  });
  marquee.addEventListener('pointermove', (event: PointerEvent) => {
    if (!cropDrag || !frame) return;
    const to = spot(event, marquee, [frame.width, frame.height], true);
    const x = Math.min(cropDrag[0], to[0]);
    const y = Math.min(cropDrag[1], to[1]);
    crop = { x, y, width: Math.abs(cropDrag[0] - to[0]), height: Math.abs(cropDrag[1] - to[1]) };
    showCrop(frame);
  });
  const drop = () => {
    if (!cropDrag) return;
    cropDrag = null;
    // A tap, not a drag: the whole still stays.
    if (crop && (crop.width < 4 || crop.height < 4)) crop = null;
    annotateRegion.disabled = !crop;
    if (frame) showCrop(frame);
  };
  marquee.addEventListener('pointerup', drop);
  marquee.addEventListener('pointercancel', drop);
  for (const input of [lineLeft, lineTop, lineWidth, lineHeight]) {
    input.addEventListener('input', () => typed(true));
    input.addEventListener('change', () => typed(false));
  }

  overlay.addEventListener('pointerdown', (event: PointerEvent) => {
    if (stage !== 'edit' || !size || stroke || !marks || (event.pointerType === 'mouse' && event.button !== 0)) return;
    hold(overlay, event);
    begin(spot(event, overlay, size, false));
    refresh();
  });
  overlay.addEventListener('pointermove', (event: PointerEvent) => {
    if (stroke && size) extend(spot(event, overlay, size, false));
  });
  overlay.addEventListener('pointerleave', lift);
  overlay.addEventListener('pointerup', lift);
  overlay.addEventListener('pointercancel', lift);
  overlay.addEventListener('focus', () => {
    // The crosshair is for the keyboard: a pointer leaves no mark of its own on the image.
    if (stage === 'edit' && !cursor && size && overlay.matches(':focus-visible')) {
      cursor = [size[0] / 2, size[1] / 2];
      refresh();
    }
  });
  overlay.addEventListener('blur', () => {
    stroke = null;
    cursor = null;
    refresh();
  });
  overlay.addEventListener('keydown', (event: KeyboardEvent) => {
    if (stage !== 'edit' || !size) return;
    const keys = ['ArrowLeft', 'ArrowRight', 'ArrowUp', 'ArrowDown', ' ', 'Enter', 'Escape'];
    if (!keys.includes(event.key)) return;
    event.preventDefault();
    const [width, height] = size;
    const step = (Math.max(2, width / 200)) * (event.shiftKey ? 5 : 1);
    const at: Point = cursor ?? [width / 2, height / 2];
    if (event.key === 'Escape') {
      stroke = null;
    } else if (event.key === ' ' || event.key === 'Enter') {
      if (stroke) lift();
      else begin(at);
    } else {
      cursor = [
        clamp(at[0] + (event.key === 'ArrowLeft' ? -step : event.key === 'ArrowRight' ? step : 0), 0, width),
        clamp(at[1] + (event.key === 'ArrowUp' ? -step : event.key === 'ArrowDown' ? step : 0), 0, height),
      ];
      if (stroke) extend(cursor);
    }
    refresh();
  });

  showMarks.addEventListener('click', () => {
    delivered = false;
    marks = !marks;
    render();
  });
  caption.addEventListener('change', () => { delivered = false; refresh(); });
  text.addEventListener('input', () => {
    delivered = false;
    render();
  });
  file.addEventListener('change', () => {
    const chosen = file.files?.[0];
    file.value = '';
    if (chosen) void open(chosen);
  });
  for (const element of [video]) {
    element.addEventListener('loadedmetadata', render);
    element.addEventListener('resize', render);
  }

  const leave = () => discard('');
  showPalette();

  return {
    mount(view: HTMLElement) {
      mounted = true;
      captureWindow.hidden = captureScreen.hidden = !supported;
      view.append(
        pageHead(t('屏幕标注')),
        group('scan-text', t('来源'), null, captureStatus, sourceTools, fileSource, liveView),
        cropGroup, editGroup, narrationGroup, resultGroup,
      );
      observer.observe(editView);
      window.addEventListener('resize', refresh);
      window.addEventListener('pagehide', leave);
      // The runtime's events drive the narration; the level only moves the meter.
      unsubscribe = live.subscribe((event: { type: string; rms?: number }) => {
        if (event.type === 'level') {
          meter.value = event.rms ?? 0;
          return;
        }
        render();
      });
      render();
    },
    unmount() {
      mounted = false;
      observer.disconnect();
      unsubscribe?.();
      window.removeEventListener('resize', refresh);
      window.removeEventListener('pagehide', leave);
      discard('');
    },
    isDirty: () => stage !== 'source' && !delivered && Boolean(strokes.length || text.value.trim()),
  };
}
