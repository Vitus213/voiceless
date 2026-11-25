import { t } from './i18n';
import './style.css';
import { SetupPage } from './setup';
import { JobsPage } from './jobs';
import { MeetingsPage } from './meetings';
import { MeetingReviewPage } from './meeting-review';
import { AnnotationPage } from './annotation';
import { applyStoredMotion, focusSetting, openDisplayMenu, type Setting } from './search';
import {
  DiagnosticsPage,
  EnginePage,
  HistoryPage,
  HotkeyPage,
  LivePage,
  ModesPage,
  NS,
  TextPage,
  confirmDialog,
  deck,
  emptyState,
  fetchStatus,
  fillDeckModes,
  h,
  humanError,
  icon,
  live,
  renderLamp,
  storage,
  tip,
} from './console.js';

interface Page {
  mount(view: HTMLElement): void | Promise<void>;
  unmount?(): void;
  isDirty?(): boolean;
  themeChanged?(): void;
}

interface Route {
  id: string;
  title: string;
  icon: string;
  create(): Page;
}

declare global {
  interface Window {
    voicelessTheme(pin: string): void;
  }
}

const routes: Route[] = [
  { id: 'live', title: t('实时'), icon: 'audio-lines', create: LivePage },
  { id: 'setup', title: t('首次试说'), icon: 'mic', create: SetupPage },
  { id: 'engine', title: t('识别引擎'), icon: 'cpu', create: EnginePage },
  { id: 'words', title: t('词库'), icon: 'book-open', create: TextPage },
  { id: 'modes', title: t('模式与提示词'), icon: 'sparkles', create: ModesPage },
  { id: 'hotkey', title: t('快捷键与上屏'), icon: 'keyboard', create: HotkeyPage },
  { id: 'history', title: t('历史'), icon: 'history', create: HistoryPage },
  { id: 'jobs', title: t('文件转写'), icon: 'file-audio', create: JobsPage },
  { id: 'meetings', title: t('会议录制'), icon: 'mic', create: MeetingsPage },
  { id: 'meeting-review', title: t('会议校对'), icon: 'book-open', create: MeetingReviewPage },
  { id: 'annotation', title: t('屏幕标注'), icon: 'scan-text', create: AnnotationPage },
  { id: 'diagnostics', title: t('诊断'), icon: 'activity', create: DiagnosticsPage },
];

const view = document.getElementById('page')!;
const nav = document.getElementById('nav')!;
const themeButton = document.getElementById('theme')!;
const menuButton = document.getElementById('menu')!;
/** The settings row whose control takes the focus once its page is mounted, and that page. */
let pendingFocus: { page: string; label: string } | null = null;
let current: { id: string; page: Page } | undefined;
let navigating = false;

/** Five bars of a waveform shaped like a V. */
function brandMark() {
  const svg = document.createElementNS(NS, 'svg');
  for (const [name, value] of Object.entries({
    viewBox: '0 0 24 24', fill: 'none', stroke: 'currentColor',
    'stroke-width': '2.4', 'stroke-linecap': 'round', 'aria-hidden': 'true',
  })) {
    svg.setAttribute(name, value);
  }
  svg.innerHTML = '<path d="M3 10v4"/><path d="M7.5 7.5v9"/><path d="M12 4.5v15"/><path d="M16.5 7.5v9"/><path d="M21 10v4"/>';
  return svg;
}

const links = routes.map(({ id, title, icon: iconName }) => {
  const link = h('a', { href: `#${id}`, 'aria-label': title }, icon(iconName));
  return tip(link, title, 'r');
});

function renderTheme() {
  const theme = storage.get('theme', '');
  const label = theme === 'dark' ? t('主题：深色') : theme === 'light' ? t('主题：浅色') : t('主题：跟随系统');
  themeButton.setAttribute('aria-label', label);
  themeButton.replaceChildren(icon(theme === 'dark' ? 'moon' : theme === 'light' ? 'sun' : 'sun-moon'));
  tip(themeButton, label, 'r');
}

themeButton.addEventListener('click', () => {
  const order = ['', 'dark', 'light'];
  const next = order[(order.indexOf(storage.get('theme', '')) + 1) % order.length];
  storage.set('theme', next);
  window.voicelessTheme(next);
  renderTheme();
  deck.dirty = true;
  current?.page.themeChanged?.();
});

/** Show the page of `setting` and leave the keyboard on its control. */
async function goTo(setting: Setting) {
  if (current?.id === setting.page) {
    focusSetting(setting.label);
    return;
  }
  pendingFocus = { page: setting.page, label: setting.label };
  location.hash = `#${setting.page}`;
}

menuButton.replaceChildren(icon('sliders-horizontal'));
menuButton.addEventListener('click', async () => {
  const setting = await openDisplayMenu(renderTheme);
  if (setting) await goTo(setting);
});
window.addEventListener('keydown', (event) => {
  if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === 'k') {
    event.preventDefault();
    menuButton.click();
  }
});

async function navigate() {
  if (navigating) return;
  const hash = location.hash;
  const requested = hash.slice(1) || storage.get('page', 'live');
  const target = routes.find((route) => route.id === requested) ?? routes[0];
  if (current?.id === target.id) return;
  navigating = true;
  try {
    if (current?.page.isDirty?.()) {
      const leave = await confirmDialog(t('当前页面有未保存的更改，确定离开吗？'), {
        okText: t('放弃更改并离开'), danger: true,
      });
      if (!leave) {
        pendingFocus = null;
        history.replaceState(null, '', `#${current.id}`);
        return;
      }
    }
    current?.page.unmount?.();
    view.replaceChildren();
    document.body.dataset.page = target.id;
    deck.dirty = true;
    const page = target.create();
    current = { id: target.id, page };
    storage.set('page', target.id);
    if (location.hash === hash && hash !== `#${target.id}`) history.replaceState(null, '', `#${target.id}`);
    document.title = `${target.title} · Voiceless`;
    links.forEach((link, index) => {
      if (routes[index] === target) link.setAttribute('aria-current', 'page');
      else link.removeAttribute('aria-current');
    });
    view.focus({ preventScroll: true });
    window.scrollTo(0, 0);
    await page.mount(view);
    // A focus asked for while another page was mounting waits for its own page.
    if (pendingFocus?.page === target.id) {
      const label = pendingFocus.label;
      pendingFocus = null;
      focusSetting(label);
    }
  } catch (error) {
    console.error(error);
    view.append(emptyState('circle-alert', t(`页面加载失败：${humanError(error)}`)));
  } finally {
    navigating = false;
    if (location.hash !== hash && location.hash !== `#${current?.id}`) void navigate();
  }
}

window.addEventListener('hashchange', navigate);
window.addEventListener('beforeunload', (event) => {
  if (current?.page.isDirty?.()) {
    event.preventDefault();
    event.returnValue = '';
  }
});
document.querySelector('.skip-link')!.addEventListener('click', (event) => {
  event.preventDefault();
  view.focus();
});

/** The shell's own words (index.html), which the bundler cannot hand to t(). */
function translateShell() {
  document.querySelector('.skip-link')!.textContent = t('跳到主要内容');
  document.querySelector('.rail')!.setAttribute('aria-label', t('导航'));
  nav.setAttribute('aria-label', t('控制台'));
  document.getElementById('deck')!.setAttribute('aria-label', t('语音输入'));
  menuButton.setAttribute('aria-label', t('显示与搜索'));
  tip(menuButton, t('显示与搜索'), 'r');
}


document.getElementById('brand')!.append(brandMark());
nav.replaceChildren(...links);
document.getElementById('lamp')!.classList.add('tip-r');
renderTheme();
renderLamp();
applyStoredMotion();
translateShell();
deck.build();
// Keep keyboard scrolling outside the sticky recording strip as it changes size.
const sessionStrip = document.getElementById('deck')!;
new ResizeObserver(() => {
  document.documentElement.style.setProperty('--deck-height', `${sessionStrip.offsetHeight}px`);
}).observe(sessionStrip);
live.connect();
void fillDeckModes();
const initialStatus = fetchStatus().catch(() => null);
window.setInterval(() => { void fetchStatus().catch(() => {}); }, 15_000);
void (async () => {
  if (!location.hash && !storage.get('page', null)) {
    const status = await initialStatus;
    if (status?.configured === false && !location.hash) history.replaceState(null, '', '#setup');
  }
  await navigate();
})();
