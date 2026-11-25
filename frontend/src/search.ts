// The rail's "display and search" dialog: the interface language, the
// theme and the motion, and a search over every settings row (the index
// scripts/i18n.mjs keeps) that ends with the keyboard on the row's control.
import INDEX from './settings-index.json';
import { chooseLocale, locale, t } from './i18n';
import { emptyState, h, openDialog, setChildren, storage, tiles } from './console.js';

/** One settings row of the index: where it is and how it is named. */
export type Setting = { page: string; title: string; section: string | null; label: string; tip?: string };

/** Theme pins of the rail's theme button: "" follows the system. */
const THEMES = [
  { value: '', icon: 'sun-moon', title: t('跟随系统') },
  { value: 'dark', icon: 'moon', title: t('深色') },
  { value: 'light', icon: 'sun', title: t('浅色') },
];
/** Motion choices: "" follows the system's reduced-motion setting. */
const MOTIONS = [
  { value: '', icon: 'sun-moon', title: t('跟随系统') },
  { value: 'full', icon: 'sparkles', title: t('完整') },
  { value: 'reduced', icon: 'square', title: t('减少') },
];
/** Each language is named in itself. */
const LANGUAGES = [
  { value: 'zh', icon: 'languages', title: /* i18n-literal */ '中文' },
  { value: 'en', icon: 'languages', title: 'English' },
];

const fold = (text: string) => text.toLowerCase().replace(/\s+/gu, ' ');

/** How well one word of the query fits a text: 0 when it is absent. */
function fit(word: string, text: string, name: string): number {
  const position = fold(text).indexOf(word);
  if (position < 0) return 0;
  return text === name ? 4 : position === 0 ? 3 : 1;
}

/** The settings whose label, help or section has every word of `query`, in either language; the best first. */
export function findSettings(query: string): Setting[] {
  const words = [...new Set(fold(query).trim().split(' ').filter(Boolean))];
  if (!words.length) return [];
  return INDEX.map((setting) => {
    const score = words.reduce((total, word) => total
      * Math.max(fit(word, setting.label, setting.label), fit(word, t(setting.label), setting.label),
        fit(word, setting.section ?? '', ''), fit(word, setting.tip ?? '', setting.tip ?? '')), 1);
    return { setting, score };
  })
    .filter(({ score }) => score > 0)
    .sort((a, b) => b.score - a.score)
    .slice(0, 12)
    .map(({ setting }) => setting);
}

/** Focus the control of the settings row labelled `label` on the mounted page; false when the page shows no such row. */
export function focusSetting(label: string): boolean {
  const row = document.querySelector<HTMLElement>(`#page [data-setting="${CSS.escape(t(label))}"]`);
  const control = row?.querySelector<HTMLElement>('input, select, textarea, button, [tabindex]');
  if (!control) return false;
  control.focus();
  control.scrollIntoView({ block: "center" });
  // A dialog that asked for this gives the focus back a frame after it closes: stay here.
  requestAnimationFrame(() => {
    if (!row?.contains(document.activeElement)) control.focus();
  });
  return true;
}

/** Apply the stored motion choice, or the system's, to the document element. */
export function applyStoredMotion(): void {
  const reduced = matchMedia('(prefers-reduced-motion: reduce)').matches;
  document.documentElement.dataset.motion = storage.get('motion', '') || (reduced ? 'reduced' : 'full');
}

/** The search field and its matches; `choose` ends the dialog with a setting. */
function searchBox(choose: (setting: Setting) => void) {
  const input = h('input', {
    class: 'inpt',
    type: 'search',
    placeholder: t('搜索设置'),
    'aria-label': t('搜索设置'),
    role: 'combobox',
    'aria-expanded': 'false',
    'aria-controls': 'setting-matches',
    autocomplete: 'off',
  }) as HTMLInputElement;
  const matches = h('div', { class: 'found', id: 'setting-matches', role: 'listbox', 'aria-label': t('匹配的设置') });
  let results: Setting[] = [];
  let active = 0;

  const render = () => {
    results = findSettings(input.value);
    active = 0;
    input.setAttribute('aria-expanded', String(results.length > 0));
    setChildren(matches, input.value.trim() && !results.length ? emptyState('search', t('没有匹配的设置')) : results.map((setting, index) => h(
      'div',
      {
        class: 'found-item',
        role: 'option',
        id: `setting-${index}`,
        'aria-selected': String(index === active),
        onclick: () => choose(setting),
      },
      h('b', { text: t(setting.label) }),
      h('span', { text: [setting.section ? t(setting.section) : '', t(setting.title)].filter(Boolean).join(' · ') }),
    )));
  };
  input.addEventListener('input', render);
  input.addEventListener('keydown', (event) => {
    if (event.key === 'ArrowDown' || event.key === 'ArrowUp') {
      event.preventDefault();
      if (!results.length) return;
      active = (active + (event.key === 'ArrowDown' ? 1 : results.length - 1)) % results.length;
      input.setAttribute('aria-activedescendant', `setting-${active}`);
      for (const option of matches.children) option.setAttribute('aria-selected', String(option.id === `setting-${active}`));
    } else if (event.key === 'Enter' && results.length) {
      event.preventDefault();
      choose(results[active]);
    }
  });
  render();
  return h('div', { class: 'stack' }, input, matches);
}

/** A label and its control, as the settings rows are laid out. */
function line(label: string, control: HTMLElement) {
  return h('div', { class: 'row' }, h('span', { class: 'lab', text: label }), control);
}

/**
 * Open the dialog. Resolves with the setting to go to once it is closed
 * (null when nothing was picked); `themeChanged` runs after a theme pick.
 */
export async function openDisplayMenu(themeChanged: () => void): Promise<Setting | null> {
  let chosen: Setting | null = null;
  let dialog: HTMLDialogElement | null = null;
  const choose = (setting: Setting) => {
    chosen = setting;
    dialog?.close();
  };
  const theme = tiles(THEMES, storage.get('theme', ''), (value) => {
    storage.set('theme', value);
    window.voicelessTheme(value);
    themeChanged();
  }, { small: true, label: t('主题') });
  const motion = tiles(MOTIONS, storage.get('motion', ''), (value) => {
    storage.set('motion', value);
    applyStoredMotion();
    window.dispatchEvent(new Event('voiceless-motion'));
  }, { small: true, label: t('动画') });
  const language = tiles(LANGUAGES, locale, (value) => chooseLocale(value as 'zh' | 'en'), { small: true, label: t('语言') });
  await openDialog({
    title: t('显示与搜索'),
    body: h('div', { class: 'stack' },
      searchBox(choose),
      line(t('语言'), language.el),
      line(t('主题'), theme.el),
      line(t('动画'), motion.el)),
    actions: [{ label: t('关闭'), value: '' }],
    onOpen: (element: HTMLDialogElement) => {
      dialog = element;
      element.querySelector<HTMLInputElement>('input[type=search]')?.focus();
    },
  });
  return chosen;
}
