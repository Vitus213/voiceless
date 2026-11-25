// Language of the interface. The source is written in Simplified Chinese
// and every user-facing string goes through t(); strings.en.json gives
// each string, and each template shape, its English. A string without an
// entry (an identifier, a transcript, a message from the runtime) is
// shown as it is in both languages.
//
// The language follows the browser until the user picks one from the
// display menu; the choice is kept under "voiceless.locale" and a change
// reloads the page, so no screen keeps a mixture of two languages.
import dictionary from './strings.en.json';

export type Locale = 'zh' | 'en';

/** A template shape with `{}` for each interpolated expression. */
type Shape = string;
type Dictionary = { strings: Record<Shape, string>; patterns: Record<Shape, string> };

const CHINESE = /\p{Script=Han}/u;
const STORAGE_KEY = 'voiceless.locale';
const english = dictionary as Dictionary;

function readLocale(): Locale | null {
  try {
    const value: unknown = JSON.parse(localStorage.getItem(STORAGE_KEY) ?? 'null');
    return value === 'zh' || value === 'en' ? value : null;
  } catch {
    return null;
  }
}

/** The language the interface is shown in: the user's choice, else the browser's. */
export const locale: Locale = readLocale() ?? (navigator.language.toLowerCase().startsWith('zh') ? 'zh' : 'en');

/** One template shape as a regular expression over the finished text. */
type Rule = { pattern: RegExp; english: string };

const escape = (text: string) => text.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');

/** The shapes with the most fixed text first, so that the specific one wins when two could match. */
const rules: Rule[] = Object.entries(english.patterns)
  .sort(([a], [b]) => b.replace(/\{\}/gu, '').length - a.replace(/\{\}/gu, '').length)
  .map(([shape, translation]) => ({
    pattern: new RegExp(`^${shape.split('{}').map(escape).join('(.+?)')}$`, 'su'),
    english: translation,
  }));

const cache = new Map<string, string>();

function englishOf(text: string): string {
  const direct = english.strings[text];
  if (direct !== undefined) return direct;
  for (const { pattern, english: translation } of rules) {
    const parts = pattern.exec(text);
    if (!parts) continue;
    let index = 0;
    return translation.replace(/\{\}/gu, () => parts[index++ + 1]);
  }
  return text;
}

/** `text` in the interface language. Give it the whole string: a literal, or a template with its expressions in place. */
export function t(text: string): string {
  if (locale === 'zh' || !CHINESE.test(text)) return text;
  let translated = cache.get(text);
  if (translated === undefined) cache.set(text, (translated = englishOf(text)));
  return translated;
}

/** Show the interface in `next` from now on: the page reloads with it. */
export function chooseLocale(next: Locale): void {
  try {
    localStorage.setItem(STORAGE_KEY, JSON.stringify(next));
  } catch { /* blocked storage: the choice lasts for this page only */ }
  location.reload();
}

document.documentElement.lang = locale === 'zh' ? 'zh-CN' : 'en';
