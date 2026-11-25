// Voiceless console: a single-page app without a framework over the daemon's
// local API (docs/console-api.md).
//
// Shape of the interface: an icon rail for navigation, a sticky "deck"
// that always shows the push-to-talk key, the live waveform, the latest
// transcript and the session readouts, and one page below it. Explanations
// live in tooltips (data-tip), not in visible copy; icons come from the
// Lucide table below.
//
// Structure (top to bottom):
//                 403 = stale token, 0 = network). withToken() appends
//                 ?token= for URLs the browser loads itself. uploadFile()
//                 posts a file's bytes and reports upload progress.
//   API         - api() wraps fetch: token header, JSON in/out; a non-2xx
//                 {"error"} body becomes an ApiError (503 = no daemon,
//                 403 = stale token, 0 = network). withToken() appends
//                 ?token= for URLs the browser loads itself.
//   kit         - toasts, dialogs, buttons, rockers, faders, tiles, tag
//                 input, table editor, save bar, character diff.
//   live + deck - the one EventSource over /api/events, shared by every
//                 page, and the session strip built from it.
//   config help - saveSections() diffs a draft against its saved copy and
//                 PUTs only the changed fields. Resource changes can be
//                 queued for apply once the runtime is idle.
//   pages       - one factory per page, each returning
//                 { mount(view), unmount?(), isDirty?() }.
//   Navigation and boot live in main.ts.
//
// Token: the server substitutes it into <meta name="voiceless-token"> on
// every page load; it is read once into TOKEN and sent on every /api call.
// The server also refuses cross-origin writes, so the page needs no CSRF
// handling of its own. Vite bundles this module into the embedded app.js.
// The server also names its own system in <meta name="voiceless-platform">:
// the typing settings follow that runtime, not the browser (NATIVE below).
import { t } from './i18n';
import { historyRevisions, offlineModels } from './history-revisions';
import { exportHistory, retentionSettings, recoveryList } from './retention';

/*! Lucide icons (ISC License, Copyright (c) Lucide Icons and Contributors). */
// The SVG strings supply the shapes; icon() sets the stroke styling.
const ICONS = {
  "activity": "<path d=\"M22 12h-2.48a2 2 0 0 0-1.93 1.46l-2.35 8.36a.25.25 0 0 1-.48 0L9.24 2.18a.25.25 0 0 0-.48 0l-2.35 8.36A2 2 0 0 1 4.49 12H2\" />",
  "arrow-right": "<path d=\"M5 12h14\" /> <path d=\"m12 5 7 7-7 7\" />",
  "audio-lines": "<path d=\"M2 10v3\" /> <path d=\"M6 6v11\" /> <path d=\"M10 3v18\" /> <path d=\"M14 8v7\" /> <path d=\"M18 5v13\" /> <path d=\"M22 10v3\" />",
  "book-open": "<path d=\"M12 5v16\" /> <path d=\"M20.001 19A2 2 0 0022 17V5a2 2 0 00-1.999-2L16 3.002A5 5 0 0012 5a5 5 0 00-4-2H4a2 2 0 00-2 2v12a2 2 0 001.999 2H8a5 5 0 014 2 5 5 0 014-2z\" />",
  "bookmark": "<path d=\"M17 3a2 2 0 0 1 2 2v15a1 1 0 0 1-1.496.868l-4.512-2.578a2 2 0 0 0-1.984 0l-4.512 2.578A1 1 0 0 1 5 20V5a2 2 0 0 1 2-2z\" />",
  "bot": "<path d=\"M12 8V4H8\" /> <rect width=\"16\" height=\"12\" x=\"4\" y=\"8\" rx=\"2\" /> <path d=\"M2 14h2\" /> <path d=\"M20 14h2\" /> <path d=\"M15 13v2\" /> <path d=\"M9 13v2\" />",
  "brain": "<path d=\"M12 18V5\" /> <path d=\"M15 13a4.17 4.17 0 01-3-4 4.17 4.17 0 01-3 4\" /> <path d=\"M17.707 6.294A3 3 0 1012 5a3 3 0 10-5.707 1.294\" /> <path d=\"M17.996 5.125a4 4 0 012.526 5.77\" /> <path d=\"M18 18a4 4 0 002-7.465\" /> <path d=\"M19.967 17.482a4 4 0 11-7.967.517 4 4 0 11-7.967-.517\" /> <path d=\"M6 18a4 4 0 01-2-7.465\" /> <path d=\"M6.004 5.125a4 4 0 00-2.526 5.77\" />",
  "captions": "<rect x=\"2\" y=\"4\" width=\"20\" height=\"16\" rx=\"2\" /> <path d=\"M6 12h4m4 0h4M6 16h3m3 0h6\" />",
  "check": "<path d=\"M20 6 9 17l-5-5\" />",
  "chevron-down": "<path d=\"m6 9 6 6 6-6\" />",
  "circle-check": "<circle cx=\"12\" cy=\"12\" r=\"10\" /> <path d=\"m16 9-5.5 5.5L8 12\" />",
  "circle-x": "<circle cx=\"12\" cy=\"12\" r=\"10\" /> <path d=\"m15 9-6 6\" /> <path d=\"m9 9 6 6\" />",
  "clock": "<circle cx=\"12\" cy=\"12\" r=\"10\" /> <path d=\"M12 6v6l4 2\" />",
  "cloud-cog": "<path d=\"m10.852 19.772-.383.924\" /> <path d=\"m13.148 14.228.383-.923\" /> <path d=\"M13.148 19.772a3 3 0 1 0-2.296-5.544l-.383-.923\" /> <path d=\"m13.53 20.696-.382-.924a3 3 0 1 1-2.296-5.544\" /> <path d=\"m14.772 15.852.923-.383\" /> <path d=\"m14.772 18.148.923.383\" /> <path d=\"M4.2 15.1a7 7 0 1 1 9.93-9.858A7 7 0 0 1 15.71 8h1.79a4.5 4.5 0 0 1 2.5 8.2\" /> <path d=\"m9.228 15.852-.923-.383\" /> <path d=\"m9.228 18.148-.923.383\" />",
  "cloud": "<path d=\"M17.5 19H9a7 7 0 1 1 6.71-9h1.79a4.5 4.5 0 1 1 0 9Z\" />",
  "copy": "<rect width=\"14\" height=\"14\" x=\"8\" y=\"8\" rx=\"2\" ry=\"2\" /> <path d=\"M4 16c-1.1 0-2-.9-2-2V4c0-1.1.9-2 2-2h10c1.1 0 2 .9 2 2\" />",
  "corner-down-left": "<path d=\"M20 4v7a4 4 0 0 1-4 4H4\" /> <path d=\"m9 10-5 5 5 5\" />",
  "cpu": "<path d=\"M12 20v2\" /> <path d=\"M12 2v2\" /> <path d=\"M17 20v2\" /> <path d=\"M17 2v2\" /> <path d=\"M2 12h2\" /> <path d=\"M2 17h2\" /> <path d=\"M2 7h2\" /> <path d=\"M20 12h2\" /> <path d=\"M20 17h2\" /> <path d=\"M20 7h2\" /> <path d=\"M7 20v2\" /> <path d=\"M7 2v2\" /> <rect x=\"4\" y=\"4\" width=\"16\" height=\"16\" rx=\"2\" /> <rect x=\"8\" y=\"8\" width=\"8\" height=\"8\" rx=\"1\" />",
  "download": "<path d=\"M12 15V3\" /> <path d=\"M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4\" /> <path d=\"m7 10 5 5 5-5\" />",
  "eye-off": "<path d=\"M10.733 5.076a10.744 10.744 0 0 1 11.205 6.575 1 1 0 0 1 0 .696 10.747 10.747 0 0 1-1.444 2.49\" /> <path d=\"M14.084 14.158a3 3 0 0 1-4.242-4.242\" /> <path d=\"M17.479 17.499a10.75 10.75 0 0 1-15.417-5.151 1 1 0 0 1 0-.696 10.75 10.75 0 0 1 4.446-5.143\" /> <path d=\"m2 2 20 20\" />",
  "eye": "<path d=\"M2.062 12.348a1 1 0 0 1 0-.696 10.75 10.75 0 0 1 19.876 0 1 1 0 0 1 0 .696 10.75 10.75 0 0 1-19.876 0\" /> <circle cx=\"12\" cy=\"12\" r=\"3\" />",
  "file-audio": "<path d=\"M15 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V7zm0 0v5h5M8 14v2m4-5v8m4-6v4\" />",
  "gauge": "<path d=\"m12 14 4-4\" /> <path d=\"M3.34 19a10 10 0 1 1 17.32 0\" />",
  "hash": "<line x1=\"4\" x2=\"20\" y1=\"9\" y2=\"9\" /> <line x1=\"4\" x2=\"20\" y1=\"15\" y2=\"15\" /> <line x1=\"10\" x2=\"8\" y1=\"3\" y2=\"21\" /> <line x1=\"16\" x2=\"14\" y1=\"3\" y2=\"21\" />",
  "history": "<path d=\"M3 12a9 9 0 1 0 9-9 9.75 9.75 0 0 0-6.74 2.74L3 8\" /> <path d=\"M3 3v5h5\" /> <path d=\"M12 7v5l4 2\" />",
  "info": "<circle cx=\"12\" cy=\"12\" r=\"10\" /> <path d=\"M12 16v-4\" /> <path d=\"M12 8h.01\" />",
  "key-round": "<path d=\"M2.586 17.414A2 2 0 0 0 2 18.828V21a1 1 0 0 0 1 1h3a1 1 0 0 0 1-1v-1a1 1 0 0 1 1-1h1a1 1 0 0 0 1-1v-1a1 1 0 0 1 1-1h.172a2 2 0 0 0 1.414-.586l.814-.814a6.5 6.5 0 1 0-4-4z\" /> <circle cx=\"16.5\" cy=\"7.5\" r=\".5\" fill=\"currentColor\" />",
  "keyboard": "<path d=\"M10 8h.01\" /> <path d=\"M12 12h.01\" /> <path d=\"M14 8h.01\" /> <path d=\"M16 12h.01\" /> <path d=\"M18 8h.01\" /> <path d=\"M6 8h.01\" /> <path d=\"M7 16h10\" /> <path d=\"M8 12h.01\" /> <rect width=\"20\" height=\"16\" x=\"2\" y=\"4\" rx=\"2\" />",
  "languages": "<path d=\"m5 8 6 6\" /> <path d=\"m4 14 6-6 2-3\" /> <path d=\"M2 5h12\" /> <path d=\"M7 2h1\" /> <path d=\"m22 22-5-10-5 10\" /> <path d=\"M14 18h6\" />",
  "layers": "<path d=\"M12.83 2.18a2 2 0 0 0-1.66 0L2.6 6.08a1 1 0 0 0 0 1.83l8.58 3.91a2 2 0 0 0 1.66 0l8.58-3.9a1 1 0 0 0 0-1.83z\" /> <path d=\"M2 12a1 1 0 0 0 .58.91l8.6 3.91a2 2 0 0 0 1.65 0l8.58-3.9A1 1 0 0 0 22 12\" /> <path d=\"M2 17a1 1 0 0 0 .58.91l8.6 3.91a2 2 0 0 0 1.65 0l8.58-3.9A1 1 0 0 0 22 17\" />",
  "list-plus": "<path d=\"M16 5H3\" /> <path d=\"M11 12H3\" /> <path d=\"M16 19H3\" /> <path d=\"M18 9v6\" /> <path d=\"M21 12h-6\" />",
  "lock": "<rect width=\"18\" height=\"11\" x=\"3\" y=\"11\" rx=\"2\" ry=\"2\" /> <path d=\"M7 11V7a5 5 0 0 1 10 0v4\" />",
  "message-square-quote": "<path d=\"M14 14a2 2 0 0 0 2-2V8h-2\" /> <path d=\"M22 17a2 2 0 0 1-2 2H6.828a2 2 0 0 0-1.414.586l-2.202 2.202A.71.71 0 0 1 2 21.286V5a2 2 0 0 1 2-2h16a2 2 0 0 1 2 2z\" /> <path d=\"M8 14a2 2 0 0 0 2-2V8H8\" />",
  "mic": "<path d=\"M12 19v3\" /> <path d=\"M19 10v2a7 7 0 0 1-14 0v-2\" /> <rect x=\"9\" y=\"2\" width=\"6\" height=\"13\" rx=\"3\" />",
  "minus": "<path d=\"M5 12h14\" />",
  "moon": "<path d=\"M20.985 12.486a9 9 0 1 1-9.473-9.472c.405-.022.617.46.402.803a6 6 0 0 0 8.268 8.268c.344-.215.825-.004.803.401\" />",
  "package": "<path d=\"M11 21.73a2 2 0 0 0 2 0l7-4A2 2 0 0 0 21 16V8a2 2 0 0 0-1-1.73l-7-4a2 2 0 0 0-2 0l-7 4A2 2 0 0 0 3 8v8a2 2 0 0 0 1 1.73z\" /> <path d=\"M12 22V12\" /> <polyline points=\"3.29 7 12 12 20.71 7\" /> <path d=\"m7.5 4.27 9 5.15\" />",
  "pause": "<rect x=\"14\" y=\"3\" width=\"5\" height=\"18\" rx=\"1\" /> <rect x=\"5\" y=\"3\" width=\"5\" height=\"18\" rx=\"1\" />",
  "pencil": "<path d=\"M21.174 6.812a1 1 0 0 0-3.986-3.987L3.842 16.174a2 2 0 0 0-.5.83l-1.321 4.352a.5.5 0 0 0 .623.622l4.353-1.32a2 2 0 0 0 .83-.497z\" /> <path d=\"m15 5 4 4\" />",
  "play": "<path d=\"M5 5a2 2 0 0 1 3.008-1.728l11.997 6.998a2 2 0 0 1 .003 3.458l-12 7A2 2 0 0 1 5 19z\" />",
  "plus": "<path d=\"M5 12h14\" /> <path d=\"M12 5v14\" />",
  "refresh-cw": "<path d=\"M3 12a9 9 0 0 1 9-9 9.75 9.75 0 0 1 6.74 2.74L21 8\" /> <path d=\"M21 3v5h-5\" /> <path d=\"M21 12a9 9 0 0 1-9 9 9.75 9.75 0 0 1-6.74-2.74L3 16\" /> <path d=\"M8 16H3v5\" />",
  "replace": "<path d=\"M14 4a1 1 0 0 1 1-1\" /> <path d=\"M15 10a1 1 0 0 1-1-1\" /> <path d=\"M21 4a1 1 0 0 0-1-1\" /> <path d=\"M21 9a1 1 0 0 1-1 1\" /> <path d=\"m3 7 3 3 3-3\" /> <path d=\"M6 10V5a2 2 0 0 1 2-2h2\" /> <rect x=\"3\" y=\"14\" width=\"7\" height=\"7\" rx=\"1\" />",
  "save": "<path d=\"M15.2 3a2 2 0 0 1 1.4.6l3.8 3.8a2 2 0 0 1 .6 1.4V19a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2z\" /> <path d=\"M17 21v-7a1 1 0 0 0-1-1H8a1 1 0 0 0-1 1v7\" /> <path d=\"M7 3v4a1 1 0 0 0 1 1h7\" />",
  "search": "<circle cx=\"11\" cy=\"11\" r=\"8\" /> <path d=\"m21 21-4.3-4.3\" />",
  "scan-text": "<path d=\"M3 7V5a2 2 0 0 1 2-2h2\" /> <path d=\"M17 3h2a2 2 0 0 1 2 2v2\" /> <path d=\"M21 17v2a2 2 0 0 1-2 2h-2\" /> <path d=\"M7 21H5a2 2 0 0 1-2-2v-2\" /> <path d=\"M7 8h8\" /> <path d=\"M7 12h10\" /> <path d=\"M7 16h6\" />",
  "server": "<rect width=\"20\" height=\"8\" x=\"2\" y=\"2\" rx=\"2\" ry=\"2\" /> <rect width=\"20\" height=\"8\" x=\"2\" y=\"14\" rx=\"2\" ry=\"2\" /> <line x1=\"6\" x2=\"6.01\" y1=\"6\" y2=\"6\" /> <line x1=\"6\" x2=\"6.01\" y1=\"18\" y2=\"18\" />",
  "sliders-horizontal": "<path d=\"M10 5H3\" /> <path d=\"M12 19H3\" /> <path d=\"M14 3v4\" /> <path d=\"M16 17v4\" /> <path d=\"M21 12h-9\" /> <path d=\"M21 19h-5\" /> <path d=\"M21 5h-7\" /> <path d=\"M8 10v4\" /> <path d=\"M8 12H3\" />",
  "sparkles": "<path d=\"M11.017 2.814a1 1 0 0 1 1.966 0l1.051 5.558a2 2 0 0 0 1.594 1.594l5.558 1.051a1 1 0 0 1 0 1.966l-5.558 1.051a2 2 0 0 0-1.594 1.594l-1.051 5.558a1 1 0 0 1-1.966 0l-1.051-5.558a2 2 0 0 0-1.594-1.594l-5.558-1.051a1 1 0 0 1 0-1.966l5.558-1.051a2 2 0 0 0 1.594-1.594z\" /> <path d=\"M20 2v4\" /> <path d=\"M22 4h-4\" /> <circle cx=\"4\" cy=\"20\" r=\"2\" />",
  "square-pen": "<path d=\"M12 3H5a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h14a2 2 0 0 0 2-2v-7\" /> <path d=\"M18.375 2.625a1 1 0 0 1 3 3l-9.013 9.014a2 2 0 0 1-.853.505l-2.873.84a.5.5 0 0 1-.62-.62l.84-2.873a2 2 0 0 1 .506-.852z\" />",
  "square": "<rect width=\"18\" height=\"18\" x=\"3\" y=\"3\" rx=\"2\" />",
  "sun-moon": "<path d=\"M12 2v2\" /> <path d=\"M14.837 16.385a6 6 0 1 1-7.223-7.222c.624-.147.97.66.715 1.248a4 4 0 0 0 5.26 5.259c.589-.255 1.396.09 1.248.715\" /> <path d=\"M16 12a4 4 0 0 0-4-4\" /> <path d=\"m19 5-1.256 1.256\" /> <path d=\"M20 12h2\" />",
  "sun": "<circle cx=\"12\" cy=\"12\" r=\"4\" /> <path d=\"M12 2v2\" /> <path d=\"M12 20v2\" /> <path d=\"m4.93 4.93 1.41 1.41\" /> <path d=\"m17.66 17.66 1.41 1.41\" /> <path d=\"M2 12h2\" /> <path d=\"M20 12h2\" /> <path d=\"m6.34 17.66-1.41 1.41\" /> <path d=\"m19.07 4.93-1.41 1.41\" />",
  "target": "<circle cx=\"12\" cy=\"12\" r=\"10\" /> <circle cx=\"12\" cy=\"12\" r=\"6\" /> <circle cx=\"12\" cy=\"12\" r=\"2\" />",
  "terminal": "<path d=\"M12 19h8\" /> <path d=\"m4 17 6-6-6-6\" />",
  "text-cursor-input": "<path d=\"M12 20h-1a2 2 0 0 1-2-2 2 2 0 0 1-2 2H6\" /> <path d=\"M13 8h7a2 2 0 0 1 2 2v4a2 2 0 0 1-2 2h-7\" /> <path d=\"M5 16H4a2 2 0 0 1-2-2v-4a2 2 0 0 1 2-2h1\" /> <path d=\"M6 4h1a2 2 0 0 1 2 2 2 2 0 0 1 2-2h1\" /> <path d=\"M9 6v12\" />",
  "timer": "<line x1=\"10\" x2=\"14\" y1=\"2\" y2=\"2\" /> <line x1=\"12\" x2=\"15\" y1=\"14\" y2=\"11\" /> <circle cx=\"12\" cy=\"14\" r=\"8\" />",
  "trash-2": "<path d=\"M10 11v6\" /> <path d=\"M14 11v6\" /> <path d=\"M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6\" /> <path d=\"M3 6h18\" /> <path d=\"M8 6V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2\" />",
  "type": "<path d=\"M12 4v16\" /> <path d=\"M4 7V5a1 1 0 0 1 1-1h14a1 1 0 0 1 1 1v2\" /> <path d=\"M9 20h6\" />",
  "undo-2": "<path d=\"M9 14 4 9l5-5\" /> <path d=\"M4 9h10.5a5.5 5.5 0 0 1 5.5 5.5a5.5 5.5 0 0 1-5.5 5.5H11\" />",
  "volume-2": "<path d=\"M11 4.702a.705.705 0 0 0-1.203-.498L6.413 7.587A1.4 1.4 0 0 1 5.416 8H3a1 1 0 0 0-1 1v6a1 1 0 0 0 1 1h2.416a1.4 1.4 0 0 1 .997.413l3.383 3.384A.705.705 0 0 0 11 19.298z\" /> <path d=\"M16 9a5 5 0 0 1 0 6\" /> <path d=\"M19.364 18.364a9 9 0 0 0 0-12.728\" />",
  "wand-sparkles": "<path d=\"m21.64 3.64-1.28-1.28a1.21 1.21 0 0 0-1.72 0L2.36 18.64a1.21 1.21 0 0 0 0 1.72l1.28 1.28a1.2 1.2 0 0 0 1.72 0L21.64 5.36a1.2 1.2 0 0 0 0-1.72\" /> <path d=\"m14 7 3 3\" /> <path d=\"M5 6v4\" /> <path d=\"M19 14v4\" /> <path d=\"M10 2v2\" /> <path d=\"M7 8H3\" /> <path d=\"M21 16h-4\" /> <path d=\"M11 3H9\" />",
  "x": "<path d=\"M18 6 6 18\" /> <path d=\"m6 6 12 12\" />",
  "zap": "<path d=\"M15.914 4a1.5 1.5 0 00-2.474-1.561l-9 9A1.5 1.5 0 005.5 14h4.002a.5.5 0 01.471.666L8.086 20a1.5 1.5 0 002.475 1.56l9-9A1.5 1.5 0 0018.5 10h-3.997a.5.5 0 01-.472-.667z\" />",
};

/* ====================================================================== */
/* basics                                                                 */
/* ====================================================================== */

const NS = "http://www.w3.org/2000/svg";

/** Lucide icon as an inline <svg>; `size` in CSS pixels. */
function icon(name, size) {
  const svg = document.createElementNS(NS, "svg");
  for (const [key, value] of Object.entries({
    viewBox: "0 0 24 24",
    fill: "none",
    stroke: "currentColor",
    "stroke-width": "2",
    "stroke-linecap": "round",
    "stroke-linejoin": "round",
    "aria-hidden": "true",
  })) svg.setAttribute(key, value);
  if (size) svg.style.width = svg.style.height = `${size}px`;
  svg.innerHTML = ICONS[name] || ICONS["circle-alert"];
  return svg;
}

/** Tooltip text for an element; pos is "r" (right), "b" (below) or "t" (above). */
function tip(el, text, pos = "b") {
  if (!text) return el;
  el.dataset.tip = text;
  el.classList.add(`tip-${pos}`);
  return el;
}

/** Session token the server substituted into the page; "" if absent (API calls then 403). */
let TOKEN = document.querySelector('meta[name="voiceless-token"]')?.content || "";
/**
 * The system the serving CLI or daemon runs on ("linux", "macos" or
 * "windows"), which it names in <meta name="voiceless-platform">; "" when the
 * page did not come from it (a dev server), which then gets Linux's page.
 */
const PLATFORM = document.querySelector('meta[name="voiceless-platform"]')?.content || "";
/**
 * Windows and macOS paste through their own API and have none of the Linux
 * typing tools. Text is delivered on the runtime's system, which is why this
 * is never read off the browser.
 */
const NATIVE = PLATFORM === "windows" || PLATFORM === "macos";
/** querySelector / querySelectorAll shorthands scoped to `root`. */
const $ = (selector, root = document) => root.querySelector(selector);
const $$ = (selector, root = document) => [...root.querySelectorAll(selector)];

/**
 * Element builder: h("div", { class: "x", onclick: fn }, child, "text").
 * Attrs: class, text, value, checked, dataset, style (object) are set as
 * properties; on* functions become listeners; `true` sets a bare attribute;
 * null/false skip. Children are flattened; strings become text nodes (never
 * HTML), which is what keeps transcripts and config values safe to display.
 */
function h(tag, attrs = {}, ...children) {
  const el = document.createElement(tag);
  for (const [key, value] of Object.entries(attrs || {})) {
    if (value == null || value === false) continue;
    if (key === "class") el.className = value;
    else if (key === "text") el.textContent = value;
    else if (key === "value") el.value = value;
    else if (key === "checked") el.checked = Boolean(value);
    else if (key === "dataset") Object.assign(el.dataset, value);
    else if (key === "style" && typeof value === "object") Object.assign(el.style, value);
    else if (key.startsWith("on") && typeof value === "function") el.addEventListener(key.slice(2), value);
    else if (value === true) el.setAttribute(key, "");
    else el.setAttribute(key, String(value));
  }
  for (const child of children.flat(Infinity)) {
    if (child == null || child === false) continue;
    el.append(child instanceof Node ? child : document.createTextNode(String(child)));
  }
  return el;
}

/** localStorage under a "voiceless." prefix, JSON-encoded; silent when storage is blocked. */
const storage = {
  /** @param {string} key @param {unknown} [fallback] */
  get(key, fallback = null) {
    try {
      const value = localStorage.getItem(`voiceless.${key}`);
      return value == null ? fallback : JSON.parse(value);
    } catch {
      return fallback;
    }
  },
  set(key, value) {
    try {
      localStorage.setItem(`voiceless.${key}`, JSON.stringify(value));
    } catch { /* private mode or blocked storage: not essential */ }
  },
};

/** Computed value of a registered CSS colour token, already resolved for the active scheme. */
function cssColor(name) {
  return getComputedStyle(document.documentElement).getPropertyValue(name).trim();
}

/** Deep copy / structural equality via JSON (config sections are plain data); promise sleep. */
const clone = (value) => (value == null ? value : JSON.parse(JSON.stringify(value)));
const same = (a, b) => JSON.stringify(a) === JSON.stringify(b);
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

/** Trailing debounce with a cancel() for unmount. */
function debounce(fn, ms) {
  let timer = null;
  const wrapped = (...args) => {
    clearTimeout(timer);
    timer = setTimeout(() => fn(...args), ms);
  };
  wrapped.cancel = () => clearTimeout(timer);
  return wrapped;
}


/** Replace an element's children; null/false entries are skipped (replaceChildren would print them). */
function setChildren(el, ...children) {
  el.replaceChildren(...h("div", {}, ...children).childNodes);
}
/** Seconds -> "m:ss"; "–:––" when unknown. */
function fmtDuration(seconds) {
  if (seconds == null || Number.isNaN(seconds)) return "–:––";
  const s = Math.round(seconds);
  return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, "0")}`;
}

/** Seconds -> fixed decimals as a string; "–" when unknown. */
function fmtSeconds(seconds, digits = 1) {
  if (seconds == null || Number.isNaN(seconds)) return "–";
  return `${Number(seconds).toFixed(digits)}`;
}

/** Bytes -> "1.23 GB" / "456 MB" / "12 KB" (decimal units, matching the model catalog). */
function fmtBytes(bytes) {
  if (bytes == null) return "–";
  if (bytes >= 1e9) return `${(bytes / 1e9).toFixed(2)} GB`;
  if (bytes >= 1e6) return `${(bytes / 1e6).toFixed(0)} MB`;
  return `${(bytes / 1e3).toFixed(0)} KB`;
}

/** Seconds -> "3天 4小时" / "4小时 12分" / "12分". */
function fmtUptime(seconds) {
  if (seconds == null) return "–";
  const d = Math.floor(seconds / 86400);
  const hrs = Math.floor((seconds % 86400) / 3600);
  const min = Math.floor((seconds % 3600) / 60);
  if (d) return t(`${d}天 ${hrs}小时`);
  if (hrs) return t(`${hrs}小时 ${min}分`);
  return t(`${min}分`);
}

/** evdev name -> the short key name shown on the keycap ("ScrollLock"). */
const keyName = (evdev) => (evdev ? evdev.replace(/^KEY_/, "").replace(/_/g, " ") : "—");

/** Streaming backends in menu order: id, label, icon, one-word tag. */
const BACKENDS = [
  ["local-offline", t("本地离线"), "cpu", "GPU"],
  ["cloud-realtime", t("百炼实时"), "cloud", t("云端")],
  ["doubao-realtime", t("豆包流式"), "cloud", t("云端")],
  ["local-streaming", t("本地流式"), "activity", "CPU"],
];

/** Short label for a backend id (also used for history rows' asr_provider). */
function backendLabel(id) {
  return {
    "local-offline": t("本地离线"),
    "doubao-realtime": t("豆包"),
    "cloud-realtime": t("百炼"),
    "local-streaming": t("本地流式"),
  }[id] || id || t("未知");
}

/** Icon for a backend id. */
const backendIcon = (id) => (id === "local-offline" ? "cpu" : id === "local-streaming" ? "activity" : "cloud");

/** Model `kind` -> [label, icon]. */
const MODEL_KIND = {
  gguf: ["GPU", "cpu"],
  sherpa: ["CPU", "brain"],
  vad: [t("断句"), "audio-lines"],
  denoiser: [t("降噪"), "sparkles"],
  streaming: [t("流式"), "activity"],
};

/** BCP-47 codes of a model's `languages` -> the name the page shows; other codes show as they are. */
const LANGUAGE_LABELS = {
  zh: t("中文"), en: t("英语"), yue: t("粤语"), ar: t("阿拉伯语"), de: t("德语"), fr: t("法语"), es: t("西班牙语"), pt: t("葡萄牙语"),
  id: t("印尼语"), it: t("意大利语"), ko: t("韩语"), ru: t("俄语"), th: t("泰语"), vi: t("越南语"), ja: t("日语"), tr: t("土耳其语"),
  hi: t("印地语"), ms: t("马来语"), nl: t("荷兰语"), sv: t("瑞典语"), da: t("丹麦语"), fi: t("芬兰语"), pl: t("波兰语"), cs: t("捷克语"),
  fil: t("菲律宾语"), fa: t("波斯语"), el: t("希腊语"), ro: t("罗马尼亚语"), hu: t("匈牙利语"), mk: t("马其顿语"),
};

/** A model's languages on its row: every name up to five, then the first three and the count. */
function languageText(codes) {
  const names = codes.map((code) => LANGUAGE_LABELS[code] || code);
  return names.length > 5 ? t(`${names.slice(0, 3).join("/")} 等 ${names.length} 种语言`) : names.join("/");
}

/** Why a `kind` of model cannot run on this build, for a model that lists no device. */
const BUILD_LIMITS = {
  gguf: t("当前构建未包含 GGUF 引擎，请改用支持 GGUF 的构建（如 voiceless-vulkan）"),
};

/** Daemon session states -> label; "down" is the page's own. */
const STATE_LABELS = { idle: t("待命"), recording: t("聆听中"), stopping: t("识别中"), down: t("后台未运行") };

/** What assistive technology hears when the session enters a state; nothing for `idle`. */
const ANNOUNCED_STATE = { recording: t("开始录音"), stopping: t("正在识别") };

/** Injection methods -> label. */
const INJECT_METHODS = [
  ["fcitx5", t("输入法"), "languages"],
  ["clipboard", t("剪贴板"), "copy"],
  ["wtype", t("模拟按键"), "keyboard"],
  ["copy", t("复制到剪贴板"), "copy"],
];

/** The methods a native platform has: its own paste, and the clipboard alone; the other two are Linux tools. */
const NATIVE_METHODS = ["clipboard", "copy"];

/** The method a saved preference comes to: a native runtime pastes for every preference but an explicit copy. */
const effectivePrefer = (prefer) => (NATIVE && prefer !== "copy" ? "clipboard" : prefer);

/** What is known of a delivery -> label; keystrokes and pastes are sent, their receipt is not confirmed. */
const DELIVERY_LABELS = { committed: t("已提交"), copied: t("已复制"), unconfirmed: t("上屏未确认，请检查目标输入框") };

/**
 * How an injection ended, as one phrase: the delivery and its method, or
 * the reason nothing reached the application.
 * @param {{ ok: boolean, method: string, message: string, delivery?: string }} injection
 */
function deliveryText(injection) {
  if (!injection.ok) return injection.message || t("上屏失败");
  const method = INJECT_METHODS.find(([id]) => id === injection.method)?.[1] || injection.method;
  return `${DELIVERY_LABELS[injection.delivery] || DELIVERY_LABELS.unconfirmed} · ${method}`;
}

/* ====================================================================== */
/* API                                                                    */
/* ====================================================================== */

/** Error from api(): status 0 = network failure, else the HTTP status; message from {"error"}. */
class ApiError extends Error {
  constructor(status, message, data = null) {
    super(message);
    this.status = status;
    this.data = data;
  }
}

/** 503 = no daemon (history serve). withToken() appends ?token= for browser-loaded URLs. */
const isDaemonDown = (error) => error instanceof ApiError && error.status === 503 && error.message === "daemon not running";
const withToken = (path) => `${path}${path.includes("?") ? "&" : "?"}token=${encodeURIComponent(TOKEN)}`;

/** User-facing text for an ApiError: daemon down, stale token, else the server's message. */
function humanError(error) {
  if (isDaemonDown(error)) return t("后台进程未运行（voiceless daemon）");
  if (error instanceof ApiError && error.status === 403) return t("令牌无效，请刷新页面");
  return error.message || String(error);
}

/**
 * Every /api call: token header, JSON in and out, `{"error"}` on non-2xx
 * becomes an ApiError (and a toast unless `quiet`). A network failure is an
 * ApiError with status 0. Resolves with the parsed body (null when empty).
 * @param {string} path
 * @param {{method?: string, body?: unknown, quiet?: boolean, keepalive?: boolean}} [options]
 */
async function api(path, { method = "GET", body, quiet = false, keepalive = false } = {}) {
  const init = { method, keepalive, headers: { "X-Voiceless-Token": TOKEN } };
  if (body !== undefined) {
    init.headers["Content-Type"] = "application/json";
    init.body = JSON.stringify(body);
  }
  let response;
  try {
    response = await fetch(path, init);
  } catch {
    const error = new ApiError(0, t("无法连接控制台服务"));
    if (!quiet) toast(error.message, "err");
    throw error;
  }
  const text = await response.text();
  let data = null;
  if (text) {
    try {
      data = JSON.parse(text);
    } catch {
      data = null;
    }
  }
  if (!response.ok) {
    const error = new ApiError(response.status, (data && data.error) || `HTTP ${response.status}`, data);
    if (!quiet) toast(humanError(error), "err");
    throw error;
  }
  return data;
}

/* ====================================================================== */
/* kit                                                                    */
/* ====================================================================== */

/**
 * POST `file`'s raw bytes to `path`, outside api() because fetch reports no
 * upload progress: onProgress(loaded, total) follows the transfer. Resolves
 * like api() (the parsed body, an ApiError on failure); `request` lets the
 * caller abort it.
 */
function uploadFile(path, file, onProgress) {
  const request = new XMLHttpRequest();
  const done = new Promise((resolve, reject) => {
    request.open("POST", path);
    request.setRequestHeader("X-Voiceless-Token", TOKEN);
    request.setRequestHeader("Content-Type", "application/octet-stream");
    // Header values are ASCII: the name travels as encodeURIComponent leaves it.
    request.setRequestHeader("X-Voiceless-Filename", encodeURIComponent(file.name));
    request.upload.onprogress = (event) => {
      if (event.lengthComputable) onProgress(event.loaded, event.total);
    };
    request.onerror = () => reject(new ApiError(0, t("无法连接控制台服务")));
    request.onabort = () => reject(new ApiError(0, t("导入已取消")));
    request.onload = () => {
      let data = null;
      try {
        data = request.response ? JSON.parse(request.response) : null;
      } catch { /* not the console's JSON: the status says enough */ }
      if (request.status < 200 || request.status >= 300) {
        reject(new ApiError(request.status, (data && data.error) || `HTTP ${request.status}`, data));
      } else {
        resolve(data);
      }
    };
    request.send(file);
  });
  return { request, done };
}

const TOAST_ICON = { info: "info", ok: "check", warn: "triangle-alert", err: "circle-x" };

/** Transient notice bottom-right; kind "info" | "ok" | "warn" | "err" (err is an aria alert). */
function toast(message, kind = "info", ms = 4200) {
  const el = h("div", { class: `toast ${kind}`, role: kind === "err" ? "alert" : "status" }, icon(TOAST_ICON[kind]), h("span", { text: message }));
  $("#toasts").append(el);
  requestAnimationFrame(() => el.classList.add("in"));
  setTimeout(() => {
    el.classList.remove("in");
    setTimeout(() => el.remove(), 250);
  }, ms);
}

/**
 * Modal dialog. `actions`: [{ label, icon, value, class, close, onClick }].
 * Resolves with the value of the closing action ("" on Escape). An action
 * with `close: false` keeps the dialog open (used for "generate suggestions").
 * @param {{ title: string, body: Node, actions?: { label: string, icon?: string, value?: string, class?: string, close?: boolean, onClick?: (event: MouseEvent) => void }[], onOpen?: (dialog: HTMLDialogElement) => void }} options
 * @returns {Promise<string>}
 */
function openDialog({ title, body, actions = [], onOpen }) {
  const dialog = $("#dialog");
  const buttons = actions.map((action) =>
    h(
      "button",
      {
        type: action.close === false ? "button" : "submit",
        value: action.value ?? "",
        class: `ibtn btn ${action.class || ""}`,
        onclick: action.onClick,
      },
      action.icon ? icon(action.icon) : null,
      action.label,
    ),
  );
  dialog.replaceChildren(
    h("form", { method: "dialog", class: "dlg" }, h("h3", { text: title }), body, h("div", { class: "dlg-a" }, buttons)),
  );
  dialog.setAttribute("aria-label", title);
  dialog.returnValue = "";
  // Under a modal dialog the rest of the page is inert and exposed again on
  // close; Chrome then reports the announcer's text as inserted once more,
  // and a screen reader says the last message a second time.
  $("#announcer").replaceChildren();
  dialog.showModal();
  onOpen?.(dialog);
  return new Promise((resolve) => {
    dialog.addEventListener("close", () => resolve(dialog.returnValue), { once: true });
  });
}

/** Yes/no dialog; resolves true only for the confirming button. */
async function confirmDialog(message, { title = t("请确认"), okText = t("确定"), danger = false } = {}) {
  const result = await openDialog({
    title,
    body: h("p", { text: message }),
    actions: [
      { label: t("取消"), value: "" },
      { label: okText, icon: "check", value: "ok", class: danger ? "danger" : "primary" },
    ],
  });
  return result === "ok";
}

/** Runs `fn` with `button` in its loading state (disabled, spinner, aria-busy), restored after. */
async function busy(button, fn) {
  if (!button) return fn();
  button.disabled = true;
  button.classList.add("loading");
  button.setAttribute("aria-busy", "true");
  try {
    return await fn();
  } finally {
    button.disabled = false;
    button.classList.remove("loading");
    button.removeAttribute("aria-busy");
  }
}

/** Square icon button. `label` is its accessible name; `tip` the hover text. */
function ibtn(name, { label, tip: text, cls = "", onclick, disabled = false } = {}) {
  const button = h("button", { type: "button", class: `ibtn ${cls}`, "aria-label": label, disabled, onclick }, icon(name));
  if (text) tip(button, text);
  return button;
}

/**
 * Icon button with a short text after the icon.
 * @param {string} name
 * @param {string} text
 * @param {{cls?: string, onclick?: (event: MouseEvent) => void, disabled?: boolean, label?: string}} [options]
 */
function btn(name, text, { cls = "", onclick, disabled = false, label } = {}) {
  return h("button", { type: "button", class: `ibtn btn ${cls}`, "aria-label": label || null, onclick, disabled }, icon(name), text);
}

/** Status chip with an optional icon. */
function chip(text, cls = "", iconName) {
  return h("span", { class: `chip ${cls}` }, iconName ? icon(iconName) : null, text);
}

/** One-line status with an icon; cls "ok" | "warn" | "err". */
function note(iconName, text, cls = "") {
  return h("div", { class: `note ${cls}` }, iconName ? icon(iconName) : null, h("span", { text }));
}

/** Page title row with right-aligned icon actions. */
function pageHead(title, ...actions) {
  return h("header", { class: "page-head" }, h("h1", { text: title }), actions.length ? h("div", { class: "head-actions" }, ...actions) : null);
}

/** Icon + title heading with optional actions, above a flat group of controls. */
function group(iconName, title, actions, ...children) {
  return h(
    "section",
    { class: "group" },
    h("div", { class: "group-head" }, icon(iconName), h("h2", { text: title }), h("span", { class: "spacer" }),
      actions && actions.length ? h("div", { class: "head-actions" }, ...actions) : null),
    ...children,
  );
}

/**
 * `children` where the Linux typing tools exist, null on the native
 * platforms, which deliver through their own paste. A settings row built in
 * here is Linux's alone in the settings index too (scripts/i18n.mjs).
 */
function linuxOnly(...children) {
  return NATIVE ? null : children;
}

/** Centred placeholder for empty lists and load failures. */
function emptyState(iconName, text) {
  return h("div", { class: "empty" }, icon(iconName), text ? h("div", { text }) : null);
}

/**
 * Label + control; the tooltip hangs off the label, `cls` widens the row in
 * a `.grid.span`. The label is tied to the control's first field, so a click
 * on it focuses that field and assistive technology announces the name.
 */
function row(label, control, { tip: text, cls = "" } = {}) {
  const lab = h("label", { class: "lab", text: label });
  const field = control.matches?.("input, select, textarea") ? control : control.querySelector?.("input:not([type=checkbox]), select, textarea");
  if (field) {
    lab.htmlFor = field.id ||= `f-${Math.random().toString(36).slice(2, 9)}`;
    field.setAttribute("aria-label", label);
    if (text) field.setAttribute("aria-description", text);
  }
  return h("div", { class: cls ? `row ${cls}` : "row", dataset: { setting: label } }, tip(lab, text, "t"), control);
}

/** Switch bound to a boolean obj[key]. */
function rocker(obj, key, changed, label, { tip: text } = {}) {
  const el = h(
    "label",
    { class: "rocker" },
    h("input", {
      type: "checkbox",
      checked: Boolean(obj[key]),
      onchange: (event) => {
        obj[key] = event.target.checked;
        changed();
      },
    }),
    h("span", { class: "track", "aria-hidden": "true" }),
    h("span", { text: label }),
  );
  if (text) tip(el, text);
  return el;
}

/** Ticked slider bound to a numeric obj[key] with a live formatted readout. */
function fader(obj, key, changed, { min, max, step = 1, format = (v) => v, tip: text, label } = {}) {
  // An unset (null) optional key shows as the minimum until it is moved.
  const start = obj[key] ?? min;
  const output = h("output", { text: format(start) });
  // --p is the filled share of the track (the thumb's centre).
  const paint = () => input.style.setProperty("--p", `${((Number(input.value) - min) / (max - min)) * 100}%`);
  const input = h("input", {
    type: "range",
    min,
    max,
    step,
    value: obj[key] ?? min,
    "aria-label": label || null,
    "aria-valuetext": format(start),
    oninput: (event) => {
      obj[key] = Number(event.target.value);
      output.textContent = format(obj[key]);
      event.target.setAttribute("aria-valuetext", format(obj[key]));
      paint();
      changed();
    },
  });
  paint();
  const el = h("div", { class: "fader" }, input, output);
  if (text) tip(el, text);
  return el;
}

/** Text input bound to obj[key]; `nullable` turns a blank into null (optional config keys). */
function textInput(obj, key, changed, { placeholder = "", mono = false, nullable = false } = {}) {
  return h("input", {
    type: "text",
    class: `inpt${mono ? " mono" : ""}`,
    value: obj[key] ?? "",
    placeholder,
    spellcheck: "false",
    oninput: (event) => {
      const value = event.target.value;
      obj[key] = nullable && value.trim() === "" ? null : value;
      changed();
    },
  });
}

/** Number input bound to obj[key]; a blank becomes null when nullable, else 0. */
function numberInput(obj, key, changed, { min, max, step = 1, nullable = false, placeholder = "" } = {}) {
  return h("input", {
    type: "number",
    class: "inpt",
    min,
    max,
    step,
    placeholder,
    value: obj[key] ?? "",
    oninput: (event) => {
      const raw = event.target.value;
      if (raw === "") obj[key] = nullable ? null : 0;
      else obj[key] = Number(raw);
      changed();
    },
  });
}

/** Select bound to obj[key]. options: [[value, label, disabled]]; a null value maps to "" in the DOM. */
function selectInput(obj, key, options, changed) {
  const select = h(
    "select",
    {
      class: "inpt",
      onchange: (event) => {
        const option = options[event.target.selectedIndex];
        obj[key] = option ? option[0] : null;
        changed();
      },
    },
    options.map(([value, label, disabled]) => h("option", { value: value ?? "", disabled }, label)),
  );
  const index = options.findIndex(([value]) => (value ?? null) === (obj[key] ?? null));
  select.selectedIndex = index >= 0 ? index : 0;
  return select;
}

/**
 * Single-choice tiles. items: [{ value, icon, title, sub, tip }]. Returns
 * { el, set(value) }; onPick receives the picked value.
 * @template T
 * @param {{ value: T, icon: string, title: string, sub?: string, tip?: string, disabled?: boolean }[]} items
 * @param {T} value
 * @param {(value: T) => void} onPick
 * @param {{ small?: boolean, label?: string }} [options]
 */
function tiles(items, value, onPick, { small = false, label } = {}) {
  const el = h("div", { class: small ? "tiles sm" : "tiles", role: "group", "aria-label": label });
  const buttons = new Map();
  const render = (current) => {
    for (const [itemValue, button] of buttons) button.setAttribute("aria-pressed", String((itemValue ?? null) === (current ?? null)));
  };
  for (const item of items) {
    const button = h(
      "button",
      {
        type: "button",
        class: `tile${small ? " sm" : ""}`,
        disabled: item.disabled,
        "aria-label": item.title,
        "aria-description": item.sub || item.tip || null,
        onclick: (event) => {
          event.preventDefault();
          onPick(item.value);
          render(item.value);
        },
      },
      icon(item.icon),
      h("b", { text: item.title }),
      item.sub ? h("span", { text: item.sub }) : null,
    );
    tip(button, item.tip || item.title, "t");
    buttons.set(item.value, button);
    el.append(button);
  }
  render(value);
  return { el, set: render };
}

/**
 * Write-only secret: "" keeps the stored value, null clears it (the server's
 * PUT /api/config contract). `hasValue` is the server's has_<key> flag. The
 * lamp shows the pending outcome; the eye shows or hides what was typed;
 * the button toggles clear (null) / undo.
 */
function secretInput(obj, key, hasValue, changed) {
  if (obj[key] === undefined) obj[key] = "";
  let shown = false;
  const lamp = h("span", { class: "lampdot" });
  const input = h("input", {
    type: "password",
    class: "inpt mono",
    autocomplete: "new-password",
    placeholder: hasValue ? "••••••••" : "",
    oninput: (event) => {
      obj[key] = event.target.value;
      render();
      changed();
    },
  });
  const reveal = ibtn("eye", { label: t("显示"), cls: "ghost sm", onclick: () => { shown = !shown; render(); } });
  const clear = ibtn("x", {
    label: t("清除"),
    cls: "ghost sm",
    onclick: () => {
      input.value = "";
      obj[key] = obj[key] === null ? "" : null;
      render();
      changed();
    },
  });
  // Tooltip and accessible name carry the same words: the buttons are icon-only.
  const describe = (button, text) => {
    tip(button, text);
    button.setAttribute("aria-label", text);
  };
  function render() {
    if (obj[key] === null) {
      lamp.className = "lampdot";
      lamp.style.background = "var(--err)";
      tip(lamp, t("保存后清除"));
      describe(clear, t("撤销清除"));
    } else if (obj[key]) {
      lamp.className = "lampdot";
      lamp.style.background = "var(--sig)";
      tip(lamp, t("将更新"));
      describe(clear, t("清除"));
    } else {
      lamp.className = `lampdot ${hasValue ? "on" : "off"}`;
      lamp.style.background = "";
      tip(lamp, hasValue ? t("已设置") : t("未设置"));
      describe(clear, t("清除"));
    }
    clear.hidden = !hasValue && obj[key] !== null;
    reveal.hidden = !obj[key];
    reveal.replaceChildren(icon(shown ? "eye-off" : "eye"));
    describe(reveal, shown ? t("隐藏") : t("显示"));
    input.type = shown ? "text" : "password";
  }
  render();
  // A form keeps browsers' password heuristics quiet; it is never submitted.
  return h("form", { class: "secret", onsubmit: (event) => event.preventDefault() }, lamp, input, reveal, clear);
}

/**
 * Chip list editor for string arrays (hotwords). Enter, comma (ASCII or
 * full-width), blur and pastes containing separators add; Backspace on an
 * empty entry removes the last chip; duplicates are ignored. Returns
 * { el, set(values) }; `onChange` receives a copy of the list.
 */
function tagInput(values, onChange, placeholder = t("回车添加")) {
  let tags = [...(values || [])];
  const list = h("div", { class: "tags" });
  const entry = h("input", { class: "tag-entry", placeholder, "aria-label": placeholder });

  function emit() {
    onChange([...tags]);
  }
  function add(raw) {
    const items = raw.split(/[,，\n]/).map((item) => item.trim()).filter(Boolean);
    let added = false;
    for (const item of items) {
      if (!tags.includes(item)) {
        tags.push(item);
        added = true;
      }
    }
    entry.value = "";
    if (added) {
      render();
      emit();
      entry.focus();
    }
  }
  function render() {
    list.replaceChildren(
      ...tags.map((tag, index) =>
        h(
          "span",
          { class: "tag" },
          tag,
          h("button", {
            type: "button",
            "aria-label": t(`删除 ${tag}`),
            onclick: () => {
              tags.splice(index, 1);
              render();
              emit();
              entry.focus();
            },
          }, icon("x", 13)),
        ),
      ),
    );
  }
  entry.addEventListener("keydown", (event) => {
    if (event.key === "Enter" || event.key === "," || event.key === "，") {
      event.preventDefault();
      add(entry.value);
    } else if (event.key === "Backspace" && !entry.value && tags.length) {
      tags.pop();
      render();
      emit();
      entry.focus();
    }
  });
  entry.addEventListener("paste", (event) => {
    const text = event.clipboardData?.getData("text") || "";
    if (/[,，\n]/.test(text)) {
      event.preventDefault();
      add(text);
    }
  });
  entry.addEventListener("blur", () => {
    if (entry.value.trim()) add(entry.value);
  });
  render();
  return {
    el: h("div", {
      class: "tag-input",
      onclick: (event) => {
        if (event.target === event.currentTarget) entry.focus();
      },
    }, list, entry),
    set(next) {
      tags = [...(next || [])];
      render();
    },
  };
}

/**
 * Editable rows of text inputs (replacements, regex rules).
 * columns: [{ key, label, placeholder, mono }]. Rows are plain objects;
 * `onChange` gets a fresh copy. markInvalid/clearInvalid highlight the row
 * the server's preview error points at.
 */
function tableEditor({ columns, rows, onChange, addLabel = t("添加") }) {
  let data = (rows || []).map((row) => ({ ...row }));
  const tbody = h("tbody");
  const table = h(
    "table",
    { class: "tbl" },
    h("thead", {}, h("tr", {}, columns.map((column) => h("th", { text: column.label })), h("th", { "aria-label": t("操作") }))),
    tbody,
  );
  const empty = h("div", { class: "empty" }, icon(columns[0].icon || "list-plus"), t("空"));
  const snapshot = () => data.map((row) => ({ ...row }));

  function render() {
    tbody.replaceChildren(
      ...data.map((row, index) =>
        h(
          "tr",
          {},
          columns.map((column) =>
            h(
              "td",
              {},
              h("input", {
                class: `inpt${column.mono ? " mono" : ""}`,
                value: row[column.key] ?? "",
                placeholder: column.placeholder || "",
                spellcheck: "false",
                "aria-label": t(`${column.label}（第 ${index + 1} 行）`),
                oninput: (event) => {
                  row[column.key] = event.target.value;
                  onChange(snapshot());
                },
              }),
            ),
          ),
          h(
            "td",
            {},
            ibtn("trash-2", {
              label: t(`删除第 ${index + 1} 行`),
              cls: "ghost sm danger",
              onclick: () => {
                data.splice(index, 1);
                render();
                onChange(snapshot());
              },
            }),
          ),
        ),
      ),
    );
    table.hidden = data.length === 0;
    empty.hidden = data.length > 0;
  }

  const add = btn("plus", addLabel, { cls: "sm btn dashed" });

  add.addEventListener("click", () => {
    data.push(Object.fromEntries(columns.map((column) => [column.key, ""])));
    render();
    onChange(snapshot());
    tbody.lastElementChild?.querySelector("input")?.focus();
  });
  render();
  return {
    el: h("div", { class: "editor" }, table, empty, add),
    set(next) {
      data = (next || []).map((row) => ({ ...row }));
      render();
    },
    markInvalid(index, message) {
      $$("tr", tbody).forEach((tr, i) => {
        tr.classList.toggle("invalid", i === index);
        if (i === index) tip(tr, message, "t");
        else delete tr.dataset.tip;
      });
    },
    clearInvalid() {
      $$("tr", tbody).forEach((tr) => {
        tr.classList.remove("invalid");
        delete tr.dataset.tip;
      });
    },
  };
}

/** Floating unsaved-changes bar on <body>: set(dirty) shows/hides it; remove() on unmount. */
function saveBar({ onSave, onDiscard, message = t("未保存") }) {
  const save = ibtn("save", { label: t("保存"), cls: "primary" });
  const discard = ibtn("undo-2", { label: t("放弃更改") });
  const el = h("div", { class: "savebar", role: "region", "aria-label": t("保存更改") }, h("span", { class: "msg", text: message }), discard, save);
  save.addEventListener("click", () => busy(save, onSave));
  discard.addEventListener("click", async () => {
    if (await confirmDialog(t("放弃所有未保存的更改？"), { okText: t("放弃更改"), danger: true })) onDiscard();
  });
  document.body.append(el);
  return {
    el,
    set(dirty) {
      el.classList.toggle("show", dirty);
      el.setAttribute("aria-hidden", dirty ? "false" : "true");
    },
    remove() {
      el.remove();
    },
  };
}

/**
 * Character-level diff via an LCS table (the same walk as the server's
 * suggest_replacements): [[op, text]] with op "=", "-" or "+", runs merged.
 * Falls back to whole-string delete + insert above 4M table cells.
 */
function diffChars(a, b) {
  const A = [...(a || "")];
  const B = [...(b || "")];
  if (A.length * B.length > 4_000_000) return [["-", a], ["+", b]];
  const n = A.length;
  const m = B.length;
  const table = Array.from({ length: n + 1 }, () => new Uint16Array(m + 1));
  for (let i = n - 1; i >= 0; i--) {
    for (let j = m - 1; j >= 0; j--) {
      table[i][j] = A[i] === B[j] ? table[i + 1][j + 1] + 1 : Math.max(table[i + 1][j], table[i][j + 1]);
    }
  }
  const parts = [];
  const push = (op, ch) => {
    const last = parts[parts.length - 1];
    if (last && last[0] === op) last[1] += ch;
    else parts.push([op, ch]);
  };
  let i = 0;
  let j = 0;
  while (i < n && j < m) {
    if (A[i] === B[j]) {
      push("=", A[i]);
      i++;
      j++;
    } else if (table[i + 1][j] >= table[i][j + 1]) {
      push("-", A[i++]);
    } else {
      push("+", B[j++]);
    }
  }
  while (i < n) push("-", A[i++]);
  while (j < m) push("+", B[j++]);
  return parts;
}

/** diffChars() rendered as <del>/<ins> spans inside a .diff block. */
function renderDiff(before, after) {
  return h(
    "div",
    { class: "diff" },
    diffChars(before, after).map(([op, text]) =>
      op === "=" ? text : h(op === "-" ? "del" : "ins", { text }),
    ),
  );
}

/* ====================================================================== */
/* live connection + deck                                                 */
/* ====================================================================== */

/**
 * @typedef {{id: number, practice?: boolean, purpose?: 'dictation' | 'practice' | 'narration', phase?: string, error?: string,
 * transcript?: {is_final: boolean, authoritative_text: string}}} SessionStatus
 * @typedef {{runtime_id: string, session_id: number | null, state: string, backend: string,
 * configured?: boolean, session?: SessionStatus, engine?: {loaded: boolean, error?: string},
 * config_apply?: {active_revision: string, state: string, error?: string},
 * hotkey?: {bindings?: Array<{field: string, chord: string, keyboards: number}>,
 * portal?: {trigger?: string}}}} RuntimeStatus
 */
/** @type {{state: string | null, mode: string, status: RuntimeStatus | null}} */
const app = {
  state: null,
  mode: "quick",
  status: null,
};

/** The shared event feed. Every connection begins with a snapshot. */
const live = {
  source: null,
  listeners: new Set(),
  retry: 1000,
  runtimeId: null,
  sequence: -1,
  sessionId: null,
  reconnectTimer: null,
  connected: false,
  connect() {
    if (this.source || this.reconnectTimer !== null) return;
    const source = new EventSource(withToken("/api/events"));
    this.source = source;
    source.onmessage = (message) => {
      if (source !== this.source) return;
      let event;
      try {
        event = JSON.parse(message.data);
      } catch {
        return;
      }
      if (event.type === "snapshot") {
        const status = event.status;
        if (status.runtime_id === this.runtimeId && status.event_sequence < this.sequence) return;
        this.adoptStatus({ ...status, default_mode: event.default_mode });
        this.connected = true;
        this.retry = 1000;
      } else {
        if (event.runtime_id !== this.runtimeId || event.event_sequence <= this.sequence) return;
        this.sequence = event.event_sequence;
        if (event.type === "state") {
          this.sessionId = event.state === "idle" ? null : event.session_id;
          app.state = event.state;
          if (event.mode) app.mode = event.mode;
        }
        applyRuntimeEvent(app.status, event);
      }
      renderLamp();
      this.publish(event);
    };
    source.onerror = () => {
      if (source !== this.source) return;
      source.close();
      this.source = null;
      this.connected = false;
      if (app.state !== "down") app.state = null;
      this.publish({ type: "connection", state: app.state });
      refreshStatus();
      this.reconnectTimer = setTimeout(async () => {
        try {
          // A restarted console mints a new token. Read its shell without
          // reloading the current page or discarding unsaved drafts.
          const response = await fetch("/index.html", {
            cache: "no-store",
            signal: AbortSignal.timeout(4000),
          });
          if (response.ok) {
            const shell = new DOMParser().parseFromString(await response.text(), "text/html");
            const token = shell.querySelector('meta[name="voiceless-token"]')?.content;
            if (token) TOKEN = token;
          }
        } catch { /* The next connection attempt will report availability. */ }
        this.reconnectTimer = null;
        this.connect();
      }, this.retry);
      this.retry = Math.min(this.retry * 2, 10_000);
    };
  },
  adoptStatus(status) {
    this.runtimeId = status.runtime_id;
    this.sequence = status.event_sequence;
    this.sessionId = status.session_id;
    app.state = status.state;
    app.mode = status.mode || app.mode;
    app.status = status;
  },
  publish(event) {
    for (const listener of this.listeners) {
      try {
        listener(event);
      } catch (error) {
        console.error(error);
      }
    }
  },
  scope() {
    return { runtime_id: this.runtimeId, session_id: this.sessionId ?? undefined };
  },
  subscribe(listener) {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  },
};

function applyRuntimeEvent(status, event) {
  status.event_sequence = event.event_sequence;
  if (event.type === "paused") status.paused = event.paused;
  if (event.config_apply) status.config_apply = event.config_apply;
  if (event.hotkey) status.hotkey = event.hotkey;
  if (event.type === "config-apply" || event.type === "hotkey") return;
  if (event.type === "engine") {
    if (event.configured != null) status.configured = event.configured;
    status.engine = event.engine;
    status.backend = event.backend;
    status.fallback_backend = event.fallback_backend;
    return;
  }
  if (event.type === "state") {
    status.state = event.state;
    status.mode = event.mode;
    status.session_id = event.state === "idle" ? null : event.session_id;
    if (event.state === "recording") status.session = { id: event.session_id, config_revision: event.config_revision, practice: event.practice, purpose: event.purpose };
  }
  const session = status.session;
  if (!session || session.id !== event.session_id) return;
  switch (event.type) {
    case "state":
      session.phase = event.phase;
      session.error = event.error;
      break;
    case "backend-selected": session.backend_selection = event.backend_selection; break;
    case "devices-loaded": session.devices = event.devices; break;
    case "output":
      session.output = event.output;
      status.mode = event.output.selection.mode;
      break;
    case "ready": if (status.state === "recording") session.phase = "recording"; break;
    case "transcript": session.transcript = event.transcript; break;
    case "phase":
      session.phase = event.phase;
      if (event.phase === "no-speech") session.transcript = null;
      break;
    case "finalized":
      session.phase = "completed";
      session.injection = event.injection;
      break;
    case "cancelled": session.phase = "cancelled"; break;
    case "error":
      session.phase = "failed";
      session.error = event.message;
      break;
  }
}

/** GET /api/status into `app` and the deck; a 503 marks the daemon "down". Rethrows for callers to branch. */
async function fetchStatus() {
  try {
    const status = await api("/api/status", { quiet: true });
    if (live.connected && status.runtime_id === live.runtimeId && status.event_sequence >= live.sequence) {
      if (status.event_sequence > live.sequence) {
        live.adoptStatus(status);
        live.publish({ type: "snapshot", status });
      } else {
        app.status = status;
        deck.renderStatus(status);
      }
    }
    return live.connected ? app.status : status;
  } catch (error) {
    if (isDaemonDown(error) && !live.connected) {
      app.status = null;
      app.state = "down";
      live.publish({ type: "connection", state: app.state });
    }
    throw error;
  } finally {
    renderLamp();
  }
}

/** Debounced status refresh, fired by SSE open/error and after engine reloads. */
const refreshStatus = debounce(() => fetchStatus().catch(() => {}), 300);

/** Rail lamp from app.state; with no known state it shows the SSE connection instead. */
function renderLamp() {
  const el = $("#lamp");
  const state = app.state || "unknown";
  const label = STATE_LABELS[state] || (live.connected ? t("已连接") : t("未连接"));
  el.dataset.state = state;
  el.dataset.tip = label;
  el.setAttribute("aria-label", label);
}

/** Whether the user or the system asked for less motion: the waveform then stays still. */
const motionReduced = () => document.documentElement.dataset.motion === "reduced";
window.addEventListener("voiceless-motion", () => { deck.dirty = true; });

/**
 * Text for assistive technology. The page keeps one polite live region and
 * only session phases and results go through it: the live transcript and
 * the waveform are not announced.
 * @param {string} message
 */
function announce(message) {
  if (!message) return;
  const region = $("#announcer");
  // Emptied first, so that the same words are spoken again the next time.
  region.replaceChildren();
  requestAnimationFrame(() => region.append(message));
}

/** Suspend or resume dictation on the daemon; the paused event brings the new state back. */
async function setPaused(paused) {
  try {
    await api("/api/session/pause", { method: "POST", body: { paused }, quiet: true });
  } catch { /* toast shown */ }
  fetchStatus().catch(() => {});
}

/** Icon + big number + unit, used for the session readouts. */
function readout(iconName, label) {
  const value = h("b", { text: "–" });
  const unit = h("small");
  const el = tip(h("span", { class: "ro" }, icon(iconName), value, unit), label, "b");
  return {
    el,
    set(next, unitText = "") {
      value.textContent = next ?? "–";
      unit.textContent = unitText;
    },
  };
}

/**
 * The deck: push-to-talk key, rolling waveform, latest transcript, mode
 * picker and session readouts. It lives above every page and follows the
 * shared event stream, so a session can be started and watched anywhere.
 */
const deck = {
  els: {},
  bars: new Float32Array(220),
  level: 0,
  target: 0,
  tick: 0, // frame parity: the history scrolls on every second frame
  frame: null,
  dirty: true, // one more paint is due (resize, theme, page change)
  state: null,
  previous: null,
  /** Whether the daemon is paused, as last rendered. */
  paused: false,
  /** Confirmed text of the current session, announced with its outcome. */
  text: "",
  hotkey: null,
  unsubscribe: null,

  build() {
    const e = this.els;
    e.ptt = h("button", { type: "button", class: "ptt", onclick: () => this.toggle() });
    e.canvas = h("canvas", { class: "ribbon", "aria-hidden": "true" });
    e.ticker = h("div", { class: "ticker", "aria-label": t("识别文字") });
    e.cancel = ibtn("x", {
      label: t("取消"),
      cls: "danger lg",
      onclick: (event) => {
        if (!live.connected || live.sessionId === null) return;
        const body = live.scope();
        return busy(event.currentTarget, () => api("/api/session/cancel", { method: "POST", body }).catch(() => {}));
      },
    });
    /** Suspend every start until resumed (the hotkey included); what is in progress finishes. */
    e.pause = ibtn("pause", {
      label: t("暂停听写"),
      cls: "ghost",
      onclick: (event) => busy(event.currentTarget, () => setPaused(!app.status?.paused)),
    });
    e.repaste = ibtn("corner-down-left", {
      label: t("重贴上次结果"),
      cls: "ghost",
      onclick: (event) => busy(event.currentTarget, () => api("/api/session/repaste", { method: "POST", quiet: true }).catch(() => {})),
    });
    e.mode = h("select", {
      class: "inpt",
      "aria-label": t("模式"),
      style: { width: "auto" },
      onchange: () => storage.set("liveMode", e.mode.value),
    });
    e.roFinal = readout("timer", t("松手到出字"));
    e.roAudio = readout("clock", t("语音时长"));
    e.roChars = readout("type", t("字数"));
    $("#deck").append(
      e.ptt,
      e.canvas,
      e.ticker,
      h("div", { class: "deck-side" },
        h("div", { class: "deck-tools" }, e.mode, e.pause, e.repaste, e.cancel),
        h("div", { class: "readouts" }, e.roFinal.el, e.roAudio.el, e.roChars.el)),
    );
    this.setText("", "");
    this.setState(null);
    this.showSession(null);
    this.unsubscribe = live.subscribe((event) => this.onEvent(event));
    this.frame = requestAnimationFrame(() => this.animate());
    window.addEventListener("resize", () => { this.dirty = true; });
  },

  /** Start (with the picked mode) or stop, depending on the current state. */
  async toggle() {
    if (!live.connected) return;
    const mode = this.els.mode.value;
    storage.set("liveMode", mode);
    const recording = this.state === "recording";
    await busy(this.els.ptt, async () => {
      try {
        const body = recording ? live.scope() : { mode, runtime_id: live.runtimeId };
        await api(recording ? "/api/session/stop" : "/api/session/start", { method: "POST", body });
      } catch { /* toast already shown */ }
    });
    this.setState(this.state);
    fetchStatus().catch(() => {});
  },

  /** Reflect a session state in the key, the cancel button and the mode picker. */
  setState(next) {
    this.state = next;
    const e = this.els;
    const down = next === "down";
    const recording = next === "recording";
    const stopping = next === "stopping";
    e.ptt.setAttribute("aria-pressed", String(recording));
    e.ptt.classList.toggle("busy", stopping);
    this.paused = Boolean(app.status?.paused);
    e.ptt.disabled = down || stopping || next == null || this.paused;
    e.ptt.replaceChildren(
      icon(recording ? "square" : stopping ? "refresh-cw" : "mic"),
      h("span", { class: "k" }, this.hotkey ? keyName(this.hotkey) : "PTT"),
    );
    tip(e.ptt, recording ? t("停止并识别") : this.paused ? t("听写已暂停") : down ? t("后台未运行") : t("开始录音"), "r");
    e.ptt.setAttribute("aria-label", recording ? t("停止并识别") : t("开始录音"));
    const pauseLabel = this.paused ? t("继续听写") : t("暂停听写");
    e.pause.replaceChildren(icon(this.paused ? "play" : "pause"));
    e.pause.setAttribute("aria-pressed", String(this.paused));
    e.pause.setAttribute("aria-label", pauseLabel);
    tip(e.pause, pauseLabel, "r");
    e.cancel.hidden = !(recording || stopping);
    e.mode.disabled = next !== "idle";
    if (!recording) this.target = 0;
    if (recording && this.previous !== "recording") this.setText("", "");
    // Losing the runtime is named once here: the lamp used to say it, and
    // was spoken over the phases.
    if (down && this.previous !== "down") announce(t("后台未运行"));
    this.previous = next;
    renderLamp();
  },

  /** Engine, hotkey and the session's mode from /api/status. */
  renderStatus(status) {
    if (status.state && status.state !== this.state) this.setState(status.state);
    else if (Boolean(status.paused) !== this.paused) this.setState(this.state);
    const shortcut = status.hotkey?.bindings?.find((binding) => binding.keyboards && ["hold_key", "toggle_key", "key"].includes(binding.field));
    const key = shortcut?.chord ?? status.hotkey?.key ?? null;
    if (key !== this.hotkey) {
      this.hotkey = key;
      this.setState(this.state);
    }
    this.showSession(status.last_session);
  },

  /** Latest transcript in the ticker. */
  setText(confirmed, partial, placeholder = false) {
    this.text = confirmed;
    setChildren(this.els.ticker,
      (placeholder || (!confirmed && !partial)) ? h("span", { class: "placeholder", text: t("按下快捷键，说的话会出现在这里") }) : null,
      h("span", { class: "confirmed", text: confirmed }),
      partial ? h("span", { class: "partial", text: partial }) : null,
    );
  },

  /** Last-session stats: stop-to-text ms, audio seconds, characters. */
  showSession(session) {
    const e = this.els;
    if (!session) {
      e.roFinal.set("–");
      e.roAudio.set("–");
      e.roChars.set("–");
      return;
    }
    e.roFinal.set(session.stop_to_final_ms != null ? String(Math.round(session.stop_to_final_ms)) : "–", "ms");
    e.roAudio.set(fmtSeconds(session.audio_seconds), "s");
    e.roChars.set(session.chars != null ? String(session.chars) : "–", t("字"));
  },

  /** SSE dispatcher: state, transcript, level and session events reach the deck. */
  onEvent(event) {
    switch (event.type) {
      case "snapshot": {
        this.renderStatus(app.status);
        const transcript = event.status.session?.transcript;
        const confirmed = transcript?.is_final ? transcript.authoritative_text : (transcript?.confirmed_segments || []).join("");
        this.setText(confirmed, transcript?.partial_text || "");
        break;
      }
      case "connection":
        this.setState(event.state);
        break;
      case "state":
        this.setState(event.state);
        announce(ANNOUNCED_STATE[event.state] ?? "");
        break;
      case "transcript": {
        const transcript = event.transcript || {};
        const confirmed = transcript.is_final ? transcript.authoritative_text || "" : (transcript.confirmed_segments || []).join("");
        const partial = transcript.is_final ? "" : transcript.partial_text || "";
        this.setText(confirmed, partial, !confirmed && !partial);
        break;
      }
      case "phase":
        if (event.phase === "no-speech") {
          this.setText("", "");
          announce(t("没有识别到语音"));
        }
        break;
      case "level":
        this.target = Math.min(1, Math.pow(Math.max(0, event.rms || 0) * 3, 0.6));
        break;
      case "session":
        this.showSession(event);
        break;
      case "paused":
        this.setState(this.state);
        announce(event.paused ? t("听写已暂停") : t("听写已继续"));
        break;
      case "finalized": {
        // The result, then what became of it: spoken once, after the final text.
        const injection = event.injection;
        // A practice never types: its outcome is that it is over.
        const outcome = app.status?.session?.practice ? t("试说完成") : injection == null ? t("未上屏") : deliveryText(injection);
        announce([this.text, outcome].filter(Boolean).join("，"));
        break;
      }
      case "cancelled":
        announce(t("已取消"));
        break;
      case "error":
        announce(t(`出错：${event.message}`));
        break;
      default:
        break;
    }
  },

  /**
   * rAF loop: fast attack, slow release, a history that scrolls while
   * recording. Once the trace has drained and nothing is recording, the
   * canvas is repainted only when `dirty` (resize, theme, page change).
   */
  animate() {
    const active = !motionReduced() && (this.state === "recording" || this.level > 0.002 || this.bars.some((bar) => bar > 0.002));
    if (active || this.dirty) {
      this.dirty = false;
      this.level += (this.target - this.level) * (this.target > this.level ? 0.5 : 0.12);
      this.target *= 0.92;
      this.tick = (this.tick + 1) % 2;
      if (this.tick === 0) {
        this.bars.copyWithin(0, 1);
        this.bars[this.bars.length - 1] = this.state === "recording" ? this.level : 0;
      }
      this.paint();
    }
    this.frame = requestAnimationFrame(() => this.animate());
  },

  /** The history as mirrored strokes: newest in amber, older fading to the cool trace. */
  paint() {
    const canvas = this.els.canvas;
    const dpr = window.devicePixelRatio || 1;
    const width = canvas.clientWidth;
    const height = canvas.clientHeight;
    if (!width || !height) return;
    if (canvas.width !== Math.round(width * dpr) || canvas.height !== Math.round(height * dpr)) {
      canvas.width = Math.round(width * dpr);
      canvas.height = Math.round(height * dpr);
    }
    const ctx = canvas.getContext("2d");
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    ctx.clearRect(0, 0, width, height);
    const sig = cssColor("--sig");
    const cool = cssColor("--cool");
    const bars = this.bars;
    const n = bars.length;
    const step = width / n;
    const mid = height / 2;
    ctx.lineWidth = Math.max(1.5, step * 0.55);
    ctx.lineCap = "round";
    for (let i = 0; i < n; i++) {
      const x = i * step + step / 2;
      const age = (n - 1 - i) / n;
      const v = bars[i];
      ctx.beginPath();
      if (v < 0.012) {
        ctx.globalAlpha = 0.35;
        ctx.strokeStyle = cool;
        ctx.moveTo(x, mid - 0.75);
        ctx.lineTo(x, mid + 0.75);
      } else if (age < 0.3) {
        ctx.globalAlpha = 1 - age * 1.6;
        ctx.strokeStyle = sig;
        const hh = 2 + v * (mid - 3);
        ctx.moveTo(x, mid - hh);
        ctx.lineTo(x, mid + hh);
      } else {
        ctx.globalAlpha = (1 - age) * 0.9;
        ctx.strokeStyle = cool;
        const hh = 2 + v * (mid - 3);
        ctx.moveTo(x, mid - hh);
        ctx.lineTo(x, mid + hh);
      }
      ctx.stroke();
    }
    ctx.globalAlpha = 1;
  },
};

/** Modes sorted by sort_order; falls back to the built-in "快速" entry when the API fails. */
let modesCache = null;
async function getModes() {
  if (modesCache) return modesCache;
  try {
    const modes = await api("/api/modes", { quiet: true });
    modesCache = [...modes].sort((a, b) => (a.sort_order ?? 0) - (b.sort_order ?? 0));
  } catch {
    modesCache = [{ id: "quick", name: t("快速"), builtin: true }];
  }
  return modesCache;
}

/** Mode name for a history row ("quick" -> "快速输入"); the id itself for an unknown or deleted mode. */
function modeName(id) {
  return (modesCache || []).find((mode) => mode.id === id)?.name || id;
}

/** Fill the deck's mode picker once the modes are known. */
async function fillDeckModes() {
  const modes = await getModes();
  const select = deck.els.mode;
  select.replaceChildren(
    h("option", { value: "" }, t("按应用自动选择")),
    ...modes.map((mode) => h("option", { value: mode.id }, mode.name || mode.id)),
  );
  const remembered = storage.get("liveMode", null);
  if (remembered && modes.some((mode) => mode.id === remembered)) select.value = remembered;
}

/* ====================================================================== */
/* shared helpers for config pages                                        */
/* ====================================================================== */

function configFields(saved, draft, sections) {
  const fields = {};
  for (const section of sections) {
    for (const [key, value] of Object.entries(draft[section] || {})) {
      if (!key.startsWith("has_") && !same(saved[section]?.[key], value)) {
        fields[`${section}.${key}`] = value ?? null;
      }
    }
  }
  return fields;
}

function compareConfigConflicts(config, fields, conflicts) {
  const text = (value) => value == null ? t("默认") : typeof value === "string" ? value : JSON.stringify(value);
  return h("div", { class: "stack" }, conflicts.map((path) => {
    const [section, key] = path.split(".");
    const current = config[section] || {};
    const presence = current[`has_${key}`];
    const isSecret = typeof presence === "boolean";
    const savedValue = isSecret ? (presence ? t("已保存凭据") : t("未设置")) : text(current[key]);
    const draftValue = isSecret ? (fields[path] === null ? t("清除凭据") : t("替换凭据")) : text(fields[path]);
    return h("div", {}, h("code", { text: path }), h("dl", { class: "kv" },
      h("dt", { text: t("当前值") }), h("dd", { text: savedValue }),
      h("dt", { text: t("我的更改") }), h("dd", { text: draftValue }),
    ));
  }));
}

async function saveSections(saved, draft, sections) {
  const fields = configFields(saved, draft, sections);
  let revision = saved.revision;
  while (Object.keys(fields).length) {
    try {
      return await api("/api/config", { method: "PUT", body: { revision, fields }, quiet: true });
    } catch (error) {
      const conflict = error.data;
      if (!(error instanceof ApiError && error.status === 409 && conflict?.conflicts?.length)) {
        toast(humanError(error), "err");
        throw error;
      }
      const choice = await openDialog({
        title: t("设置已被修改"),
        body: compareConfigConflicts(conflict.config, fields, conflict.conflicts),
        actions: [
          { label: t("保留草稿"), value: "" },
          { label: t("采用当前值"), value: "current" },
          { label: t("用我的值覆盖"), value: "overwrite", class: "primary" },
        ],
      });
      if (!choice) return null;
      if (choice === "current") {
        for (const path of conflict.conflicts) delete fields[path];
      }
      revision = conflict.revision;
    }
  }
  return { restart_engine: false };
}

function downloadText(name, text, type) {
  const url = URL.createObjectURL(new Blob([text], { type }));
  h("a", { href: url, download: name }).click();
  setTimeout(() => URL.revokeObjectURL(url), 0);
}

async function exportSettings(button) {
  await busy(button, async () => {
    try {
      const { toml } = await api("/api/config/template");
      downloadText("voiceless-config.toml", toml, "application/toml");
    } catch { /* toast already shown */ }
  });
}

async function offerEngineReload(revision) {
  if (app.state === "down") return;
  const choice = await openDialog({
    title: t("应用设置"),
    body: h("p", { text: t("应用已保存的设置。录音进行中时，会等它结束。") }),
    actions: [
      { label: t("稍后"), value: "" },
      { label: t("应用"), icon: "refresh-cw", value: "apply", class: "primary" },
    ],
  });
  if (choice === "apply") await reloadEngine(null, revision);
}

async function reloadEngine(button, revision) {
  return busy(button, async () => {
    try {
      const body = revision ? { revision } : undefined;
      await api("/api/engine/reload", { method: "POST", body });
      toast(t("已提交应用请求"), "ok");
      refreshStatus();
    } catch { /* toast shown */ }
  });
}

async function restoreSettings(button, revision, onRestore) {
  if (!(await confirmDialog(t("用正在运行的配置替换已保存的设置？此页面的草稿也会丢弃。"), { okText: t("恢复") }))) return;
  await busy(button, async () => {
    try {
      await api("/api/config/restore", { method: "POST", body: { revision } });
      await onRestore?.();
      toast(t("已恢复正在运行的设置"), "ok");
      refreshStatus();
    } catch { /* toast shown */ }
  });
}

function configApplyDetails(status, savedRevision, onRestore) {
  const apply = status?.config_apply;
  if (!apply) return null;
  const revision = savedRevision ?? apply.target_revision;
  const pending = apply.state === "queued" || apply.state === "loading";
  const failed = apply.state === "failed";
  const changed = revision && revision !== apply.active_revision;
  const message = pending ? (apply.state === "queued" ? t("等待空闲后应用设置") : t("正在验证设置"))
    : failed ? t(`应用失败 · ${apply.error}`)
    : changed ? t("设置已保存，等待应用")
    : t("设置已生效");
  return h("div", { class: "stack" },
    h("div", { role: "status" }, note(failed ? "circle-x" : changed || pending ? "clock" : "circle-check", message, failed ? "err" : "")),
    !pending && (changed || failed) ? h("div", { class: "inline" },
      ibtn("refresh-cw", { label: failed ? t("重试应用") : t("应用设置"), onclick: (event) => reloadEngine(event.currentTarget, revision) }),
      changed ? ibtn("undo-2", { label: t("恢复运行中的设置"), onclick: (event) => restoreSettings(event.currentTarget, revision, onRestore) }) : null,
    ) : null,
  );
}

/** Secrets come back as "" plus has_<key>; keep "" in the draft (= unchanged). */
function secretFlag(section, key) {
  return Boolean(section?.[`has_${key}`]);
}

/** "Load failed" block for a page whose config could not be read. */
function loadFailed(error) {
  return emptyState("circle-alert", t(`读取失败：${humanError(error)}`));
}

/* ====================================================================== */
/* page: 实时                                                              */
/* ====================================================================== */

/**
 * Live page: the session transcript as a sheet under a state bar with a
 * recording clock, the result note, the engine strip and the daemon log.
 * Subscribes to the SSE stream; polls /api/status every 5 s for engine and GPU.
 */
function LivePage() {
  const stateLabel = h("span", { text: t("待命") });
  const clock = h("span", { class: "mono" });
  const body = h("div", { class: "sheet-body" });
  const els = {
    sheet: h("div", { class: "sheet", "aria-label": t("识别文字") }),
    note: h("div", { class: "note" }),
    engine: h("div"),
    output: h("div", { class: "stack" }),
    log: h("pre", { class: "logbox", tabindex: "0", "aria-label": t("实时日志") }),
  };
  let unsubscribe = null;
  let statusTimer = null;
  let clockTimer = null;
  let startedAt = 0; // recording start, for the clock in the sheet bar
  let logLines = [];
  let shownState; // the last state showState() rendered

  function setNote(iconName, text, cls = "") {
    els.note.className = `note ${cls}`;
    setChildren(els.note, iconName ? icon(iconName) : null, h("span", { text }));
  }

  /** Render a transcript snapshot: confirmed segments + underlined partial until is_final. */
  function showTranscript(snapshot) {
    const final = Boolean(snapshot.is_final);
    let confirmed = Array.isArray(snapshot.confirmed_segments) ? snapshot.confirmed_segments.join("") : "";
    let partial = snapshot.partial_text || "";
    if (final) {
      confirmed = snapshot.authoritative_text || confirmed + partial;
      partial = "";
    } else if (!confirmed && !partial) {
      confirmed = snapshot.authoritative_text || "";
    }
    els.sheet.classList.toggle("final", final);
    setChildren(body,
      h("span", { class: "confirmed", text: confirmed }),
      partial ? h("span", { class: "partial", text: partial }) : null,
    );
    els.sheet.scrollTop = els.sheet.scrollHeight;
  }

  function resetTranscript() {
    els.sheet.classList.remove("final");
    body.replaceChildren(h("span", { class: "placeholder", text: t("正在聆听…") }));
    setNote("", "");
    els.output.replaceChildren();
  }

  /** Session state on the sheet bar; the clock runs from the recording start until the next state. */
  function showState(state) {
    if (state === shownState) return;
    shownState = state;
    const recording = state === "recording";
    els.sheet.classList.toggle("rec", recording);
    stateLabel.textContent = { idle: t("待命"), recording: t("正在聆听"), stopping: t("识别中"), down: t("后台未运行") }[state] || t("连接中…");
    clearInterval(clockTimer);
    startedAt = recording ? Date.now() : 0;
    const tick = () => { clock.textContent = recording ? fmtDuration((Date.now() - startedAt) / 1000) : ""; };
    clockTimer = recording ? setInterval(tick, 250) : null;
    tick();
  }

  /** Append a log line (keep 300); auto-scroll only when the view was already at the bottom. */
  function appendLog(line) {
    logLines.push(line);
    if (logLines.length > 300) logLines = logLines.slice(-300);
    const atBottom = els.log.scrollHeight - els.log.scrollTop - els.log.clientHeight < 30;
    els.log.textContent = logLines.join("\n");
    if (atBottom) els.log.scrollTop = els.log.scrollHeight;
  }

  function showPhase(phase, injection, error) {
    if (phase === "processing") setNote("sparkles", t("正在处理文字"));
    else if (phase === "delivering") setNote("corner-down-left", t("正在上屏"));
    else if (phase === "no-speech") setNote("mic-off", t("没有识别到语音"), "warn");
    else if (phase === "cancelled") setNote("x", t("已取消，没有上屏"), "warn");
    else if (phase === "failed") setNote("circle-x", error || t("识别失败"), "err");
    else if (phase === "completed") {
      if (injection == null) setNote("check", t("完成，未上屏"));
      else if (injection.ok) setNote(injection.delivery === "committed" ? "check" : "corner-down-left", deliveryText(injection), "ok");
      else setNote("circle-x", deliveryText(injection), "err");
    }
  }

  function showSnapshot(status) {
    const session = status.session;
    showState(app.state);
    if (session?.transcript) showTranscript(session.transcript);
    else {
      resetTranscript();
      if (status.state === "idle") body.replaceChildren();
    }
    showPhase(session?.phase, session?.injection, session?.error);
    els.output.replaceChildren(...(session?.output ? [outputTrace(session.output)] : []));
  }

  /** SSE dispatcher for this page: one case per event type in docs/console-api.md. */
  function onEvent(event) {
    if (recognitionChanged(event) && app.status) renderEngine(app.status);
    switch (event.type) {
      case "snapshot":
        showSnapshot(event.status);
        break;
      case "connection":
        showState(event.state);
        break;
      case "state":
        showState(event.state);
        if (event.state === "recording") resetTranscript();
        if (event.phase === "failed") showPhase(event.phase, null, event.error);
        break;
      case "ready":
        if (app.state === "recording") setNote("mic", t("麦克风已就绪"));
        break;
      case "transcript":
        if (event.transcript) showTranscript(event.transcript);
        break;
      case "output":
        els.output.replaceChildren(outputTrace(event.output));
        break;
      case "phase":
        showPhase(event.phase);
        break;
      case "finalized":
        showPhase("completed", event.injection);
        break;
      case "warning":
        setNote("triangle-alert", event.message === "no speech text was recognized" ? t("没有识别到语音") : event.message, "warn");
        break;
      case "error":
        setNote("circle-x", event.message, "err");
        toast(t(`识别出错：${event.message}`), "err");
        break;
      case "cancelled":
        setNote("x", t("已取消，没有上屏"), "warn");
        break;
      case "log":
        appendLog(event.line);
        break;
      default:
        break;
    }
  }

  function renderEngine(status) {
    const gpu = status.gpu || [];
    setChildren(els.engine, recognitionDetails(status), configApplyDetails(status),
      gpu.length ? h("div", { class: "stack" }, ...gpu.map(gpuBar)) : null);
  }

  /** Poll /api/status; a 503 flips the page into its "daemon down" presentation. */
  async function refresh() {
    try {
      const status = await fetchStatus();
      renderEngine(status);
      showState(app.state);
    } catch (error) {
      if (isDaemonDown(error)) els.engine.replaceChildren(note("circle-alert", t("运行 voiceless daemon 开启实时听写")));
    }
  }
  return {
    async mount(view) {
      resetTranscript();
      els.sheet.append(h("div", { class: "sheet-bar" }, h("i", { class: "rec-dot" }), stateLabel, h("span", { class: "spacer" }), clock), body);
      const fold = h("details", { class: "fold" }, h("summary", {}, icon("chevron-down"), t("日志")), h("div", { class: "fold-body" }, els.log));
      view.append(
        pageHead(t("实时"),
          ibtn("terminal", { label: t("日志"), cls: "ghost", onclick: () => { fold.open = !fold.open; } }),
          ibtn("refresh-cw", { label: t("重新加载引擎"), cls: "ghost", onclick: (event) => reloadEngine(event.currentTarget).then(refresh) })),
        els.sheet,
        els.note,
        els.output,
        group("cpu", t("引擎"), null, els.engine),
        fold,
      );
      unsubscribe = live.subscribe(onEvent);
      if (app.status) showSnapshot(app.status);
      live.connect();
      await refresh();
      statusTimer = setInterval(refresh, 5000);
      try {
        const logs = await api("/api/logs", { quiet: true });
        logLines = (logs.lines || []).slice(-300);
        els.log.textContent = logLines.join("\n");
      } catch { /* daemon down: the note explains */ }
    },
    unmount() {
      unsubscribe?.();
      clearInterval(clockTimer);
      clearInterval(statusTimer);
    },
  };
}

const COMPUTE_DEVICES = { auto: t("自动"), cpu: "CPU", cuda: "CUDA", vulkan: "Vulkan" };
const CPU_FALLBACK = {
  build_without_gpu: t("当前构建未包含 CUDA 或 Vulkan"),
  no_gpu_device: t("未找到有可用驱动的 GPU"),
  gpu_initialization_failed: t("GPU 初始化失败"),
};

function recognitionChanged(event) {
  switch (event.type) {
    case "snapshot": case "state": case "backend-selected": case "devices-loaded": case "engine": case "config-apply": case "error":
      return true;
    default:
      return false;
  }
}

function effectiveRecognition(status) {
  const session = status.session;
  const selected = session?.backend_selection;
  const engine = status.engine || {};
  return {
    backend: selected?.backend ?? (engine.loaded ? status.backend : null),
    model: selected?.model ?? engine.model,
    requested: selected ? selected.requested_device : engine.requested_device,
    devices: selected ? session.devices : engine.devices,
    fallback: selected?.fallback,
    error: session?.error || engine.error,
  };
}

function deviceDescription(device) {
  const api = COMPUTE_DEVICES[device.api] ?? device.backend;
  return device.description && device.description !== api ? `${api} · ${device.description}` : api;
}

function recognitionDetails(status) {
  if (!status) return note("circle-alert", t("等待读取后台状态"));
  const recognition = effectiveRecognition(status);
  const { backend, model, requested, devices, fallback, error } = recognition;
  const rows = [[t("配置后端"), backendLabel(status.backend)]];
  if (backend) rows.push([status.session_id == null ? t("有效后端") : t("当前后端"), backendLabel(backend)]);
  if (model) rows.push([t("识别模型"), model]);
  if (requested) rows.push([t("请求设备"), COMPUTE_DEVICES[requested] ?? requested]);
  if (requested) rows.push([t("最终识别设备"), devices ? deviceDescription(devices.primary) : error ? t("加载失败") : t("等待加载")]);
  if (devices?.preview) rows.push([t("快速预览设备"), `SenseVoice · ${deviceDescription(devices.preview)}`]);
  const computeFallback = CPU_FALLBACK[devices?.primary?.cpu_fallback];
  return h("div", { class: "stack" },
    h("dl", { class: "kv" }, rows.map(([label, value]) => [h("dt", { text: label }), h("dd", { text: value })])),
    computeFallback ? note("triangle-alert", t(`计算设备回退到 CPU（${computeFallback}）`), "warn") : null,
    fallback ? note("triangle-alert", t(`${backendLabel(fallback.primary)} 启动失败（${fallback.reason}），已切换到 ${backendLabel(backend)}`), "warn") : null,
    error ? note("circle-x", error, "err") : null,
  );
}

/** One GPU row: name, used/total and a meter bar. */
function gpuBar(gpu) {
  const used = gpu.memory_used_mib ?? 0;
  const total = gpu.memory_total_mib || 1;
  return h(
    "div",
    { class: "inline" },
    icon("cpu", 16),
    h("span", { text: gpu.name || "GPU", style: { flex: 1, minWidth: 0, overflowWrap: "break-word" } }),
    h("div", { class: "bar", style: { flex: "1 1 120px" }, role: "meter", "aria-valuemin": "0", "aria-valuemax": String(total), "aria-valuenow": String(used), "aria-label": t("显存占用") },
      h("i", { style: { width: `${(Math.min(1, used / total) * 100).toFixed(1)}%` } })),
    h("span", { class: "mono", style: { fontSize: "12.5px", color: "var(--ink-2)", whiteSpace: "nowrap" }, text: `${(used / 1024).toFixed(1)} / ${(total / 1024).toFixed(1)} GB` }),
  );
}

/* ====================================================================== */
/* page: 识别引擎                                                          */
/* ====================================================================== */

/** Known Doubao resource ids offered as datalist suggestions. */
const DOUBAO_RESOURCES = [
  ["volc.seedasr.sauc.duration", t("Seed ASR 2.0 · 按时长")],
  ["volc.seedasr.sauc.concurrent", t("Seed ASR 2.0 · 按并发")],
  ["volc.bigasr.sauc.duration", t("ASR 1.0 · 按时长")],
  ["volc.bigasr.sauc.concurrent", t("ASR 1.0 · 按并发")],
];

/**
 * Engine page: [asr], [local], [doubao], [cloud] as one draft saved through
 * saveSections(), plus the model catalog and the models imported from local
 * GGUF files. Installs run on the server; the list polls /api/models every
 * 1.5 s while any model is installing and toasts completion or failure.
 * Picking a model only edits the draft: the save bar saves it and the apply
 * (POST /api/engine/reload) switches to it once the runtime has verified
 * it, so the running engine stays until then.
 */
function EnginePage() {
  const SECTIONS = ["asr", "local", "doubao", "cloud"];
  let saved = null;
  let draft = null;
  let models = [];
  /** The last failed /api/models read, shown in place of the list until a retry succeeds. */
  let modelsError = null;
  /** Filters over the list: a language code and "cpu" | "gpu"; "" leaves everything in. */
  const filter = { language: "", device: "" };
  /** Language codes the filter last offered, so a poll does not rebuild unchanged tiles. */
  let filterLanguages = null;
  /** The import under way or waiting for a retry: { file, state, sent, error, request }. */
  let importJob = null;
  let filterSlot = null;
  let importSlot = null;
  let importButton = null;
  let mounted = false;
  let bar = null;
  let pollTimer = null;
  let view = null;
  let modelList = null;
  const runtimeSlot = h("div");
  let unsubscribeRuntime = null;

  const dirty = () => saved && !same(saved, draft);
  const changed = () => bar?.set(dirty());

  /** A model `[local].model` can take: an offline recognition model, not the VAD, the denoiser or the streaming one. */
  const isLocalModel = (model) => model.kind === "gguf" || model.kind === "sherpa";
  /** Devices this build can run `model` on, as /api/models reports them. */
  const devicesOf = (model) => model.devices || [];
  const modelDevices = (id, catalog = models) => catalog.find((model) => model.id === id)?.devices;
  /** Why this build cannot run `model` ("" when it can). */
  const unusableReason = (model) => (isLocalModel(model) && !devicesOf(model).length ? BUILD_LIMITS[model.kind] || t("当前构建不支持这个模型") : "");
  /** This build has no engine for GGUF models: the catalog lists them and none can run. */
  const lacksGgufEngine = () => models.some((model) => model.kind === "gguf") && !models.some((model) => model.kind === "gguf" && devicesOf(model).length);
  const hasGpu = (model) => devicesOf(model).some((device) => device !== "cpu" && device !== "auto");

  /** Installed offline models for the [local] select, the usable ones first; the current value stays listed even when it is neither. */
  function localModelOptions() {
    const device = (model) => (unusableReason(model) ? t("当前构建不可用") : hasGpu(model) ? "GPU" : "CPU");
    const installed = models.filter((model) => isLocalModel(model) && model.installed);
    const options = [...installed.filter((model) => !unusableReason(model)), ...installed.filter(unusableReason)]
      .map((model) => [model.id, `${model.id}（${device(model)}）`, Boolean(unusableReason(model))]);
    const current = draft.local?.model;
    if (current && !options.some(([id]) => id === current)) options.unshift([current, t(`${current}（未下载）`)]);
    if (!options.length) options.push(["", t("没有已下载的离线模型")]);
    return options;
  }

  const backendItems = BACKENDS.map(([value, title, iconName, sub]) => ({ value, icon: iconName, title, sub }));
  const fallbackItems = [{ value: null, icon: "minus", title: t("不用") }, ...backendItems];
  const providerItems = [
    { value: null, icon: "sliders-horizontal", title: t("默认"), tip: t("按模型默认") },
    { value: "auto", icon: "target", title: t("自动"), tip: t("可用的 GPU 后端，没有就用 CPU") },
    { value: "cuda", icon: "zap", title: "CUDA" },
    { value: "vulkan", icon: "layers", title: "Vulkan" },
    { value: "cpu", icon: "brain", title: "CPU" },
  ];
  const batchItems = [
    { value: "cloud", icon: "cloud", title: t("云端"), sub: "DashScope" },
    { value: "local", icon: "brain", title: t("本地"), sub: "SenseVoice" },
  ];

  function devicePicker(local) {
    const devices = modelDevices(local.model);
    const choices = [];
    for (const item of providerItems) {
      if (item.value === null || devices?.includes(item.value)) choices.push(item);
      else if (item.value === local.provider) {
        const reason = devices ? t("不可用") : t("未知");
        choices.push({ ...item, disabled: true, sub: reason, tip: reason });
      }
    }
    return tiles(choices, local.provider, (value) => { local.provider = value; changed(); }, { small: true, label: t("设备") }).el;
  }

  /** Only an explicit model switch can replace an incompatible device with the model default. */
  function keepCompatibleDevice(local) {
    const devices = modelDevices(local.model);
    if (local.provider && devices && !devices.includes(local.provider)) local.provider = null;
  }

  /** (Re)build the whole page from `draft`: after load, discard, save, a picked model and a changed installation. */
  function build() {
    const a = draft.asr;
    const l = draft.local;
    const d = draft.doubao;
    const c = draft.cloud;
    const deviceControl = h("div", {}, devicePicker(l));
    const modelControl = selectInput(l, "model", localModelOptions(), () => {
      keepCompatibleDevice(l);
      deviceControl.replaceChildren(devicePicker(l));
      renderModels();
      changed();
    });

    const resourceList = h("datalist", { id: "doubaoResources" }, DOUBAO_RESOURCES.map(([value, label]) => h("option", { value, label })));
    const resourceInput = textInput(d, "resource_id", changed, { mono: true });
    resourceInput.setAttribute("list", "doubaoResources");
    modelList = h("div");
    filterSlot = h("div", { style: { marginBottom: "14px" } });
    filterLanguages = null;
    importSlot = h("div");
    const fileInput = h("input", {
      type: "file",
      accept: ".gguf",
      hidden: true,
      "aria-label": t("选择 GGUF 文件"),
      onchange: (event) => {
        const file = event.target.files?.[0];
        // Cleared first, so picking the same file again is another change.
        event.target.value = "";
        if (file) void startImport(file);
      },
    });
    // renderImport() keeps its state: off while an import runs or the build cannot run GGUF models.
    importButton = btn("plus", t("导入 GGUF"), { cls: "sm", onclick: () => fileInput.click() });

    view.replaceChildren(
      pageHead(t("识别引擎"), ibtn("download", { label: t("导出设置模板"), onclick: (event) => exportSettings(event.currentTarget) })),
      group("cpu", t("运行状态"), null, runtimeSlot),
      group(
        "audio-lines",
        t("识别方式"),
        null,
        tiles(backendItems, a.streaming_backend, (value) => { a.streaming_backend = value; changed(); }).el,
        h("div", { class: "inline", style: { marginTop: "18px", gap: "12px" } },
          h("span", { class: "lab", text: t("备用") }),
          tiles(fallbackItems, a.fallback_backend, (value) => { a.fallback_backend = value; changed(); }, { small: true }).el),
        h("details", { class: "fold", style: { marginTop: "18px" } },
          h("summary", {}, icon("chevron-down"), t("更多")),
          h("div", { class: "fold-body stack" },
            h("div", { class: "row" }, h("span", { class: "lab", text: t("整段转写") }),
              tiles(batchItems, a.batch_backend, (value) => { a.batch_backend = value; changed(); }, { small: true }).el),
            h("div", { class: "grid span" },
              row(t("流式模型目录"), textInput(a, "local_model_dir", changed, { mono: true, nullable: true, placeholder: t("模型目录") }), { cls: "s3" }),
              row(t("批量模型目录"), textInput(a, "batch_model_dir", changed, { mono: true, nullable: true, placeholder: t("模型目录") }), { cls: "s3" })))),
      ),
      group(
        "cpu",
        t("本地模型"),
        null,
        h(
          "div",
          { class: "grid span" },
          row(t("模型"), modelControl, { cls: "s2" }),
          row(t("设备"), deviceControl, { cls: "s4" }),
          row(t("线程"), numberInput(l, "num_threads", changed, { min: 1, max: 32 }), { cls: "s2", tip: t("仅 Sherpa 模型使用") }),
          row(t("出字间隔"), fader(l, "partial_interval_ms", changed, { min: 100, max: 5000, step: 50, format: (v) => `${v} ms`, label: t("出字间隔") }), { cls: "s2", tip: t("出字后的目标刷新间隔，耗时较长时会自动放缓") }),
          row(t("单句上限"), fader(l, "max_segment_seconds", changed, { min: 1, max: 60, step: 1, format: (v) => t(`${v} 秒`), label: t("单句上限") }), { cls: "s2", tip: t("GGUF 最多 60 秒，Sherpa 最多 25 秒") }),
        ),
        h("div", { class: "rockers", style: { marginTop: "18px" } },
          rocker(l, "denoise", changed, t("远讲降噪"), { tip: t("仅本地分段识别使用，需要 gtcrn 模型") }),
          rocker(l, "fast_preview", changed, t("快速预览"), { tip: t("先用小模型出临时文字") })),
        h("div", { class: "row", style: { marginTop: "16px" } },
          h("span", { class: "lab", text: t("热词") }),
          tagInput(l.hotwords, (values) => { l.hotwords = values; changed(); }, t("回车添加")).el),
      ),
      group(
        "cloud",
        t("云端账号"),
        null,
        h(
          "div",
          { class: "grid-2" },
          h(
            "div",
            { class: "inset stack" },
            h("div", { class: "inset-head" }, icon("cloud-cog"), h("b", { text: t("百炼") })),
            row("API Key", secretInput(c, "api_key", secretFlag(saved.cloud, "api_key"), changed)),
            row(t("实时模型"), textInput(c, "realtime_model", changed, { mono: true })),
            row(t("实时接口"), textInput(c, "realtime_endpoint", changed, { mono: true })),
            row(t("批量模型"), textInput(c, "model", changed, { mono: true })),
            row(t("批量接口"), textInput(c, "base_url", changed, { mono: true })),
            row(t("超时"), numberInput(c, "timeout_seconds", changed, { min: 1, max: 600 }), { tip: t("秒") }),
          ),
          h(
            "div",
            { class: "inset stack" },
            h("div", { class: "inset-head" }, icon("zap"), h("b", { text: t("豆包") })),
            resourceList,
            row("APP Key", secretInput(d, "api_key", secretFlag(saved.doubao, "api_key"), changed)),
            row("App ID", textInput(d, "app_key", changed, { mono: true })),
            row("Access Token", secretInput(d, "access_key", secretFlag(saved.doubao, "access_key"), changed)),
            row(t("资源 ID"), resourceInput, { tip: t("要与开通的服务一致") }),
            row(t("接口地址"), textInput(d, "endpoint", changed, { mono: true })),
            row(t("断句静音"), numberInput(d, "end_window_size", changed, { min: 200, max: 5000, step: 50, nullable: true, placeholder: "800" }), { tip: t("毫秒") }),
            h("div", { class: "rockers" },
              rocker(d, "enable_punc", changed, t("标点")),
              rocker(d, "enable_itn", changed, t("数字规整"), { tip: t("“二十三”写成 23") }),
              rocker(d, "enable_ddc", changed, t("语义顺滑"), { tip: t("去掉口头语和重复") }),
              rocker(d, "enable_nonstream", changed, t("二遍识别"), { tip: t("更准，略慢") })),
            h("div", { class: "row" }, h("span", { class: "lab", text: t("热词") }),
              tagInput(d.hotwords, (values) => { d.hotwords = values; changed(); }, t("回车添加")).el),
          ),
        ),
      ),
      group("package", t("模型"), [importButton], filterSlot, importSlot, modelList, fileInput),
    );
    renderFilters();
    renderModels();
  }

  /** Language and device filters over the list; a language is offered while any model has it. */
  function renderFilters() {
    if (!filterSlot) return;
    const languages = [...new Set(models.flatMap((model) => model.languages || []))];
    if (!languages.includes(filter.language)) filter.language = "";
    if (same(languages, filterLanguages)) return;
    filterLanguages = languages;
    filterSlot.replaceChildren(
      h("div", { class: "inline", style: { flexWrap: "wrap", gap: "16px 28px" } },
        row(t("语言"), selectInput(filter, "language",
          [["", t("全部")], ...languages.map((code) => [code, LANGUAGE_LABELS[code] || code])], renderModels)),
        h("div", { class: "row" },
          h("span", { class: "lab", text: t("设备") }),
          tiles([{ value: "", icon: "cpu", title: t("全部") }, { value: "cpu", icon: "brain", title: "CPU" }, { value: "gpu", icon: "cpu", title: "GPU" }],
            filter.device, (value) => { filter.device = value; renderModels(); }, { small: true, label: t("CPU 或 GPU") }).el)),
    );
  }

  /** The list under the filters: this build's limit when it has one, then the models that pass the filters. */
  function renderModels() {
    if (!modelList) return;
    renderImport();
    if (modelsError) {
      const failed = emptyState("package", t(`读取失败：${humanError(modelsError)}`));
      failed.append(ibtn("refresh-cw", { label: t("重试"), onclick: (event) => busy(event.currentTarget, loadModels) }));
      modelList.replaceChildren(failed);
      return;
    }
    const shown = models.filter((model) =>
      (!filter.language || (model.languages || []).includes(filter.language))
      && (!filter.device || (filter.device === "gpu" ? hasGpu(model) : devicesOf(model).includes("cpu"))));
    setChildren(modelList,
      lacksGgufEngine() ? h("div", { style: { marginBottom: "12px" } }, note("triangle-alert", t(`${BUILD_LIMITS.gguf}；标记为不可用的模型不能选用，也不能导入`), "warn")) : null,
      !models.length ? emptyState("package", t("没有可用的模型"))
        : !shown.length ? emptyState("package", t("没有符合筛选的模型"))
        : h("div", { class: "items" }, ...shown.map(modelItem)),
    );
    schedulePoll();
  }

  /** One model row: its name and state, the actions that state allows, and the limit that keeps a model out of use here. */
  function modelItem(model) {
    const [kindLabel, kindIcon] = MODEL_KIND[model.kind] || [model.kind || "—", "package"];
    const reason = unusableReason(model);
    const selected = draft.local?.model === model.id;
    let status;
    let progress = null;
    if (model.installing) {
      progress = model.progress;
      status = chip(progress == null ? t("校验中") : `${Math.round(progress * 100)}%`, "cool");
    } else if (model.error) {
      status = tip(chip(t("失败"), "err"), model.error, "t");
    } else if (model.in_use) {
      status = chip(t("使用中"), "warn");
    } else if (reason) {
      status = tip(chip(t("不可用"), "warn"), reason, "t");
    } else if (model.installed) {
      status = chip(model.imported ? t("已导入") : t("已下载"), "ok");
    } else {
      status = chip(t("未下载"), "dashed");
    }
    const actions = [];
    if (isLocalModel(model) && model.installed) {
      const pick = tip(ibtn("check", {
        label: t("选用"),
        cls: "ghost",
        disabled: Boolean(reason),
        onclick: () => pickModel(model),
      }), reason || t("设为本地模型，保存后生效"));
      if (selected) pick.setAttribute("aria-pressed", "true");
      actions.push(pick);
    }
    if (!model.installed && !model.installing && !model.imported) {
      actions.push(ibtn("download", {
        label: model.error ? t("重试") : t("下载"),
        tip: model.error ? t("重试") : t(`下载 ${fmtBytes(model.size_bytes)}`),
        onclick: (event) => installModel(model, event.currentTarget),
      }));
    } else if (model.installed) {
      actions.push(ibtn("trash-2", {
        label: t("删除"),
        cls: "danger",
        disabled: model.in_use,
        tip: model.in_use ? t("正在使用的模型不能删除") : t("删除"),
        onclick: (event) => deleteModel(model, event.currentTarget),
      }));
    }
    const languages = languageText(model.languages || []);
    return h(
      "div",
      { class: `item${model.in_use ? " on" : ""}` },
      tip(h("div", { class: "glyph" }, icon(kindIcon)), t(`${kindLabel} 模型`), "t"),
      h("div", { class: "title" },
        h("b", { class: model.name === model.id ? "mono" : "", text: model.name || model.id }),
        status),
      h("div", { class: "acts" }, actions),
      h("div", { class: "sub" },
        model.name && model.name !== model.id ? h("span", { class: "mono", text: model.id, style: { overflowWrap: "anywhere" } }) : null,
        model.version ? h("span", { class: "mono", text: model.version }) : null,
        h("span", { class: "mono", text: fmtBytes(model.size_bytes) }),
        languages ? h("span", { text: languages }) : null,
        progress == null ? null : h("div", { class: "bar sig", role: "progressbar", "aria-valuemin": "0", "aria-valuemax": "100", "aria-valuenow": String(Math.round(progress * 100)) },
          h("i", { style: { width: `${(progress * 100).toFixed(1)}%` } })),
      ),
    );
  }

  /** The import's row, and the import button: disabled while an import runs or on a build that cannot run GGUF models. */
  function renderImport() {
    if (importSlot) setChildren(importSlot, importRow());
    if (!importButton) return;
    const ongoing = Boolean(importJob) && importJob.state !== "failed";
    importButton.disabled = ongoing || lacksGgufEngine();
    tip(importButton, lacksGgufEngine() ? BUILD_LIMITS.gguf : ongoing ? t("导入进行中") : t("从本机文件导入 GGUF 模型"));
  }

  /** The import's row: progress while the bytes travel, an indeterminate bar while the store verifies, the error with a retry after a failure. */
  function importRow() {
    if (!importJob) return null;
    const { file, state, sent, error } = importJob;
    const verifying = state === "verifying";
    const percent = Math.min(100, Math.floor((sent / file.size) * 100));
    const progress = state === "failed" ? null : h("div", {
      class: `bar sig${verifying ? " indeterminate" : ""}`,
      role: "progressbar",
      "aria-label": t("导入进度"),
      "aria-valuemin": "0",
      "aria-valuemax": "100",
      "aria-valuenow": verifying ? null : String(percent),
    }, h("i", verifying ? {} : { style: { width: `${percent}%` } }));
    return h(
      "div",
      { class: "item" },
      tip(h("div", { class: "glyph" }, icon("package")), t("导入的模型"), "t"),
      h("div", { class: "title" }, h("b", { text: file.name }),
        chip(state === "failed" ? t("失败") : verifying ? t("校验中") : `${percent}%`, state === "failed" ? "err" : "cool")),
      h("div", { class: "acts" },
        state === "sending" ? ibtn("x", { label: t("取消导入"), cls: "ghost", onclick: cancelImport }) : null,
        state === "failed" ? ibtn("refresh-cw", { label: t("重试导入"), onclick: () => startImport(file) }) : null,
        state === "failed" ? ibtn("x", { label: t("关闭"), cls: "ghost", onclick: clearImport }) : null),
      h("div", { class: "sub" }, progress,
        h("span", { style: { overflowWrap: "anywhere" }, text: state === "failed" ? error
          : verifying ? t(`已传输 ${fmtBytes(file.size)}，正在校验模型`)
          : t(`正在传输 · ${fmtBytes(sent)} / ${fmtBytes(file.size)}`) })),
    );
  }

  /** Upload `file` as a GGUF model, showing the transfer and then the store's verification; a failure keeps the file for a retry. */
  async function startImport(file) {
    const problem = !/\.gguf$/i.test(file.name) ? t("请选择 .gguf 文件") : file.size ? "" : t("文件是空的");
    if (problem) {
      toast(problem, "err");
      return;
    }
    const job = { file, state: "sending", sent: 0, error: "", request: null };
    importJob = job;
    renderImport();
    const upload = uploadFile("/api/models/import", file, (sent, total) => {
      if (importJob !== job || job.state !== "sending" || !mounted) return;
      job.sent = sent;
      if (sent >= total) job.state = "verifying";
      renderImport();
    });
    job.request = upload.request;
    let id;
    try {
      ({ id } = await upload.done);
    } catch (error) {
      if (importJob !== job) return;
      job.state = "failed";
      job.error = humanError(error);
      toast(job.error, "err");
      renderImport();
      return;
    }
    importJob = null;
    toast(t(`已导入 ${id}`), "ok");
    // The upload outlives the page when the user moves on; only the list needs the page.
    if (mounted) await loadModels();
  }

  /** Stop the transfer under way; the server drops the partial file once the request ends. */
  function cancelImport() {
    const job = importJob;
    importJob = null;
    job?.request.abort();
    renderImport();
  }

  /** Forget the failed import and its row. */
  function clearImport() {
    importJob = null;
    renderImport();
  }

  /** Read /api/models. A failure is kept and shown with a retry, not passed off as an empty catalog. */
  async function loadModels() {
    try {
      showModels(await api("/api/models", { quiet: true }));
    } catch (error) {
      modelsError = error;
      renderModels();
    }
  }

  /** Rebuild settings only when the model select or the selected model's device choices change. */
  function showModels(next) {
    const installed = (list) => list.filter((model) => model.installed).map((model) => model.id);
    const changedInstallation = !same(installed(models), installed(next));
    const modelId = draft?.local.model;
    const changedDevices = modelId && !same(modelDevices(modelId), modelDevices(modelId, next));
    models = next;
    modelsError = null;
    if (draft && (changedInstallation || changedDevices)) build();
    else {
      renderFilters();
      renderModels();
    }
  }

  /** While any install is running, refetch /api/models every 1.5 s and toast state changes. */
  function schedulePoll() {
    clearTimeout(pollTimer);
    if (models.some((model) => model.installing)) {
      pollTimer = setTimeout(async () => {
        try {
          const before = new Set(models.filter((m) => m.installing).map((m) => m.id));
          showModels(await api("/api/models", { quiet: true }));
          for (const model of models) {
            if (before.has(model.id) && model.installed) toast(t(`${model.id} 下载完成`), "ok");
            if (before.has(model.id) && model.error) toast(t(`${model.id} 下载失败：${model.error}`), "err");
          }
        } catch {
          schedulePoll();
        }
      }, 1500);
    }
  }

  /** POST .../install, then refresh the list; the poll takes over from there. */
  async function installModel(model, button) {
    await busy(button, async () => {
      try {
        await api(`/api/models/${encodeURIComponent(model.id)}/install`, { method: "POST" });
        toast(t(`开始下载 ${model.id}（${fmtBytes(model.size_bytes)}）`));
      } catch { /* toast shown */ }
    });
    await loadModels();
  }

  /** Confirm, DELETE, refresh (a 409 for a model in use surfaces as a toast). */
  async function deleteModel(model, button) {
    const again = model.imported ? t("之后要用需要重新导入") : t(`之后要用需要重新下载 ${fmtBytes(model.size_bytes)}`);
    if (!(await confirmDialog(t(`删除模型 ${model.id}？${again}。`), { okText: t("删除"), danger: true }))) return;
    await busy(button, async () => {
      try {
        await api(`/api/models/${encodeURIComponent(model.id)}`, { method: "DELETE" });
        toast(t(`已删除 ${model.id}`), "ok");
      } catch { /* toast shown */ }
    });
    await loadModels();
  }

  /** Make `model` the draft's local model. Only the draft changes: the save bar saves it and the apply switches to it, so other unsaved edits stay. */
  function pickModel(model) {
    if (draft.local.model === model.id) return;
    draft.local.model = model.id;
    keepCompatibleDevice(draft.local);
    build();
    changed();
    toast(t(`已选为本地模型 ${model.id}，保存并应用后生效`));
  }

  /** The runtime status on the page. It is kept, so that reloading the settings re-renders it rather than an empty `app.status`. */
  let shownStatus = null;
  function renderRuntime(status = app.status ?? shownStatus) {
    shownStatus = status;
    setChildren(runtimeSlot, recognitionDetails(status), configApplyDetails(status, saved?.revision, reloadConfig));
  }

  /** Save the edited fields and offer to apply resource changes. */
  async function save() {
    try {
      const result = await saveSections(saved, draft, SECTIONS);
      if (!result) return;
      await reloadConfig();
      toast(t("已保存"), "ok");
      if (result.restart_engine) await offerEngineReload(result.revision);
    } catch { /* toast shown; draft kept */ }
  }

  /** Fetch /api/config into saved/draft; secrets start as "" (= keep) so the draft is clean. */
  async function reloadConfig() {
    const config = await api("/api/config");
    saved = Object.fromEntries(SECTIONS.map((name) => [name, clone(config[name] || {})]));
    saved.revision = config.revision;
    for (const name of ["doubao", "cloud"]) {
      // Secrets are write-only: the draft starts "unchanged".
      saved[name].api_key = "";
      if (name === "doubao") saved[name].access_key = "";
    }
    draft = clone(saved);
    // Keep presence markers in the draft to show which credentials are set.
    build();
    renderRuntime();
    changed();
  }

  return {
    async mount(target) {
      mounted = true;
      view = target;
      view.append(pageHead(t("识别引擎")), emptyState("package", t("读取中…")));
      bar = saveBar({ onSave: save, onDiscard: () => { draft = clone(saved); build(); changed(); } });
      renderRuntime(app.status);
      unsubscribeRuntime = live.subscribe((event) => {
        if (recognitionChanged(event)) renderRuntime(app.status);
      });
      void fetchStatus().then(renderRuntime).catch((error) => {
        runtimeSlot.replaceChildren(note("circle-alert", humanError(error)));
      });
      await loadModels();
      try {
        await reloadConfig();
      } catch (error) {
        view.replaceChildren(pageHead(t("识别引擎")), loadFailed(error));
      }
    },
    unmount() {
      mounted = false;
      clearTimeout(pollTimer);
      unsubscribeRuntime?.();
      bar?.remove();
    },
    isDirty: dirty,
  };
}

/* ====================================================================== */
/* page: 词库                                                              */
/* ====================================================================== */

function vocabularyTerms(terms = {}) {
  return { hotwords: terms.hotwords || [], replacements: terms.replacements || [], rules: terms.rules || [] };
}

function termsForScope(text, scope, create = false) {
  if (!scope.mode && !scope.project) return text;
  const match = (text.scopes || []).find((item) => item.mode === scope.mode && item.project === scope.project);
  if (match || !create) return match?.terms;
  const terms = vocabularyTerms();
  (text.scopes ||= []).push({ ...scope, terms });
  return terms;
}

function vocabularyScopeFields(modes, initial = {}) {
  const mode = h("select", { class: "inpt", "aria-label": t("词库模式") },
    h("option", { value: "" }, t("全部模式")),
    ...modes.map((item) => h("option", { value: item.id }, modeName(item.id))),
    initial.mode && !modes.some((item) => item.id === initial.mode) ? h("option", { value: initial.mode }, initial.mode) : null,
  );
  mode.value = initial.mode || "";
  const project = h("input", { class: "inpt", "aria-label": t("词库项目"), value: initial.project || "" });
  return {
    el: h("div", { class: "stack" }, row(t("词库模式"), mode), row(t("词库项目"), project)),
    value: () => ({ mode: mode.value || undefined, project: project.value.trim() || undefined }),
  };
}

async function mergeVocabulary(base, incoming) {
  return api("/api/text/vocabulary/merge", { method: "POST", body: { base: vocabularyTerms(base), incoming } });
}

async function confirmVocabularyImport(report) {
  return (await openDialog({
    title: t("导入预览"),
    body: h("div", { class: "stack" },
      h("p", { text: t(`新增 ${report.added} 条，重复 ${report.duplicates} 条，冲突 ${report.conflicts.length} 条`) }),
      report.conflicts.length ? note("circle-alert", t("冲突项保留现有词条"), "warn") : null,
      report.conflicts.length ? h("ul", {}, report.conflicts.map((item) => h("li", { text: item.key }))) : null,
    ),
    actions: [{ label: t("取消"), value: "" }, { label: t("导入无冲突词条"), value: "import", class: "primary" }],
  })) === "import";
}

/**
 * Lexicon page: the [text] section as a draft with a debounced live
 * preview (POST /api/text/preview with the unsaved config) shown as a
 * diff; a regex error highlights the offending rule row.
 */
function TextPage() {
  let saved = null;
  let draft = null;
  let bar = null;
  let view = null;
  let rulesTable = null;
  let modes = [];
  let scope = {};
  let previewGeneration = 0;
  let sample = storage.get("previewSample", t("呃，我去找张三峰聊聊，然后现在，呃，我会给你发一份 receive test。"));
  const output = h("div", { class: "preview-out", "aria-live": "polite" });
  const outputMeta = h("span", { class: "lab" });
  const bias = h("p", { class: "lab" });

  const dirty = () => saved && !same(saved, draft);
  const changed = () => {
    bar?.set(dirty());
    schedulePreview();
  };

  const schedulePreview = debounce(runPreview, 300);

  /** Preview the sample through the unsaved rules; on error, point at the bad regex row. */
  async function runPreview() {
    if (!draft) return;
    const generation = ++previewGeneration;
    const started = performance.now();
    try {
      const result = await api("/api/text/preview", {
        method: "POST", body: { text: sample, config: draft.text, ...scope }, quiet: true,
      });
      if (generation !== previewGeneration) return;
      const status = result.recognition_bias === true ? t("支持热词识别偏置") : result.recognition_bias === false
        ? t("当前识别后端不使用热词") : t("取决于当前模型的词表支持");
      bias.textContent = [result.model || result.backend, status].filter(Boolean).join(" · ");
      output.className = "preview-out";
      output.replaceChildren(sample.trim() ? renderDiff(sample, result.text ?? "") : h("span", { style: { color: "var(--ink-3)" }, text: t("输入一句话试试") }));
      outputMeta.textContent = `${Math.round(performance.now() - started)} ms`;
      rulesTable?.clearInvalid();
    } catch (error) {
      if (generation !== previewGeneration) return;
      output.className = "preview-out err";
      output.textContent = humanError(error);
      outputMeta.textContent = "";
      // Point at the offending regex row when the server names its pattern.
      const index = (termsForScope(draft.text, scope)?.rules || []).findIndex((rule) => rule.pattern && error.message.includes(JSON.stringify(rule.pattern)));
      if (index >= 0) rulesTable?.markInvalid(index, error.message);
    }
  }

  async function addScope() {
    const fields = vocabularyScopeFields(modes);
    const choice = await openDialog({ title: t("添加词库范围"), body: fields.el,
      actions: [{ label: t("取消"), value: "" }, { label: t("添加"), value: "add", class: "primary" }] });
    if (choice !== "add") return;
    scope = fields.value();
    termsForScope(draft.text, scope, true);
    build();
    changed();
  }

  async function importTerms(file) {
    if (!file) return;
    try {
      if (file.size > 1024 * 1024) throw new Error(t("词库文件不得超过 1 MiB"));
      const incoming = JSON.parse(await file.text());
      const current = termsForScope(draft.text, scope);
      const { terms, report } = await mergeVocabulary(current, incoming);
      if (!await confirmVocabularyImport(report)) return;
      Object.assign(current, terms);
      build();
      changed();
    } catch (error) { toast(humanError(error), "err"); }
  }

  /** (Re)build the page from `draft` and run the preview once. */
  function build() {
    const text = draft.text;
    if (!termsForScope(text, scope)) scope = {};
    const terms = termsForScope(text, scope);
    const scopes = text.scopes || [];
    const index = scopes.findIndex((item) => item.mode === scope.mode && item.project === scope.project);
    const picker = h("select", { class: "inpt", "aria-label": t("词库范围"), onchange: (event) => {
      const selected = scopes[Number(event.target.value)];
      scope = { mode: selected?.mode, project: selected?.project };
      build();
    } }, h("option", { value: "-1" }, t("全局词库")), ...scopes.map((item, number) => h("option", {
      value: String(number), text: [item.mode && modeName(item.mode), item.project].filter(Boolean).join(" · "),
    })));
    picker.value = String(index);
    rulesTable = tableEditor({
      columns: [
        { key: "pattern", label: t("正则"), placeholder: t("(\\d+)点(\\d+)分"), mono: true, icon: "hash" },
        { key: "replace", label: t("替换为"), placeholder: "$1:$2", mono: true },
      ],
      rows: terms.rules,
      addLabel: t("规则"),
      onChange: (rows) => { terms.rules = rows; changed(); },
    });
    const replacements = tableEditor({
      columns: [
        { key: "from", label: t("识别成了"), placeholder: "receive test", icon: "replace" },
        { key: "to", label: t("改为"), placeholder: "resume text" },
      ],
      rows: terms.replacements,
      addLabel: t("替换"),
      onChange: (rows) => { terms.replacements = rows; changed(); },
    });
    const sampleInput = h("textarea", {
      class: "inpt",
      rows: 5,
      value: sample,
      placeholder: t("输入一句话"),
      "aria-label": t("试一试"),
      oninput: (event) => {
        sample = event.target.value;
        storage.set("previewSample", sample);
        schedulePreview();
      },
    });

    view.replaceChildren(
      pageHead(t("词库")),
      h("div", { class: "inline", style: { flexWrap: "wrap" } },
        picker,
        btn("plus", t("添加词库范围"), { onclick: () => void addScope() }),
        index >= 0 ? btn("trash-2", t("移除范围"), { onclick: () => { scopes.splice(index, 1); scope = {}; build(); changed(); } }) : null,
        btn("download", t("导出词库"), { onclick: () => downloadText("voiceless-vocabulary.json", JSON.stringify(vocabularyTerms(terms), null, 2), "application/json") }),
        h("label", { style: { maxWidth: "100%" } }, t("导入词库"), h("input", { type: "file", accept: ".json,application/json", "aria-label": t("导入词库"), style: { maxWidth: "100%" }, onchange: (event) => { void importTerms(event.target.files?.[0]); event.target.value = ""; } })),
      ),
      h(
        "div",
        { class: "grid-2" },
        h(
          "div",
          { class: "stack", style: { "--gap": "0" } },
          group(
            "book-open",
            t("热词"),
            null,
            tagInput(terms.hotwords, (values) => { terms.hotwords = values; changed(); }, t("回车添加")).el,
            bias,
            h("p", { class: "lab", text: t("至少两个汉字的热词可用于谐音纠正") }),
            h("div", { class: "row", style: { marginTop: "16px" } },
              h("span", { class: "lab" }, t("读音相似度")),
              fader(text, "hotword_threshold", changed, { min: 0.5, max: 1, step: 0.01, format: (v) => Number(v).toFixed(2), label: t("读音相似度阈值") })),
          ),
          group(
            // These preferences apply to the entire text section.
            "wand-sparkles",
            t("清理"),
            null,
            h("div", { class: "rockers" },
              rocker(text, "remove_fillers", changed, t("去语气词"), { tip: t("删除所有“呃”和单独的“嗯”") }),
              rocker(text, "strip_trailing_period", changed, t("去句末句号"), { tip: t("适合聊天框") })),
          ),
          group("replace", t("替换"), null, replacements.el),
          group("hash", t("正则"), null, rulesTable.el),
        ),
        h(
          "div",
          { class: "inset stack", style: { position: "sticky", top: "150px" } },
          h("div", { class: "inline" }, icon("scan-text"), h("b", { text: t("试一试") }), h("span", { style: { flex: 1 } }), outputMeta),
          sampleInput,
          output,
        ),
      ),
    );
    runPreview();
  }

  /** Save [text], reload, offer an engine reload when hotwords changed. */
  async function save() {
    try {
      const result = await saveSections(saved, draft, ["text"]);
      if (!result) return;
      await reloadConfig();
      toast(t("词库已保存"), "ok");
      if (result.restart_engine) await offerEngineReload(result.revision);
    } catch { /* toast shown */ }
  }

  /** Fetch [text] into saved/draft, defaulting the list fields so the editors can bind. */
  async function reloadConfig() {
    const [config, availableModes] = await Promise.all([api("/api/config"), getModes()]);
    modes = availableModes;
    const text = config.text || {};
    saved = {
      revision: config.revision,
      text: {
        ...text,
        hotwords: text.hotwords || [],
        replacements: text.replacements || [],
        rules: text.rules || [],
        scopes: text.scopes || [],
      },
    };
    draft = clone(saved);
    build();
    changed();
  }

  return {
    async mount(target) {
      view = target;
      view.append(pageHead(t("词库")), emptyState("book-open", t("读取中…")));
      bar = saveBar({ onSave: save, onDiscard: () => { draft = clone(saved); build(); changed(); } });
      try {
        await reloadConfig();
      } catch (error) {
        view.replaceChildren(pageHead(t("词库")), loadFailed(error));
      }
    },
    unmount() {
      schedulePreview.cancel();
      bar?.remove();
    },
    isDirty: dirty,
  };
}

/* ====================================================================== */
/* page: 模式                                                              */
/* ====================================================================== */

/** Glyph of the built-in modes; custom ones get the sparkles. */
const MODE_ICONS = { quick: "zap", "voice-polish": "wand-sparkles", "prompt-optimize": "square-pen", "translate-en": "languages" };
const PROCESSING_PROVIDERS = [
  { value: "none", icon: "minus", title: t("不用") },
  { value: "openai-compatible", icon: "cloud", title: t("OpenAI 兼容") },
  { value: "ollama", icon: "server", title: "Ollama", tip: t("本地") },
];

const OUTPUT_STEPS = [
  ["fillers", () => t("清理语气词")], ["hotwords", () => t("纠正热词")], ["replace", () => t("替换词语")],
  ["regex", () => t("应用正则")], ["ending", () => t("清理句末标点")], ["trim", () => t("去掉首尾空白")], ["model", () => t("模型处理")],
  ["script", () => t("本地脚本")],
];

function outputTrace(trace, preview = false) {
  const { mode, source, app_id: appId, app_source: appSource, site, site_source: siteSource, project } = trace.selection;
  const reasons = { manual: t("手动选择"), "application-rule": t("应用规则"), "site-rule": t("网站规则"), default: t("默认模式") };
  const sources = { preview: t("预览输入"), "local-client": t("本机调用") };
  const annotated = (value, origin) => value && (origin ? `${value} (${sources[origin] || origin})` : value);
  return h("div", { class: "stack" },
    h("p", { text: [modeName(mode), reasons[source], annotated(appId, appSource), annotated(site, siteSource), project].filter(Boolean).join(" · ") }),
    trace.steps.length ? h("ol", {}, trace.steps.map((step) => h("li", {},
      h("b", { text: OUTPUT_STEPS.find(([key]) => key === step.step)?.[1]() || step.step }),
      h("span", { text: ` · ${step.elapsed_ms} ms` }),
      step.error ? note("circle-x", step.error, "err") : null,
      preview && step.before !== step.after ? h("dl", { class: "kv" },
        h("dt", { text: t("处理前") }), h("dd", {}, h("pre", { text: step.before, style: { whiteSpace: "pre-wrap", overflowWrap: "anywhere" } })),
        h("dt", { text: t("处理后") }), h("dd", {}, h("pre", { text: step.after, style: { whiteSpace: "pre-wrap", overflowWrap: "anywhere" } })),
      ) : null,
    ))) : null,
  );
}

function outputControls(output, modes, changed) {
  const ruleList = h("ol", { class: "stack" });
  const stepList = h("ol", { class: "stack" });
  const previewResult = h("div", { class: "stack", "aria-live": "polite" });
  function modePicker(label, value, onChange, automatic = false) {
    const select = h("select", { class: "inpt", "aria-label": label, onchange: onChange },
      automatic ? h("option", { value: "" }, t("按应用自动选择")) : null,
      ...modes.map((mode) => h("option", { value: mode.id }, mode.name || mode.id)),
      value && !modes.some((mode) => mode.id === value) ? h("option", { value }, value) : null,
    );
    select.value = value;
    return select;
  }
  const command = (label, onclick, disabled = false) => h("button", { type: "button", class: "btn sm", onclick, disabled, text: label });
  const move = (entries, index, amount) => {
    const other = index + amount;
    [entries[index], entries[other]] = [entries[other], entries[index]];
    changed();
  };

  function renderRules() {
    ruleList.replaceChildren(...output.rules.map((rule, index) => h("li", { class: "stack" },
      row(t("应用 ID"), h("input", { class: "inpt", "aria-label": t("应用 ID"), value: rule.app_id, placeholder: "kitty", oninput: (event) => { rule.app_id = event.target.value; changed(); } })),
      row(t("网站主机名"), h("input", { class: "inpt", "aria-label": t("网站主机名"), value: rule.site || "", placeholder: "example.com", oninput: (event) => { rule.site = event.target.value.trim() || undefined; changed(); } })),
      row(t("项目"), h("input", { class: "inpt", "aria-label": t("项目"), value: rule.project || "", oninput: (event) => { rule.project = event.target.value.trim() || undefined; changed(); } })),
      row(t("规则模式"), modePicker(t("规则模式"), rule.mode, (event) => { rule.mode = event.target.value; changed(); })),
      h("div", { class: "inline" },
        h("label", {}, h("input", { type: "checkbox", checked: rule.enabled, onchange: (event) => { rule.enabled = event.target.checked; changed(); } }), t("启用规则")),
        command(t("上移"), () => { move(output.rules, index, -1); renderRules(); }, index === 0),
        command(t("下移"), () => { move(output.rules, index, 1); renderRules(); }, index === output.rules.length - 1),
        command(t("删除"), () => { output.rules.splice(index, 1); changed(); renderRules(); }),
      ),
    )));
  }

  const editor = modePicker(t("编辑步骤的模式"), modes[0]?.id || "", () => renderSteps());
  function renderSteps() {
    const id = editor.value;
    if (!id) return;
    const saved = output.pipelines.find((item) => item.mode === id);
    const catalog = OUTPUT_STEPS.filter(([step]) => id !== "quick" || step !== "model");
    const order = saved?.steps.slice() || catalog.map(([step]) => step).filter((step) => step !== "script");
    const enabled = new Set(order);
    const steps = [...order, ...catalog.map(([step]) => step).filter((step) => !enabled.has(step))];
    const plan = saved || { mode: id, steps: order, allow_model: false };
    const persist = () => {
      if (!output.pipelines.includes(plan)) output.pipelines.push(plan);
      changed();
    };
    const update = () => {
      plan.steps = steps.filter((step) => enabled.has(step));
      persist();
      renderSteps();
    };
    stepList.replaceChildren(...steps.map((step, index) => {
      const label = OUTPUT_STEPS.find(([key]) => key === step)[1]();
      return h("li", { class: "inline" },
        h("label", {}, h("input", { type: "checkbox", checked: enabled.has(step), onchange: (event) => {
          if (event.target.checked) enabled.add(step); else enabled.delete(step);
          update();
        } }), label),
        h("button", { type: "button", class: "btn sm", disabled: index === 0 || !enabled.has(step), "aria-label": `${t("上移")} ${label}`, text: t("上移"), onclick: () => { move(steps, index, -1); update(); } }),
        h("button", { type: "button", class: "btn sm", disabled: index >= order.length - 1 || !enabled.has(step), "aria-label": `${t("下移")} ${label}`, text: t("下移"), onclick: () => { move(steps, index, 1); update(); } }),
      );
    }));
    if (enabled.has("model")) stepList.append(h("li", {}, h("label", {},
      h("input", { type: "checkbox", checked: plan.allow_model, onchange: (event) => { plan.allow_model = event.target.checked; persist(); } }),
      t("允许该模式将文字发送给已配置的模型"),
    )));
    if (enabled.has("script")) {
      const script = plan.script ?? { source: "", timeout_ms: 500, allowed: false };
      const allow = h("input", { type: "checkbox", checked: script.allowed, onchange: (event) => {
        script.allowed = event.target.checked;
        plan.script = script;
        persist();
      } });
      const setSource = (source) => {
        script.source = source;
        script.allowed = false;
        allow.checked = false;
        plan.script = script;
        persist();
      };
      const source = h("textarea", { class: "inpt", rows: 4, "aria-label": t("脚本代码"), value: script.source,
        placeholder: 'text.trim()', oninput: (event) => setSource(event.target.value) });
      const file = h("input", { type: "file", accept: ".rhai,.txt", "aria-label": t("导入脚本"), onchange: async (event) => {
        const selected = event.target.files?.[0];
        if (!selected) return;
        if (selected.size > 16 * 1024) { toast(t("脚本不得超过 16 KiB"), "err"); return; }
        source.value = await selected.text();
        setSource(source.value);
      } });
      stepList.append(h("li", { class: "stack" },
        row(t("脚本代码"), source), row(t("导入脚本"), file),
        row(t("脚本时限（毫秒）"), h("input", { type: "number", min: 1, max: 5000, class: "inpt", "aria-label": t("脚本时限（毫秒）"), value: script.timeout_ms,
          onchange: (event) => { script.timeout_ms = event.target.valueAsNumber; plan.script = script; persist(); } })),
        h("label", {}, allow, t("允许脚本读取并改写文字")),
        note("lock", t("脚本无法访问文件、网络或系统命令")),
      ));
    }
  }

  const previewMode = modePicker(t("预览模式"), "", null, true);
  const previewApp = h("input", { class: "inpt", "aria-label": t("预览应用 ID"), placeholder: "kitty" });
  const previewSite = h("input", { class: "inpt", "aria-label": t("预览网站"), placeholder: "example.com" });
  const previewProject = h("input", { class: "inpt", "aria-label": t("预览项目") });
  const sample = h("textarea", { class: "inpt", rows: 4, "aria-label": t("示例文本") });
  const send = h("input", { type: "checkbox" });
  const preview = h("button", { type: "button", class: "btn", text: t("预览输出"), onclick: (event) => busy(event.currentTarget, async () => {
    try {
      const result = await api("/api/modes/preview", { method: "POST", body: {
        text: sample.value, mode: previewMode.value, app_id: previewApp.value, site: previewSite.value,
        project: previewProject.value, output, send_model: send.checked,
      } });
      previewResult.replaceChildren(outputTrace(result.trace, true),
        h("pre", { "aria-label": t("预览结果"), style: { whiteSpace: "pre-wrap", overflowWrap: "anywhere" }, text: result.text }));
    } catch { previewResult.replaceChildren(); }
  }) });
  renderRules();
  renderSteps();
  return h("div", { class: "stack" },
    group("sparkles", t("应用规则"), null, ruleList, command(t("添加应用规则"), () => {
      output.rules.push({ app_id: "", mode: "quick", enabled: true }); changed(); renderRules();
      // Rules are evaluated in the order shown.
    }), h("label", {},
      h("input", { type: "checkbox", checked: output.allow_site_context, onchange: (event) => { output.allow_site_context = event.target.checked; changed(); } }),
      t("允许本机调用提供网站主机名"),
    )),
    group("terminal", "OpenCode", null, h("label", {},
      h("input", { type: "checkbox", checked: output.allow_opencode, onchange: (event) => { output.allow_opencode = event.target.checked; changed(); } }),
      t("允许 OpenCode 使用本机输出预览"),
    )),
    group("sparkles", t("输出步骤"), null, row(t("编辑步骤的模式"), editor), stepList),
    group("sparkles", t("输出预览"), null, row(t("预览模式"), previewMode), row(t("预览应用 ID"), previewApp),
      row(t("预览网站"), previewSite), row(t("预览项目"), previewProject),
      row(t("示例文本"), sample), h("label", {}, send, t("允许本次预览将示例发送给已配置的模型")), preview, previewResult),
  );
}

/**
 * Modes page: custom modes are edited one at a time in dialogs and saved
 * immediately through /api/modes; the organize prompt (/api/prompt) and
 * the [processing] section form a draft saved by the save bar.
 */
function ModesPage() {
  let view = null;
  let modes = [];
  let bar = null;
  let saved = null;
  let draft = null;
  const list = h("div", { class: "items" });
  const controls = h("div");

  const dirty = () => saved && !same(saved, draft);
  const changed = () => bar?.set(dirty());

  /** Mode list; built-ins are read-only, custom ones get edit/delete. */
  function renderModes() {
    if (!modes.length) {
      list.replaceChildren(emptyState("sparkles", t("没有模式")));
      return;
    }
    modesCache = modes;
    list.replaceChildren(
      ...modes.map((mode) =>
        h(
          "div",
          { class: "item" },
          h("div", { class: "glyph" }, icon(MODE_ICONS[mode.id] || "sparkles")),
          h("div", { class: "title" },
            h("b", { text: mode.name || mode.id }),
            h("span", { class: "mono", style: { color: "var(--ink-3)", fontSize: "12.5px" }, text: mode.id }),
            mode.builtin ? chip(t("内置"), "", "lock") : chip(t("自定义"), "ok")),
          h("div", { class: "acts" },
            mode.builtin ? null : ibtn("pencil", { label: t("编辑"), cls: "ghost sm", onclick: () => editMode(mode) }),
            mode.builtin ? null : ibtn("trash-2", { label: t("删除"), cls: "ghost sm danger", onclick: (event) => deleteMode(mode, event.currentTarget) })),
          mode.prompt || mode.id === "quick"
            ? h("div", { class: "text" }, mode.prompt || t("直接输出识别结果，不调用大模型。"))
            : null,
        ),
      ),
    );
    if (draft) controls.replaceChildren(outputControls(draft.output, modes, changed));
  }

  async function refreshModes() {
    try {
      modes = [...(await api("/api/modes"))].sort((a, b) => (a.sort_order ?? 0) - (b.sort_order ?? 0));
    } catch {
      modes = [];
    }
    renderModes();
  }

  /** Create (mode null) or edit a mode in a dialog; warns when the prompt lacks {text}. */
  async function editMode(mode) {
    const form = { name: mode?.name || "", processing_label: mode?.processing_label || "", prompt: mode?.prompt || "" };
    const name = h("input", { class: "inpt", value: form.name, placeholder: t("例如：翻译成英文"), oninput: (e) => { form.name = e.target.value; } });
    const label = h("input", { class: "inpt", value: form.processing_label, placeholder: t("例如：翻译中"), oninput: (e) => { form.processing_label = e.target.value; } });
    const prompt = h("textarea", { class: "inpt", rows: 7, value: form.prompt, spellcheck: "false", placeholder: t("把下面的内容翻译成自然的英文，只输出译文：{text}"), oninput: (e) => { form.prompt = e.target.value; } });
    const body = h(
      "div",
      { class: "stack" },
      row(t("名称"), name),
      row(t("处理中提示"), label, { tip: t("浮层在等待大模型时显示的文字") }),
      row(t("提示词"), prompt, { tip: t("{text} 会被替换为识别结果") }),
    );
    const result = await openDialog({
      title: mode ? t(`编辑：${mode.name}`) : t("新建模式"),
      body,
      actions: [
        { label: t("取消"), value: "" },
        { label: mode ? t("保存") : t("创建"), icon: "check", value: "save", class: "primary" },
      ],
      onOpen: () => name.focus(),
    });
    if (result !== "save") return;
    if (!form.name.trim()) {
      toast(t("名称不能为空"), "warn");
      return;
    }
    if (!form.prompt.includes("{text}")) {
      toast(t("提示词里没有 {text}，识别结果不会被带进去"), "warn", 6000);
    }
    try {
      if (mode) await api(`/api/modes/${encodeURIComponent(mode.id)}`, { method: "PUT", body: form });
      else await api("/api/modes", { method: "POST", body: form });
      toast(mode ? t("模式已更新") : t("模式已创建"), "ok");
      await refreshModes();
      await fillDeckModes();
    } catch { /* toast shown */ }
  }

  /** Confirm and DELETE a custom mode (built-ins get 409 from the server). */
  async function deleteMode(mode, button) {
    if (!(await confirmDialog(t(`删除模式“${mode.name}”？`), { okText: t("删除"), danger: true }))) return;
    await busy(button, async () => {
      try {
        await api(`/api/modes/${encodeURIComponent(mode.id)}`, { method: "DELETE" });
        toast(t("已删除"), "ok");
      } catch { /* toast shown */ }
    });
    await refreshModes();
    await fillDeckModes();
  }

  async function save() {
    try {
      const result = await saveSections(saved, draft, ["processing", "output"]);
      if (!result) return;
      await reload();
      toast(t("已保存"), "ok");
    } catch { /* toast shown */ }
  }

  function build() {
    const p = draft.processing;
    view.replaceChildren(
      pageHead(t("模式"), ibtn("plus", { label: t("新建模式"), cls: "primary", onclick: () => editMode(null) })),
      group("sparkles", t("模式"), null, list),
      controls,
      group(
        "message-square-quote",
        t("提示词优化"),
        null,
        h("textarea", {
          class: "inpt",
          rows: 8,
          value: p.prompt,
          spellcheck: "false",
          "aria-label": t("提示词"),
          placeholder: t("把下面的内容整理成清晰的提示词，只输出提示词：{text}"),
          oninput: (event) => { p.prompt = event.target.value; changed(); },
        }),
      ),
      group(
        "bot",
        t("大模型"),
        null,
        tiles(PROCESSING_PROVIDERS, p.provider, (value) => { p.provider = value; changed(); }).el,
        h("div", { class: "grid span", style: { marginTop: "18px" } },
          row(t("模型"), textInput(p, "model", changed, { mono: true }), { cls: "s2" }),
          row(t("接口"), textInput(p, "base_url", changed, { mono: true }), { cls: "s4" }),
          row("API Key", secretInput(p, "api_key", secretFlag(saved.processing, "api_key"), changed), { cls: "s2" }),
          row(t("环境变量"), textInput(p, "api_key_env", changed, { mono: true, placeholder: "DASHSCOPE_API_KEY" }), { cls: "s2", tip: t("或从环境变量读取") }),
          row(t("超时"), numberInput(p, "timeout_seconds", changed, { min: 1, max: 600 }), { cls: "s2", tip: t("秒") })),
      ),
    );
    renderModes();
  }

  /** Load the stored processing fields and display the effective prompt when unset. */
  async function reload() {
    const [config, prompt] = await Promise.all([api("/api/config"), api("/api/prompt")]);
    const processing = clone(config.processing || {});
    if (!processing.prompt?.trim()) processing.prompt = prompt.prompt || "";
    processing.api_key = "";
    saved = { processing, output: clone(config.output || { rules: [], pipelines: [] }), revision: config.revision };
    draft = clone(saved);
    build();
    changed();
  }

  return {
    async mount(target) {
      view = target;
      view.append(pageHead(t("模式")), emptyState("sparkles", t("读取中…")));
      bar = saveBar({ onSave: save, onDiscard: () => { draft = clone(saved); build(); changed(); } });
      await refreshModes();
      try {
        await reload();
      } catch (error) {
        view.replaceChildren(pageHead(t("模式")), group("sparkles", t("模式"), null, list), loadFailed(error));
      }
    },
    unmount() {
      bar?.remove();
    },
    isDirty: dirty,
  };
}

/* ====================================================================== */
/* page: 快捷键与上屏                                                       */
/* ====================================================================== */

/**
 * Hotkey & injection page: [hotkey], [inject], [audio] as one draft. The
 * key-capture widget listens for one keydown and maps KeyboardEvent.code
 * to an evdev name via /api/hotkey/keys; the test button shows a 3 s
 * countdown while the server's POST /api/inject/test waits before typing.
 */
function HotkeyPage() {
  const SECTIONS = ["hotkey", "inject", "audio"];
  const KEYS = [
    ["key", t("按住或点按")], ["hold_key", t("按住说话")], ["toggle_key", t("点按切换")],
    ["cancel_key", t("取消录音")], ["raw_key", t("原文直出")], ["repaste_key", t("重贴上次结果")],
  ];
  let view = null;
  let saved = null;
  let draft = null;
  let bar = null;
  let keymap = [];
  let modes = [];
  let status = null;
  const listenerSlot = h("div", { role: "status" });
  const activeSlots = new Map();
  const applySlot = h("div");
  let unsubscribeRuntime = null;
  const inputStatus = h("div", { role: "status" });
  const sourceSelect = h("select", { class: "inpt", onchange: () => {
    draft.audio.source = sourceSelect.value || null;
    changed();
  } });
  let audioDevices = null;
  let audioTimer = null;
  let overlaySupport = null;
  let mounted = false;
  let capturing = null;

  const dirty = () => saved && !same(saved, draft);
  const changed = () => bar?.set(dirty());

  function keyLabel(chord) {
    if (!chord) return "—";
    return chord.split("+").map((part) => keymap.find((item) => item.evdev === part)?.code ?? keyName(part)).join("+");
  }

  function stopCapture() {
    capturing?.close();
    capturing = null;
  }

  function keyControl(field, title) {
    const keys = draft.hotkey;
    const display = h("kbd", { class: "keycap", text: keyLabel(keys[field]) });
    const active = h("span");
    activeSlots.set(field, active);
    const recordLabel = t(`录制${title}快捷键`);
    const record = ibtn("mic", { label: recordLabel, cls: "lg" });
    const setChord = (chord) => {
      keys[field] = chord;
      display.textContent = keyLabel(chord);
      clear.disabled = !chord;
      changed();
    };
    const clear = ibtn("x", { label: t(`清除${title}快捷键`), cls: "lg", disabled: !keys[field], onclick: () => setChord(null) });
    record.addEventListener("click", () => {
      const wasRecording = capturing?.field === field;
      stopCapture();
      if (wasRecording) return;
      display.classList.add("listening");
      display.textContent = t("按键…");
      record.replaceChildren(icon("x"));
      record.setAttribute("aria-label", t("取消录制快捷键"));
      const onKey = (event) => {
        event.preventDefault();
        event.stopPropagation();
        if (event.repeat || ["Control", "Shift", "Alt", "Meta", "AltGraph"].includes(event.key)) return;
        stopCapture();
        const modifiers = [[event.ctrlKey, "Ctrl"], [event.altKey, "Alt"], [event.shiftKey, "Shift"], [event.metaKey, "Super"]]
          .filter(([held]) => held).map(([, name]) => name);
        if (event.key === "Escape" && field !== "cancel_key" && !modifiers.length) return;
        const entry = keymap.find((item) => item.code === event.code);
        if (!entry || (field === "key" && modifiers.length)) {
          toast(field === "key" ? t("按住或点按请录制单键") : t(`无法录制 ${event.code || event.key}`), "warn");
          return;
        }
        const chord = [...modifiers, entry.evdev].join("+");
        const conflict = KEYS.find(([other]) => {
          if (field === other || !keys[other]) return false;
          const parts = keys[other].split("+");
          if (parts.pop() !== entry.evdev) return false;
          return field === "key" || other === "key" || same(parts.sort(), [...modifiers].sort());
        });
        if (conflict) {
          toast(t(`${keyLabel(chord)} 已用于${conflict[1]}`), "warn");
          return;
        }
        setChord(chord);
      };
      capturing = {
        field,
        close() {
          window.removeEventListener("keydown", onKey, true);
          display.classList.remove("listening");
          display.textContent = keyLabel(keys[field]);
          record.replaceChildren(icon("mic"));
          record.setAttribute("aria-label", recordLabel);
        },
      };
      window.addEventListener("keydown", onKey, true);
    });
    return row(title, h("div", { class: "inline" }, display, record, clear, active));
  }

  function keyCard() {
    const k = draft.hotkey;

    const ctrlOptions = [...modes.map((mode) => [mode.id, mode.name || mode.id]), ["raw", t("原文直出")]];
    const modeOptions = [["", t("默认")], ...ctrlOptions];
    if (k.ctrl_mode && !ctrlOptions.some(([id]) => id === k.ctrl_mode)) ctrlOptions.push([k.ctrl_mode, k.ctrl_mode]);

    return group(
      "key-round",
      t("按键"),
      null,
      h(
        "div",
        { class: "inset" },
        h("div", { class: "grid" },
          ...KEYS.map(([field, title]) => keyControl(field, title)),
          row(t("点按与按住模式"), selectInput(k, "mode", modeOptions, changed)),
          row(t("Ctrl 模式"), selectInput(k, "ctrl_mode", ctrlOptions, changed)),
          row(t("按住阈值"), fader(k, "hold_threshold_ms", changed, { min: 100, max: 1000, step: 10, format: (v) => `${v} ms`, label: t("按住阈值") })),
          listenerSlot),
        h("details", { class: "fold" }, h("summary", {}, icon("terminal"), t("快捷键的 CLI 命令")),
          h("div", { class: "fold-body" }, ...[
            [t("按住说话"), "voiceless ctl start / voiceless ctl stop"],
            [t("点按切换"), "voiceless ctl toggle"], [t("取消录音"), "voiceless ctl cancel"],
            [t("原文直出"), "voiceless ctl toggle --mode raw"], [t("重贴上次结果"), "voiceless ctl repaste"],
          ].map(([label, command]) => row(label, h("code", { text: command }))))),
      ),
    );
  }

  function listenerStatus() {
    if (!status?.hotkey) return chip(t("后台未运行"), "err");
    const { portal, error } = status.hotkey;
    return h("div", { class: "inline" },
      status.state !== "idle" ? chip(status.state === "recording" ? t("录音中") : t("识别中"), "ok") : null,
      error ? note("circle-alert", error) : null,
      portal?.trigger ? chip(t(`系统快捷键 ${portal.trigger}`), "ok") : null,
      portal?.error ? tip(chip(t("系统快捷键不可用"), "dashed"), portal.error, "b") : null);
  }

  function renderRuntime(nextStatus) {
    status = nextStatus;
    for (const [field, slot] of activeSlots) {
      const binding = status?.hotkey?.bindings?.find((value) => value.field === field);
      setChildren(slot, binding ? chip(`${keyLabel(binding.chord)}${t(binding.keyboards ? " 已生效" : " 不可用")}`, binding.keyboards ? "ok" : "err") : null);
    }
    listenerSlot.replaceChildren(listenerStatus());
    setChildren(applySlot, configApplyDetails(status, saved?.revision, reload));
  }

  /**
   * [inject] method as tiles; the preedit rocker only applies to fcitx5. A
   * native runtime has the paste and the copy alone: it pastes for every
   * saved preference but an explicit copy, which is shown as that paste
   * without writing anything over the saved preference.
   */
  function injectCard() {
    const i = draft.inject;
    const preedit = h("div", { hidden: i.prefer !== "fcitx5" }, rocker(i, "preedit", changed, t("输入框内实时预编辑"), { tip: t("识别中的文字以预编辑显示在光标处，说完正式上屏") }));
    const methodTiles = tiles(
      INJECT_METHODS.filter(([value]) => !NATIVE || NATIVE_METHODS.includes(value))
        .map(([value, title, iconName]) => ({ value, icon: iconName, title, tip: value === "fcitx5" ? t("经输入法提交，和打字同一条路，不占用剪贴板") : value === "clipboard" ? t("写入剪贴板后粘贴") : value === "copy" ? t("只放入剪贴板，由你自己粘贴") : t("模拟按键逐字输入") })),
      effectivePrefer(i.prefer),
      (value) => {
        // Picking the method already in effect keeps a Linux preference as saved.
        if (value === effectivePrefer(i.prefer)) return;
        i.prefer = value;
        preedit.hidden = value !== "fcitx5";
        changed();
      },
    );
    return group(
      "text-cursor-input",
      t("上屏"),
      null,
      methodTiles.el,
      linuxOnly(
        h("div", { class: "rockers", style: { marginTop: "18px" } }, preedit, rocker(i, "clipboard_fallback", changed, t("失败时回退剪贴板"))),
        h("details", { class: "fold", style: { marginTop: "18px" } },
          h("summary", {}, icon("terminal"), t("命令")),
          h("div", { class: "fold-body grid" },
            row(t("粘贴工具"), textInput(i, "paste_tool", changed, { mono: true })),
            row(t("粘贴参数"), textInput(i, "paste_command", changed, { mono: true })),
            row("wl-copy", textInput(i, "wl_copy_command", changed, { mono: true })),
            row("wtype", textInput(i, "wtype_command", changed, { mono: true })),
            row(t("X11 剪贴板"), textInput(i, "x11_clipboard_command", changed, { mono: true, placeholder: "xsel --clipboard --input" })),
            row(t("粘贴前聚焦"), textInput(i, "focus_command", changed, { mono: true })),
            row(t("超时"), numberInput(i, "timeout_seconds", changed, { min: 0.5, max: 60, step: 0.5 }), { tip: t("秒") }))),
      ),
    );
  }

  /** Test injection: send the request first, count down 3 s, then show the InjectionResult. */
  function testCard() {
    const text = h("input", { class: "inpt", value: storage.get("injectTest", t("Voiceless 上屏测试 ✓")), "aria-label": t("测试文字") });
    const result = h("div", { class: "note" });
    const run = ibtn("play", { label: t("测试上屏"), cls: "primary lg" });
    run.addEventListener("click", () =>
      busy(run, async () => {
        storage.set("injectTest", text.value);
        if (dirty()) toast(t("测试使用的是已保存的设置"), "warn");
        const overlay = h("div", { class: "countdown", role: "status" }, h("div", { class: "n" }, "3"));
        document.body.append(overlay);
        const request = api("/api/inject/test", { method: "POST", body: { text: text.value, delay_ms: 3000 } });
        for (const n of [3, 2, 1]) {
          $(".n", overlay).textContent = String(n);
          await sleep(1000);
        }
        overlay.remove();
        result.replaceChildren();
        try {
          const injection = (await request).injection || {};
          result.className = `note ${injection.ok ? "ok" : "err"}`;
          result.append(icon(injection.ok ? "check" : "circle-x"), h("span", { text: deliveryText(injection) }));
        } catch (error) {
          result.className = "note err";
          result.append(icon("circle-x"), h("span", { text: humanError(error) }));
        }
      }),
    );
    return group("play", t("测试"), null, h("div", { class: "inline" }, text, run), result);
  }

  function renderMicrophone() {
    if (!draft) return;
    const { microphones = [], effective, error } = audioDevices || {};
    const source = draft.audio.source || "";
    const options = microphones.map(({ id, label }) => h("option", { value: id, text: label }));
    if (source && !microphones.some(({ id }) => id === source)) {
      options.push(h("option", { value: source, text: t(`${source}（未连接）`) }));
    }
    sourceSelect.replaceChildren(h("option", { value: "", text: t("跟随系统默认") }), ...options);
    sourceSelect.value = source;
    setChildren(inputStatus,
      effective ? note("mic", t(`当前麦克风 · ${microphones.find(({ id }) => id === effective)?.label || effective}`)) : null,
      error ? note("circle-alert", error, "warn") : null,
    );
  }

  async function refreshMicrophone() {
    let next;
    try {
      next = await api("/api/desktop/audio", { quiet: true });
    } catch (error) {
      next = { error: humanError(error) };
    }
    if (!mounted) return;
    if (!same(next, audioDevices)) {
      audioDevices = next;
      renderMicrophone();
    }
    audioTimer = setTimeout(refreshMicrophone, 2000);
  }

  function microphoneCard() {
    return group("mic", t("麦克风"), null, row(t("输入设备"), sourceSelect), inputStatus);
  }

  /**
   * [audio]: media pause and start/stop cues, and the recording overlay
   * where the desktop has one. A native runtime names its window instead;
   * the probe's own reasons are the Linux tools' diagnostics.
   */
  function audioCard() {
    const a = draft.audio;
    return group(
      "volume-2",
      t("声音"),
      null,
      overlaySupport?.available ? linuxOnly(row(t("录音浮层"), selectInput(a, "overlay", [
        [null, t("跟随桌面设置")], [true, t("显示")], [false, t("关闭")],
      ], changed), { tip: t("由桌面已启用的 Eww 浮层显示") }))
        : NATIVE ? note("info", t("录音状态显示在 Voiceless 窗口中"))
        : overlaySupport?.reason ? note("circle-alert", overlaySupport.reason) : null,
      h("div", { class: "rockers" },
        rocker(a, "pause_media", changed, t("录音时暂停音乐"), { tip: t("结束后自动恢复播放") }),
        rocker(a, "cues", changed, t("开始与结束提示音"))),
    );
  }

  function build() {
    stopCapture();
    activeSlots.clear();
    view.replaceChildren(pageHead(t("快捷键与上屏")), applySlot, microphoneCard(), keyCard(), injectCard(), testCard(), audioCard());
    renderMicrophone();
    renderRuntime(status);
  }

  /** Save changed sections, reload, offer an engine reload when the hotkey changed. */
  async function save() {
    try {
      const result = await saveSections(saved, draft, SECTIONS);
      if (!result) return;
      await reload();
      toast(t("已保存"), "ok");
      if (result.restart_engine) await offerEngineReload(result.revision);
    } catch { /* toast shown */ }
  }

  /** Fetch the sections into saved/draft (key normalised to null) and the listener status. */
  async function reload() {
    const config = await api("/api/config");
    saved = Object.fromEntries(SECTIONS.map((name) => [name, clone(config[name] || {})]));
    saved.revision = config.revision;
    for (const [field] of KEYS) saved.hotkey[field] ??= null;
    draft = clone(saved);
    try {
      status = await fetchStatus();
    } catch {
      status = null;
    }
    build();
    changed();
  }

  return {
    async mount(target) {
      view = target;
      mounted = true;
      view.append(pageHead(t("快捷键与上屏")), emptyState("key-round", t("读取中…")));
      bar = saveBar({ onSave: save, onDiscard: () => { draft = clone(saved); build(); changed(); } });
      unsubscribeRuntime = live.subscribe((event) => {
        if (recognitionChanged(event) || event.type === "hotkey") renderRuntime(app.status);
      });
      const [keys, modeList, overlay] = await Promise.all([
        api("/api/hotkey/keys", { quiet: true }).catch(() => []), getModes(),
        api("/api/desktop/overlay", { quiet: true }).catch(() => null),
      ]);
      overlaySupport = overlay;
      keymap = keys || [];
      modes = modeList;
      try {
        await reload();
        if (mounted) void refreshMicrophone();
      } catch (error) {
        view.replaceChildren(pageHead(t("快捷键与上屏")), loadFailed(error));
      }
    },
    unmount() {
      stopCapture();
      mounted = false;
      clearTimeout(audioTimer);
      unsubscribeRuntime?.();
      bar?.remove();
    },
    isDirty: dirty,
  };
}

/* ====================================================================== */
/* page: 历史                                                              */
/* ====================================================================== */

/**
 * History page: records newest-first in day groups, loaded 100 at a time
 * with the server's cursor. A search box and filters select rows from the
 * whole library on the server, page by page and for the CSV export. One
 * <audio> plays at a time; waveforms are drawn from the WAV's PCM (fetched
 * once per record and cached). Rows expand to the player, raw/final text
 * and the re-transcribe / correct / delete actions.
 */
function HistoryPage() {
  let view = null;
  let feed = null;
  let feedStatus = null;
  let searchSlot = null;
  let cursor = null;
  /** The search the rows on screen belong to; counted up whenever the filter changes, so a page of the previous one still on its way is dropped. */
  let run = 0;
  /** The search whose page request is in flight; 0 when none (runs start at 1). */
  let pending = 0;
  let exhausted = false;
  let activeAudio = null; // { article, audio, record }
  const waveformCache = new Map();
  const totalsEl = h("div", { class: "totals" });
  /** The search as the server takes it: a blank field selects anything; `from` and `to` are local days (YYYY-MM-DD), both inclusive. */
  const filter = { text: "", from: null, to: null, mode: null, app: null, model: null, audio: null };
  /** What the menus offer, read from /api/records/facets. */
  let facets = { modes: [], apps: [], models: [] };
  let clearButton = null;
  const rerunSoon = debounce(() => search(), 250);

  // The year is part of a day's label: the same month and day in another year is another day.
  const dateFmt = new Intl.DateTimeFormat("zh-CN", { year: "numeric", month: "long", day: "numeric" });
  const weekdayFmt = new Intl.DateTimeFormat("zh-CN", { weekday: "short" });
  const timeFmt = new Intl.DateTimeFormat("zh-CN", { hour: "2-digit", minute: "2-digit", second: "2-digit", hour12: false });
  const audioUrl = (record) => withToken(`/api/audio/${encodeURIComponent(record.id)}.wav`);
  const hasAudio = (record) => record.has_audio ?? Boolean(record.audio_path);

  /** Header totals: records, characters, speech duration. */
  async function loadTotals() {
    try {
      const totals = await api("/api/totals", { quiet: true });
      const items = [
        ["layers", t("记录"), String(totals.records), t("条")],
        ["type", t("字数"), Number(totals.characters).toLocaleString("zh-CN"), ""],
        ["clock", t("语音时长"), fmtDuration(totals.duration_seconds), ""],
      ];
      totalsEl.replaceChildren(...items.map(([iconName, label, value, unit]) => {
        const ro = readout(iconName, label);
        ro.set(value, unit);
        return ro.el;
      }));
    } catch { /* decorative */ }
  }

  /** Find or create the day section a record belongs to (grouped by local date). */
  function dayGroup(record) {
    const label = dateFmt.format(new Date(record.created_at));
    let group = $$(".day", feed).find((el) => el.dataset.day === label);
    if (group) return group;
    group = h("section", { class: "day", dataset: { day: label } }, h("div", { class: "day-head" }, label, h("span", { text: weekdayFmt.format(new Date(record.created_at)) })));
    feed.append(group);
    return group;
  }

  /** Build one record: collapsed row (play, text, mini waveform, meta) + expandable detail. */
  function recordRow(record) {
    const audio = hasAudio(record);
    const time = timeFmt.format(new Date(record.created_at));
    const versions = historyRevisions(api, record.id);
    let provider = chip(backendLabel(record.asr_provider), "", backendIcon(record.asr_provider));
    const article = h("article", { class: `rec${audio ? " wave" : ""}`, dataset: { id: record.id } });
    const play = ibtn("play", { label: t("播放"), cls: "round ghost play", disabled: !audio });
    const row = h(
      "div",
      { class: "r", tabindex: "0", role: "button", "aria-expanded": "false", "aria-label": t(`展开记录：${record.final_text.slice(0, 30)}`) },
      play,
      h("div", { class: "main" }, h("div", { class: "txt", text: record.final_text }), audio ? h("canvas", { class: "wave" }) : null),
      h("div", { class: "meta" },
        h("span", { text: time }),
        h("span", { text: fmtDuration(record.duration_seconds) }),
        // A row from before an attribute was recorded says so rather than guessing.
        record.processing_mode ? chip(modeName(record.processing_mode), "", "sparkles") : chip(t("模式未记录"), "dashed"),
        provider,
        record.app_id ? tip(chip(record.app_id, "mono", "target"), t("听写时所在的应用"), "t") : chip(t("应用未记录"), "dashed"),
        audio ? null : chip(t("无录音"), "dashed")),
    );
    const compareSlot = h("div");
    const retranscribe = ibtn("refresh-cw", { label: t("重新识别"), cls: "ghost", disabled: !audio });
    const copy = ibtn("copy", { label: t("复制"), cls: "ghost" });
    const raw = h("div", { class: "raw", text: record.raw_text, hidden: record.raw_text === record.final_text });
    const status = h("div", { role: "status" });
    const correct = ibtn("pencil", { label: t("纠错"), cls: "ghost" });
    const remove = ibtn("trash-2", { label: t("删除"), cls: "ghost danger" });
    const expand = h(
      "div",
      { class: "x", hidden: true },
      audio ? [
        h("canvas", { class: "wave-big" }),
        h("div", { class: "scrub" }, h("span", { class: "t-cur", text: "0:00" }), h("input", { type: "range", min: "0", max: "1000", value: "0", "aria-label": t("播放进度") }), h("span", { class: "t-dur", text: fmtDuration(record.duration_seconds) })),
      ] : null,
      h("div", { class: "final", text: record.final_text }),
      raw,
      h("div", { class: "acts" }, retranscribe, copy, correct, remove),
      status, compareSlot,
    );
    article.append(row, expand);
    function updateRow(next) {
      Object.assign(record, next);
      row.querySelector(".txt").textContent = record.final_text;
      row.setAttribute("aria-label", t(`展开记录：${record.final_text.slice(0, 30)}`));
      expand.querySelector(".final").textContent = record.final_text;
      raw.textContent = record.raw_text;
      raw.hidden = record.raw_text === record.final_text;
      const nextProvider = chip(backendLabel(record.asr_provider), "", backendIcon(record.asr_provider));
      provider.replaceWith(nextProvider);
      provider = nextProvider;
    }

    let generation = 0;
    async function showVersions() {
      const request = ++generation;
      try {
        const all = await versions.list();
        if (request !== generation) return;
        const current = all.find(v => v.state === "current");
        if (current) {
          const { final_text, raw_text, processed_text, asr_provider, asr_model } = current;
          updateRow({ final_text, raw_text, processed_text, asr_provider, asr_model });
        }
        const expected = record.final_text;
        const kind = { original: t("原文"), correction: t("纠错"), retranscription: t("重新识别") };
        compareSlot.replaceChildren(...all.map(v => {
          const source = [kind[v.kind], v.asr_provider, v.asr_model, v.device].filter(Boolean).join(" · ");
          const panel = h("div", { class: "compare" }, h("div", { class: "c-head", text: source }));
          if (v.state === "current") {
            panel.append(h("span", { text: t("当前版本") }));
          } else {
            const confirm = h("button", { type: "button", class: "btn", text: v.state === "pending" ? t("确认替换") : t("恢复此版本") });
            confirm.onclick = () => busy(confirm, async () => {
              try {
                const adopted = await versions.adopt(v.id, expected);
                updateRow(adopted.record);
                void loadTotals();
                void loadFacets();
                status.textContent = "";
              } catch (error) { status.textContent = error.message; }
              await showVersions();
            });
            panel.append(renderDiff(expected, v.final_text), confirm);
          }
          return panel;
        }));
      } catch (error) { if (request === generation) status.textContent = error.message; }
    }

    const toggle = () => {
      expand.hidden = !expand.hidden;
      row.setAttribute("aria-expanded", String(!expand.hidden));
      if (!expand.hidden) void showVersions();
      if (audio && !expand.hidden) drawWaveforms(article, record);
    };
    // composedPath(), not target.closest(): the play handler swaps its icon, which detaches the target.
    row.addEventListener("click", (event) => {
      if (event.composedPath().some((node) => node.classList?.contains("ibtn"))) return;
      toggle();
    });
    row.addEventListener("keydown", (event) => {
      if (event.target !== row) return;
      if (event.key === "Enter" || event.key === " ") {
        event.preventDefault();
        toggle();
      }
    });
    if (audio) {
      play.addEventListener("click", () => {
        if (expand.hidden) toggle();
        togglePlay(article, record);
      });
      drawWaveforms(article, record);
    }
    retranscribe.addEventListener("click", () => busy(retranscribe, async () => {
      try {
        const models = await offlineModels(api);
        if (!models.length) { status.textContent = t("没有已安装的本地识别模型"); return; }
        const select = h("select", { class: "inpt", "aria-label": t("识别模型") },
          models.map(m => h("option", { value: m.id, text: m.name || m.id })));
        select.value = (models.find(m => m.in_use) || models[0]).id;
        const action = await openDialog({ title: t("重新识别"), body: select, actions: [
          { label: t("取消"), value: "" }, { label: t("重新识别"), value: "recognize", class: "primary" },
        ] });
        if (action !== "recognize") return;
        status.textContent = t("正在识别");
        await versions.recognize(select.value);
        status.textContent = "";
        await showVersions();
      } catch (error) { status.textContent = error.message; }
    }));
    copy.addEventListener("click", async () => {
      try { await navigator.clipboard.writeText(record.final_text); status.textContent = t("已复制"); }
      catch (error) { status.textContent = error.message; }
    });
    correct.addEventListener("click", async () => {
      const expected = record.final_text;
      try {
        await correctRecord({ ...record }, async text => {
          await versions.correct(text, expected);
          status.textContent = "";
          await showVersions();
        });
      } catch (error) { status.textContent = error.message; }
    });
    remove.addEventListener("click", () => deleteRecord(record, article, remove));
    dayGroup(record).append(article);
  }

  /**
   * Correction dialog: the user edits the text, "生成建议" asks
   * POST /api/corrections for replacement pairs, and the chosen ones are
   * merged into the selected vocabulary through the same rules as imports.
   */
  async function correctRecord(record, propose) {
    let corrected = record.final_text;
    let suggestions = [];
    const fields = vocabularyScopeFields(await getModes());
    const textarea = h("textarea", { class: "inpt", rows: 5, value: corrected, "aria-label": t("正确的文字"), oninput: (e) => { corrected = e.target.value; } });
    const list = h("div", { class: "sugg" });
    const hint = h("p", {});
    const save = { label: t("加入词库"), icon: "book-open", value: "save", class: "primary" };

    function renderSuggestions() {
      if (!suggestions.length) {
        list.replaceChildren();
        hint.textContent = t("没有找到可以加入词库的差异。");
        return;
      }
      hint.textContent = t("勾选要加入的条目：");
      list.replaceChildren(
        ...suggestions.map((item) => {
          const target = h("select", { class: "inpt", style: { width: "auto" }, "aria-label": t("加入到"), onchange: (e) => { item.target = e.target.value; } },
            h("option", { value: "replacement" }, t("替换规则")), h("option", { value: "hotword" }, t("热词")));
          target.value = item.target;
          return h(
            "label",
            {},
            h("input", { type: "checkbox", class: "inpt", checked: item.checked, onchange: (e) => { item.checked = e.target.checked; } }),
            h("span", { class: "pair diff" }, h("del", { text: item.from || t("（空）") }), icon("arrow-right", 14), h("ins", { text: item.to })),
            target,
          );
        }),
      );
    }

    const result = await openDialog({
      title: t("纠错"),
      body: h("div", { class: "stack" }, h("div", { class: "diff", style: { color: "var(--ink-2)" }, text: t(`原结果：${record.final_text}`) }), textarea, hint, list, fields.el),
      actions: [
        { label: t("取消"), value: "" },
        {
          label: t("生成建议"),
          icon: "sparkles",
          close: false,
          onClick: async (event) => {
            await busy(event.currentTarget, async () => {
              try {
                const response = await api("/api/corrections", { method: "POST", body: { original: record.final_text, corrected } });
                suggestions = (response.suggestions || []).map((item) => ({
                  ...item,
                  checked: true,
                  target: "replacement",
                }));
                renderSuggestions();
              } catch { /* toast shown */ }
            });
          },
        },
        save,
        { label: t("对比"), value: "compare", icon: "pencil" },
      ],
      onOpen: () => textarea.focus(),
    });
    if (result === "compare") return propose(corrected);
    if (result !== "save") return;
    const chosen = suggestions.filter((item) => item.checked && item.to);
    if (!chosen.length) {
      toast(t("没有选中任何条目"), "warn");
      return;
    }
    try {
      const config = await api("/api/config");
      const text = clone(config.text || {});
      const destination = termsForScope(text, fields.value(), true);
      const incoming = vocabularyTerms();
      for (const item of chosen) {
        if (item.target === "hotword") incoming.hotwords.push(item.to);
        else if (item.from) incoming.replacements.push({ from: item.from, to: item.to });
      }
      const { terms, report } = await mergeVocabulary(destination, incoming);
      if (report.conflicts.length && !await confirmVocabularyImport(report)) return;
      Object.assign(destination, terms);
      const response = await saveSections(config, { text }, ["text"]);
      if (!response) return;
      toast(t(`已加入词库（${report.added} 条）`), "ok");
      if (response.restart_engine) await offerEngineReload(response.revision);
    } catch { /* toast shown */ }
  }

  /** Confirm, DELETE the record (the server removes its WAV too), drop the row and empty day. */
  async function deleteRecord(record, article, button) {
    if (!(await confirmDialog(t("删除这条记录和它的录音？此操作不能撤销。"), { okText: t("删除"), danger: true }))) return;
    await busy(button, async () => {
      try {
        await api(`/api/records/${encodeURIComponent(record.id)}`, { method: "DELETE" });
        if (activeAudio?.article === article) stopActive();
        const group = article.parentElement;
        article.remove();
        if (group && !$(".rec", group)) group.remove();
        toast(t("已删除"), "ok");
        loadTotals();
        loadFacets();
      } catch { /* toast shown */ }
    });
  }

  /* ---- playback & waveform ---- */

  /** Play/pause this record; starting one stops whichever record was playing. */
  function togglePlay(article, record) {
    if (activeAudio && activeAudio.article === article) {
      if (activeAudio.audio.paused) {
        activeAudio.audio.play().catch(() => stopActive());
        setPlaying(article, true);
      } else {
        activeAudio.audio.pause();
        setPlaying(article, false);
      }
      return;
    }
    stopActive();
    const audio = new Audio(audioUrl(record));
    activeAudio = { article, audio, record };
    const expand = $(".x", article);
    if (expand) expand.hidden = false;
    setPlaying(article, true);
    audio.addEventListener("ended", () => stopActive());
    audio.addEventListener("timeupdate", () => updateProgress());
    audio.play().catch(() => {
      toast(t("无法播放这段录音"), "err");
      stopActive();
    });
    drawWaveforms(article, record);
  }

  function setPlaying(article, playing) {
    const button = $(".play", article);
    if (button) {
      button.replaceChildren(icon(playing ? "pause" : "play"));
      button.setAttribute("aria-label", playing ? t("暂停") : t("播放"));
    }
  }

  /** Stop and release the active <audio> (src removed so the browser drops the stream). */
  function stopActive() {
    if (!activeAudio) return;
    const { article, audio } = activeAudio;
    audio.pause();
    audio.removeAttribute("src");
    setPlaying(article, false);
    activeAudio = null;
  }

  /** timeupdate: slider position, time labels and waveform progress. */
  function updateProgress() {
    if (!activeAudio) return;
    const { article, audio, record } = activeAudio;
    const slider = $('input[type="range"]', article);
    if (audio.duration && Number.isFinite(audio.duration)) {
      slider.value = Math.round((audio.currentTime / audio.duration) * 1000);
      $(".t-dur", article).textContent = fmtDuration(audio.duration);
    }
    $(".t-cur", article).textContent = fmtDuration(audio.currentTime);
    drawWaveforms(article, record, audio.currentTime, audio.duration || 0);
  }

  /**
   * Min/max peaks for the waveform: fetch the WAV once (token header), parse
   * the PCM, reduce to 900 buckets of [min, max] in -1..1; cached per record.
   */
  async function fetchPeaks(record) {
    if (waveformCache.has(record.id)) return waveformCache.get(record.id);
    const response = await fetch(audioUrl(record), { headers: { "X-Voiceless-Token": TOKEN } });
    if (!response.ok) throw new Error(`audio ${response.status}`);
    const pcm = extractPcm16(await response.arrayBuffer());
    if (!pcm || !pcm.length) throw new Error("no pcm data");
    const buckets = 900;
    const peaks = new Float32Array(buckets * 2);
    const perBucket = Math.max(1, Math.floor(pcm.length / buckets));
    for (let i = 0; i < buckets; i++) {
      let min = 0;
      let max = 0;
      const start = i * perBucket;
      const end = Math.min(pcm.length, start + perBucket);
      for (let j = start; j < end; j++) {
        if (pcm[j] < min) min = pcm[j];
        if (pcm[j] > max) max = pcm[j];
      }
      peaks[i * 2] = min;
      peaks[i * 2 + 1] = max;
    }
    waveformCache.set(record.id, peaks);
    return peaks;
  }

  /**
   * Draw peaks as 3 px-pitch bars on a canvas sized to its CSS box x DPR.
   * Each bar spans the peak buckets it covers, so the same 900 buckets fit
   * any width; in the active record, the bars up to `progress` take the
   * signal colour.
   */
  function paintWave(canvas, peaks, progress, active) {
    const dpr = window.devicePixelRatio || 1;
    const width = canvas.clientWidth;
    const height = canvas.clientHeight;
    if (!width || !height) return;
    canvas.width = width * dpr;
    canvas.height = height * dpr;
    const ctx = canvas.getContext("2d");
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    const mid = height / 2;
    const source = peaks.length / 2;
    const bars = Math.max(1, Math.floor(width / 3));
    const span = source / bars;
    const played = new Path2D();
    const rest = new Path2D();
    for (let i = 0; i < bars; i++) {
      let low = 0;
      let high = 0;
      // At least one bucket per bar, so narrow canvases keep every syllable.
      for (let bucket = Math.floor(i * span); bucket < Math.max(Math.floor((i + 1) * span), Math.floor(i * span) + 1) && bucket < source; bucket++) {
        low = Math.min(low, peaks[bucket * 2]);
        high = Math.max(high, peaks[bucket * 2 + 1]);
      }
      const x = i * 3 + 1.5;
      const path = active && i / bars <= progress ? played : rest;
      path.moveTo(x, mid + low * (mid - 1) || mid - 0.6);
      path.lineTo(x, mid + high * (mid - 1) || mid + 0.6);
    }
    ctx.lineWidth = 1.6;
    ctx.lineCap = "round";
    ctx.strokeStyle = cssColor("--ink-3");
    ctx.globalAlpha = active ? 0.45 : 0.6;
    ctx.stroke(rest);
    ctx.strokeStyle = cssColor("--sig");
    ctx.globalAlpha = 1;
    ctx.stroke(played);
  }

  /** Paint every canvas in the row and wire the seek slider once; a no-op without peaks. */
  async function drawWaveforms(article, record, currentTime = 0, duration = 0) {
    let peaks;
    try {
      peaks = await fetchPeaks(record);
    } catch {
      return; // playback still works without a waveform
    }
    const active = Boolean(activeAudio && activeAudio.record.id === record.id);
    const progress = duration > 0 ? currentTime / duration : 0;
    for (const canvas of $$("canvas", article)) {
      if (canvas.clientWidth) paintWave(canvas, peaks, progress, active);
    }
    const slider = $('input[type="range"]', article);
    if (slider && !slider.dataset.wired) {
      slider.dataset.wired = "1";
      slider.addEventListener("input", () => {
        if (!activeAudio || activeAudio.record.id !== record.id) return;
        const audio = activeAudio.audio;
        if (audio.duration && Number.isFinite(audio.duration)) audio.currentTime = (slider.value / 1000) * audio.duration;
      });
    }
  }

  /* ---- search and filters ---- */

  /** Whether any filter is set. */
  const active = () => Object.values(filter).some(Boolean);

  /** The search as query parameters: each filter that is set, the days as the UTC instants of their bounds. */
  function searchParams() {
    const params = new URLSearchParams();
    if (filter.text.trim()) params.set("q", filter.text.trim());
    // The server compares UTC instants; a day is the user's own, from its first second to the next day's.
    if (filter.from) params.set("from", dayStartUtc(filter.from));
    if (filter.to) params.set("to", dayStartUtc(filter.to, 1));
    if (filter.mode) params.set("mode", filter.mode);
    if (filter.app) params.set("app", filter.app);
    if (filter.model) params.set("model", filter.model);
    if (filter.audio) params.set("audio", filter.audio);
    return params;
  }

  /** Run the search from its first page: the rows of the previous one are dropped. */
  function search() {
    if (filter.from && filter.to && filter.to < filter.from) {
      toast(t("结束日期不能早于开始日期"), "warn");
      return;
    }
    for (const key of ["mode", "app", "model", "audio"]) filter[key] = filter[key] || null;
    run += 1;
    cursor = null;
    exhausted = false;
    clearButton.disabled = !active();
    feed.replaceChildren();
    loadPage();
  }

  /** The search box and the filters over the whole library; each change runs the search again from its first page. */
  function searchBar() {
    const labeled = (label, control) => h("label", { class: "hist-filter" }, h("span", { text: label }), control);
    const day = (key, label) => labeled(label, h("input", {
      type: "date", class: "inpt", value: filter[key] || "",
      onchange: (event) => { filter[key] = event.target.value || null; search(); },
    }));
    const menu = (key, label, values, name = (value) => value) => labeled(label, selectInput(filter, key,
      [["", t("全部")], ...values.map((facet) => [facet.value, name(facet.value)])], search));
    clearButton = btn("x", t("清除筛选"), { cls: "ghost", onclick: clearFilters, disabled: !active() });
    return h(
      "div",
      { class: "hist-search", role: "search", "aria-label": t("筛选记录") },
      h("div", { class: "inline" },
        icon("search"),
        h("input", {
          type: "search", class: "inpt", value: filter.text, placeholder: t("搜索原文和最终文字"), "aria-label": t("搜索记录"),
          oninput: (event) => { filter.text = event.target.value; rerunSoon(); },
          // Enter looks at once instead of after the pause the typing gets.
          onkeydown: (event) => {
            if (event.key !== "Enter") return;
            event.preventDefault();
            rerunSoon.cancel();
            search();
          },
        })),
      h("div", { class: "hist-filters" },
        day("from", t("开始日期")),
        day("to", t("结束日期")),
        menu("mode", t("模式"), facets.modes, modeName),
        menu("app", t("应用程序"), facets.apps),
        menu("model", t("模型"), facets.models),
        labeled(t("录音"), selectInput(filter, "audio", [["", t("全部")], ["with", t("有录音")], ["without", t("无录音")]], search)),
        h("div", { class: "head-actions" },
          clearButton,
          btn("download", t("导出 CSV"), { cls: "ghost", onclick: (event) => exportCsv(event.currentTarget) }))),
    );
  }

  /** Read what the menus offer and rebuild the bar when they changed; a selected value no row carries any more is dropped and the search runs again. */
  async function loadFacets() {
    let next;
    try {
      const loaded = await api("/api/records/facets", { quiet: true });
      next = { modes: loaded.modes || [], apps: loaded.apps || [], models: loaded.models || [] };
    } catch {
      return; // the search box and the days work without the menus
    }
    if (same(next, facets)) return;
    facets = next;
    let dropped = false;
    for (const key of ["mode", "app", "model"]) {
      if (filter[key] && !facets[`${key}s`].some((facet) => facet.value === filter[key])) {
        filter[key] = null;
        dropped = true;
      }
    }
    searchSlot.replaceChildren(searchBar());
    if (dropped) search();
  }

  /** Back to the whole library: the filters are emptied and the search runs from its first page. */
  function clearFilters() {
    Object.assign(filter, { text: "", from: null, to: null, mode: null, app: null, model: null, audio: null });
    searchSlot.replaceChildren(searchBar());
    search();
  }

  /** Download every row of the current search as CSV: the server selects them with the parameters the list uses. */
  async function exportCsv(button) {
    await busy(button, async () => {
      try {
        const { csv, exported } = await api(`/api/records/export?${searchParams()}`, { quiet: true });
        downloadText("voiceless-history.csv", csv, "text/csv");
        toast(t(`已导出 ${exported} 条记录`), "ok");
      } catch (error) {
        toast(t(`导出失败：${humanError(error)}`), "err");
      }
    });
  }

  /** Fetch the next page of the current search with the cursor; appends its rows and updates the footer status/button. */
  async function loadPage() {
    if (pending === run || exhausted) return;
    pending = run;
    const mine = run;
    feedStatus.replaceChildren(emptyState("history", t("读取中…")));
    try {
      const params = searchParams();
      params.set("limit", "100");
      if (cursor) params.set("cursor", cursor);
      const body = await api(`/api/records?${params}`, { quiet: true });
      if (mine !== run) return; // the filter changed while this page was on its way
      const filtered = active();
      if (!body.records.length && !cursor) {
        feed.replaceChildren(emptyState(filtered ? "search" : "mic", filtered ? t("没有符合筛选的记录") : t("说一句话，记录就会出现在这里")));
        feedStatus.replaceChildren();
        exhausted = true;
        return;
      }
      body.records.forEach(recordRow);
      cursor = body.next_cursor;
      exhausted = !cursor;
      const end = filtered ? t("没有更多符合条件的记录") : t("已经到最早的一条");
      feedStatus.replaceChildren(h("div", { class: "empty", style: { padding: "18px 0" } }, exhausted ? end : btn("chevron-down", t("更早的记录"), { cls: "ghost" })));
      if (!exhausted) $(".btn", feedStatus).addEventListener("click", loadPage);
    } catch (error) {
      if (mine === run) feedStatus.replaceChildren(note("circle-x", t(`读取失败：${humanError(error)}`), "err"));
    } finally {
      if (pending === mine) pending = 0;
    }
  }

  /** [history] enabled / save_audio as two icon toggles in the title row, saved on change. */
  async function settingsToggles() {
    let config;
    try {
      config = await api("/api/config", { quiet: true });
    } catch {
      return [];
    }
    const historyConfig = clone(config.history || {});
    const make = (key, iconName, label) => {
      const button = ibtn(iconName, { label });
      const render = () => button.setAttribute("aria-pressed", String(Boolean(historyConfig[key])));
      button.addEventListener("click", () => busy(button, async () => {
        const value = !historyConfig[key];
        try {
          const saved = await api("/api/config", { method: "PUT", body: { revision: config.revision, fields: { [`history.${key}`]: value } } });
          config.revision = saved.revision;
          historyConfig[key] = value;
          toast(t("记录设置已保存"), "ok");
        } catch { /* toast shown */ }
        render();
      }));
      render();
      return button;
    };
    return [make("enabled", "bookmark", t("保存识别记录")), make("save_audio", "mic", t("保存录音"))];
  }

  return {
    async mount(target) {
      view = target;
      feed = h("div", {});
      searchSlot = h("div", {});
      feedStatus = h("div", { class: "feed-status" });
      const refreshHistory = () => { stopActive(); search(); loadTotals(); loadFacets(); };
      view.append(pageHead(t("历史"), totalsEl, ...(await settingsToggles())), searchSlot,
        h("div", { class: "head-actions" },
          btn("download", t("导出记录"), { cls: "ghost", onclick: () => exportHistory(searchParams(), $$("article.rec", feed)) }),
          btn("clock", t("保留策略"), { cls: "ghost", onclick: () => retentionSettings(refreshHistory) }),
          btn("mic", t("失败录音"), { cls: "ghost", onclick: recoveryList })),
        feed, feedStatus);
      loadTotals();
      await getModes(); // the rows show mode names
      searchSlot.append(searchBar());
      loadFacets();
      search();
    },
    unmount() {
      rerunSoon.cancel();
      stopActive();
    },

    // Waveforms are drawn on demand with the colours of their time.
    themeChanged() {
      for (const article of $$("article.rec", feed)) {
        const peaks = waveformCache.get(article.dataset.id);
        if (!peaks) continue;
        const playing = activeAudio && activeAudio.article === article;
        const progress = playing && activeAudio.audio.duration ? activeAudio.audio.currentTime / activeAudio.audio.duration : 0;
        for (const canvas of $$("canvas", article)) {
          if (canvas.clientWidth) paintWave(canvas, peaks, progress, Boolean(playing));
        }
      }
    },
  };
}

/**
 * The first second of the local day `YYYY-MM-DD` as RFC 3339 UTC on a whole
 * second, the form the history search takes its time bounds in; `later`
 * counts days on from it, for the exclusive end of a range.
 */
function dayStartUtc(day, later = 0) {
  const [year, month, date] = day.split("-").map(Number);
  return new Date(year, month - 1, date + later).toISOString().replace(".000Z", "Z");
}

/**
 * PCM16 mono payload of a WAV container as Float32 in -1..1: walks the RIFF
 * chunks (padding odd sizes) to the "data" chunk; null if not a WAV or no
 * data chunk. Recordings are 16 kHz mono PCM16, so no format chunk is read.
 */
function extractPcm16(buffer) {
  if (buffer.byteLength < 12) return null;
  const view = new DataView(buffer);
  const tag = (offset) => String.fromCharCode(...new Uint8Array(buffer, offset, 4));
  if (tag(0) !== "RIFF" || tag(8) !== "WAVE") return null;
  let offset = 12;
  while (offset + 8 <= buffer.byteLength) {
    const id = tag(offset);
    const size = view.getUint32(offset + 4, true);
    if (id === "data") {
      const start = offset + 8;
      const end = Math.min(buffer.byteLength, start + size);
      const samples = new Float32Array(Math.floor((end - start) / 2));
      for (let i = 0; i < samples.length; i++) samples[i] = view.getInt16(start + i * 2, true) / 32768;
      return samples;
    }
    offset += 8 + size + (size % 2);
  }
  return null;
}

/* ====================================================================== */
/* page: 诊断                                                              */
/* ====================================================================== */

/**
 * Diagnostics page: doctor checks (/api/doctor), engine + GPU (from
 * /api/status), the daemon log (/api/logs, copyable) and the replay
 * benchmark (/api/benchmark) rendered as RTF / CER with reference diffs.
 */
function DiagnosticsPage() {
  let view = null;
  const doctorSlot = h("div", {}, emptyState("circle-check", t("检查中…")));
  const gpuSlot = h("div", {}, emptyState("cpu", t("读取中…")));
  const logPre = h("pre", { class: "logbox", tabindex: "0", "aria-label": t("后台日志") });
  const benchSlot = h("div");
  let lastStatus = null;
  let unsubscribeRuntime = null;
  const exportButton = ibtn("download", { label: t("导出诊断"), disabled: true, onclick: exportDiagnostics });

  /** Doctor list: pass / required-fail / optional-missing per check. */
  async function loadDoctor(button) {
    await busy(button, async () => {
      try {
        const checks = await api("/api/doctor", { quiet: true });
        doctorSlot.replaceChildren(
          h("div", { class: "checks" }, ...checks.map((check) => {
            const ok = check.present;
            const [iconName, cls] = ok ? ["circle-check", "ok"] : check.required ? ["circle-x", "fail"] : ["minus", "opt"];
            return h(
              "div",
              { class: "check" },
              tip(h("span", { class: cls }, icon(iconName)), ok ? t("已就绪") : check.required ? t("缺少，必需") : t("未安装，可选"), "t"),
              h("span", { text: check.name }),
              check.detail ? h("span", { class: "detail", text: check.detail }) : null,
            );
          })),
        );
      } catch (error) {
        doctorSlot.replaceChildren(note("circle-x", humanError(error), "err"));
      }
    });
  }

  function renderDiagnostics(status) {
    if (!status) return;
    lastStatus = status;
    exportButton.disabled = false;
    const gpus = status.gpu || [];
    setChildren(gpuSlot, recognitionDetails(status),
      status.version ? h("p", { class: "mono", text: `Voiceless ${status.version}` }) : null,
      gpus.length ? h("div", { class: "stack" }, ...gpus.map(gpuBar)) : null,
    );
  }

  async function loadGpu() {
    try {
      renderDiagnostics(await fetchStatus());
    } catch (error) {
      gpuSlot.replaceChildren(note("circle-x", humanError(error), "err"));
    }
  }

  function exportDiagnostics() {
    if (!lastStatus) return;
    const recognition = effectiveRecognition(lastStatus);
    const device = (value) => value ? {
      engine: value.engine,
      requested: value.requested,
      api: value.api,
      cpu_fallback: value.cpu_fallback,
    } : null;
    const report = {
      version: lastStatus.version,
      configured_backend: lastStatus.backend,
      effective_backend: recognition.backend,
      requested_device: recognition.requested,
      primary_device: device(recognition.devices?.primary),
      preview_device: device(recognition.devices?.preview),
      fallback_from: recognition.fallback?.primary ?? null,
      engine_error: Boolean(recognition.error),
    };
    downloadText("voiceless-diagnostics.json", JSON.stringify(report, null, 2), "application/json");
  }

  /** Last 500 daemon log lines, scrolled to the end. */
  async function loadLogs(button) {
    await busy(button, async () => {
      try {
        const logs = await api("/api/logs", { quiet: true });
        logPre.textContent = (logs.lines || []).join("\n") || t("（暂无日志）");
        logPre.scrollTop = logPre.scrollHeight;
      } catch (error) {
        logPre.textContent = isDaemonDown(error) ? t("后台进程未运行。可以用 journalctl --user -u voiceless 查看系统日志。") : humanError(error);
      }
    });
  }

  /** Clipboard copy with a select-all fallback when the clipboard API is unavailable. */
  async function copyLogs() {
    try {
      await navigator.clipboard.writeText(logPre.textContent);
      toast(t("日志已复制"), "ok");
    } catch {
      const range = document.createRange();
      range.selectNodeContents(logPre);
      const selection = getSelection();
      selection.removeAllRanges();
      selection.addRange(range);
      toast(t("已选中日志，按 Ctrl+C 复制"));
    }
  }

  /** POST /api/benchmark over the last N recordings (1..50, remembered); blocks for the run. */
  async function runBenchmark(button, limitInput) {
    const limit = Math.max(1, Math.min(50, Number(limitInput.value) || 10));
    storage.set("benchLimit", limit);
    benchSlot.replaceChildren(emptyState("gauge", t(`正在识别最近 ${limit} 条录音…`)));
    await busy(button, async () => {
      try {
        renderBenchmark(await api("/api/benchmark", { method: "POST", body: { limit } }));
      } catch (error) {
        benchSlot.replaceChildren(note("circle-x", isDaemonDown(error) ? t("需要后台进程加载本地模型后才能运行") : humanError(error), "err"));
      }
    });
  }

  /** Summary readouts plus a per-record table; CER chips go ok / warn / err at 5% and 15%. */
  function renderBenchmark(result) {
    const rows = result.results || [];
    if (!rows.length) {
      benchSlot.replaceChildren(emptyState("mic", t("历史记录中需要有保存了录音的条目")));
      return;
    }
    const cerClass = (cer) => (cer == null ? "" : cer <= 0.05 ? "ok" : cer <= 0.15 ? "warn" : "err");
    const stats = [
      ["gauge", t("平均实时率（识别耗时 ÷ 语音时长，越小越快）"), fmtSeconds(result.mean_rtf, 3), ""],
      ["target", t("平均字错率（以当时的识别原文为参考）"), result.mean_cer != null ? (result.mean_cer * 100).toFixed(1) : "–", "%"],
      ["layers", t("条数"), String(rows.length), ""],
    ];
    const readouts = h("div", { class: "readouts", style: { marginBottom: "18px" } });
    stats.forEach(([iconName, label, value, unit]) => {
      const ro = readout(iconName, label);
      ro.set(value, unit);
      readouts.append(ro.el);
    });
    benchSlot.replaceChildren(
      readouts,
      h("div", { class: "inline", style: { marginBottom: "12px" } }, icon("cpu", 15), h("span", { class: "mono", style: { fontSize: "13px" }, text: result.engine || "–" })),
      h("div", { class: "tbl-wrap" }, h(
        "table",
        { class: "tbl" },
        h("thead", {}, h("tr", {}, h("th", { class: "num", text: t("时长") }), h("th", { class: "num", text: t("耗时") }), h("th", { class: "num", text: t("实时率") }), h("th", { class: "num", text: t("字错率") }), tip(h("th", { text: t("对比") }), t("红色：参考，绿色：本次"), "t"))),
        h("tbody", {}, rows.map((row) =>
          h(
            "tr",
            {},
            h("td", { class: "num" }, `${fmtSeconds(row.audio_seconds)}s`),
            h("td", { class: "num" }, `${fmtSeconds(row.decode_seconds, 2)}s`),
            h("td", { class: "num" }, fmtSeconds(row.rtf, 3)),
            h("td", { class: "num" }, h("span", { class: `chip ${cerClass(row.cer)}` }, row.cer != null ? `${(row.cer * 100).toFixed(1)}%` : "–")),
            h("td", { style: { minWidth: "240px", fontSize: "13.5px" } }, renderDiff(row.reference || "", row.text || "")),
          ))),
      )),
    );
  }

  return {
    async mount(target) {
      view = target;
      const doctorButton = ibtn("refresh-cw", { label: t("重新检查"), cls: "ghost" });
      const logButton = ibtn("refresh-cw", { label: t("刷新日志"), cls: "ghost" });
      const copyButton = ibtn("copy", { label: t("复制日志"), cls: "ghost" });
      const limitInput = h("input", { type: "number", class: "inpt", min: "1", max: "50", value: String(storage.get("benchLimit", 10)), style: { width: "84px", flex: "none" }, "aria-label": t("录音条数") });
      const benchButton = ibtn("play", { label: t("运行回放基准"), cls: "primary lg" });
      benchButton.addEventListener("click", () => runBenchmark(benchButton, limitInput));
      view.append(
        pageHead(t("诊断"), exportButton),
        group("circle-check", t("环境检查"), [doctorButton], doctorSlot),
        h("div", { class: "grid-2" }, group("cpu", t("引擎与 GPU"), null, gpuSlot), group("terminal", t("日志"), [copyButton, logButton], logPre)),
        group("gauge", t("回放基准"), null,
          h("div", { class: "inline", style: { marginBottom: "16px" } }, icon("layers", 16), limitInput, h("span", { class: "lab", text: t("条最近的录音") }), benchButton),
          benchSlot),
      );
      unsubscribeRuntime = live.subscribe((event) => {
        if (recognitionChanged(event)) renderDiagnostics(app.status);
      });
      await Promise.all([loadDoctor(doctorButton), loadGpu(), loadLogs(logButton)]);
    },
    unmount() {
      unsubscribeRuntime?.();
    },
  };
}

export {
  DiagnosticsPage,
  EnginePage,
  HistoryPage,
  HotkeyPage,
  LivePage,
  ModesPage,
  NATIVE,
  NS,
  TextPage,
  api,
  announce,
  app,
  btn,
  confirmDialog,
  deck,
  deliveryText,
  emptyState,
  downloadText,
  fmtBytes,
  fmtDuration,
  fetchStatus,
  fillDeckModes,
  group,
  h,
  humanError,
  icon,
  live,
  note,
  openDialog,
  pageHead,
  renderLamp,
  row,
  setChildren,
  storage,
  tiles,
  tip,
  uploadFile,
  toast,
  withToken,
};
