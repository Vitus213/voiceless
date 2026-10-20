// Voiceless console: a dependency-free single-page app over the daemon's
// local API (docs/console-api.md). Hash routing, one module-level object
// per page, shared components (api, toast, dialogs, tag input, table
// editor, save bar) at the top.
"use strict";

/* ====================================================================== */
/* basics                                                                 */
/* ====================================================================== */

const TOKEN = document.querySelector('meta[name="voiceless-token"]')?.content || "";
const $ = (selector, root = document) => root.querySelector(selector);
const $$ = (selector, root = document) => [...root.querySelectorAll(selector)];

/// Element builder: h("div", { class: "x", onclick: fn }, child, "text").
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

const storage = {
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

const clone = (value) => (value == null ? value : JSON.parse(JSON.stringify(value)));
const same = (a, b) => JSON.stringify(a) === JSON.stringify(b);
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

function debounce(fn, ms) {
  let timer = null;
  const wrapped = (...args) => {
    clearTimeout(timer);
    timer = setTimeout(() => fn(...args), ms);
  };
  wrapped.cancel = () => clearTimeout(timer);
  return wrapped;
}

function fmtDuration(seconds) {
  if (seconds == null || Number.isNaN(seconds)) return "–:––";
  const s = Math.round(seconds);
  return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, "0")}`;
}

function fmtSeconds(seconds, digits = 1) {
  if (seconds == null || Number.isNaN(seconds)) return "–";
  return `${Number(seconds).toFixed(digits)}`;
}

function fmtBytes(bytes) {
  if (bytes == null) return "–";
  if (bytes >= 1e9) return `${(bytes / 1e9).toFixed(2)} GB`;
  if (bytes >= 1e6) return `${(bytes / 1e6).toFixed(0)} MB`;
  return `${(bytes / 1e3).toFixed(0)} KB`;
}

function fmtUptime(seconds) {
  if (seconds == null) return "–";
  const d = Math.floor(seconds / 86400);
  const hrs = Math.floor((seconds % 86400) / 3600);
  const min = Math.floor((seconds % 3600) / 60);
  if (d) return `${d} 天 ${hrs} 小时`;
  if (hrs) return `${hrs} 小时 ${min} 分`;
  return `${min} 分钟`;
}

const BACKENDS = [
  ["local-offline", "本地离线模型（GPU，推荐）"],
  ["doubao-realtime", "豆包流式（火山引擎）"],
  ["cloud-realtime", "百炼实时（阿里云）"],
  ["local-streaming", "本地流式 Paraformer（CPU）"],
];

function backendLabel(id) {
  return {
    "local-offline": "本地离线",
    "doubao-realtime": "豆包",
    "cloud-realtime": "百炼",
    "local-streaming": "本地流式",
  }[id] || id || "未知";
}

const STATE_LABELS = { idle: "空闲", recording: "录音中", stopping: "识别中", down: "后台未运行" };

/* ====================================================================== */
/* API                                                                    */
/* ====================================================================== */

class ApiError extends Error {
  constructor(status, message) {
    super(message);
    this.status = status;
  }
}

const isDaemonDown = (error) => error instanceof ApiError && error.status === 503;
const withToken = (path) => `${path}${path.includes("?") ? "&" : "?"}token=${encodeURIComponent(TOKEN)}`;

function humanError(error) {
  if (isDaemonDown(error)) return "后台进程未运行（voiceless daemon），该功能暂不可用";
  if (error instanceof ApiError && error.status === 403) return "令牌无效，请刷新页面";
  return error.message || String(error);
}

/// Every /api call: token header, JSON in and out, `{"error"}` on non-2xx
/// becomes an ApiError (and a toast unless `quiet`).
async function api(path, { method = "GET", body, quiet = false } = {}) {
  const init = { method, headers: { "X-Voiceless-Token": TOKEN } };
  if (body !== undefined) {
    init.headers["Content-Type"] = "application/json";
    init.body = JSON.stringify(body);
  }
  let response;
  try {
    response = await fetch(path, init);
  } catch {
    const error = new ApiError(0, "无法连接控制台服务");
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
    const error = new ApiError(response.status, (data && data.error) || `HTTP ${response.status}`);
    if (!quiet) toast(humanError(error), "err");
    throw error;
  }
  return data;
}

/* ====================================================================== */
/* components                                                             */
/* ====================================================================== */

function toast(message, kind = "info", ms = 4200) {
  const el = h("div", { class: `toast ${kind}`, role: kind === "err" ? "alert" : "status" }, message);
  $("#toasts").append(el);
  requestAnimationFrame(() => el.classList.add("in"));
  setTimeout(() => {
    el.classList.remove("in");
    setTimeout(() => el.remove(), 250);
  }, ms);
}

/// Modal dialog. `actions`: [{ label, value, class, close: false, onClick }].
/// Resolves with the value of the closing action ("" on Escape).
function openDialog({ title, body, actions = [], onOpen }) {
  const dialog = $("#dialog");
  const buttons = actions.map((action) =>
    h(
      "button",
      {
        type: action.close === false ? "button" : "submit",
        value: action.value ?? "",
        class: `btn ${action.class || ""}`,
        onclick: action.onClick,
      },
      action.label,
    ),
  );
  const content = typeof body === "string" ? h("p", { text: body }) : body;
  dialog.replaceChildren(
    h("form", { method: "dialog", class: "dialog-inner" }, h("h3", { text: title }), content, h("div", { class: "dialog-actions" }, buttons)),
  );
  dialog.returnValue = "";
  dialog.showModal();
  onOpen?.(dialog);
  return new Promise((resolve) => {
    dialog.addEventListener("close", () => resolve(dialog.returnValue), { once: true });
  });
}

async function confirmDialog(message, { title = "请确认", okText = "确定", danger = false } = {}) {
  const result = await openDialog({
    title,
    body: message,
    actions: [
      { label: "取消", value: "" },
      { label: okText, value: "ok", class: danger ? "danger" : "primary" },
    ],
  });
  return result === "ok";
}

/// Runs `fn` with `button` in its loading state.
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

function card(title, section, ...children) {
  return h(
    "section",
    { class: "card" },
    h("h2", {}, title, section ? h("span", { class: "section", text: section }) : null),
    ...children,
  );
}

function field(label, control, { help, wide = false } = {}) {
  const id = control.id || `f-${Math.random().toString(36).slice(2, 9)}`;
  const target = control.matches?.("input, select, textarea") ? control : control.querySelector?.("input, select, textarea");
  if (target && !target.id) target.id = id;
  return h(
    "div",
    { class: `field${wide ? " wide" : ""}` },
    h("label", { for: target?.id || null, text: label }),
    control,
    help ? h("div", { class: "help", text: help }) : null,
  );
}

function pageHead(title, sub, ...actions) {
  return h(
    "header",
    { class: "page-head" },
    h("div", {}, h("h1", { text: title }), sub ? h("div", { class: "sub", text: sub }) : null),
    actions.length ? h("div", { class: "head-actions" }, actions) : null,
  );
}

function emptyState(big, text) {
  return h("div", { class: "empty" }, h("div", { class: "big", text: big }), text ? h("div", {}, text) : null);
}

/* ---- form bindings on a draft object; each calls `changed()` ---- */

function textInput(obj, key, changed, { placeholder = "", mono = false, type = "text", nullable = false } = {}) {
  return h("input", {
    type,
    class: mono ? "mono" : null,
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

function numberInput(obj, key, changed, { min, max, step = 1, nullable = false, placeholder = "" } = {}) {
  return h("input", {
    type: "number",
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

/// options: [[value, label]]; value null maps to "" in the DOM.
function selectInput(obj, key, options, changed) {
  const select = h(
    "select",
    {
      onchange: (event) => {
        const option = options[event.target.selectedIndex];
        obj[key] = option ? option[0] : null;
        changed();
      },
    },
    options.map(([value, label]) => h("option", { value: value ?? "" }, label)),
  );
  const index = options.findIndex(([value]) => (value ?? null) === (obj[key] ?? null));
  select.selectedIndex = index >= 0 ? index : 0;
  return select;
}

function toggleInput(obj, key, changed, label, sub) {
  return h(
    "label",
    { class: "switch" },
    h("input", {
      type: "checkbox",
      checked: Boolean(obj[key]),
      onchange: (event) => {
        obj[key] = event.target.checked;
        changed();
      },
    }),
    h("span", { class: "track", "aria-hidden": "true" }),
    h("span", {}, label, sub ? h("span", { class: "sub", text: sub }) : null),
  );
}

function rangeInput(obj, key, changed, { min, max, step, format = (v) => v }) {
  const output = h("output", { text: format(obj[key]) });
  const input = h("input", {
    type: "range",
    min,
    max,
    step,
    value: obj[key],
    "aria-valuetext": format(obj[key]),
    oninput: (event) => {
      obj[key] = Number(event.target.value);
      output.textContent = format(obj[key]);
      event.target.setAttribute("aria-valuetext", format(obj[key]));
      changed();
    },
  });
  return h("div", { class: "range-line" }, input, output);
}

/// Write-only secret: "" keeps the stored value, null clears it.
function secretInput(obj, key, hasValue, changed) {
  if (obj[key] === undefined) obj[key] = "";
  const chip = h("span", { class: "chip" });
  const input = h("input", {
    type: "password",
    autocomplete: "new-password",
    placeholder: hasValue ? "已设置，留空则不修改" : "未设置",
    oninput: (event) => {
      obj[key] = event.target.value;
      render();
      changed();
    },
  });
  const clear = h(
    "button",
    {
      type: "button",
      class: "btn small ghost",
      onclick: () => {
        input.value = "";
        obj[key] = obj[key] === null ? "" : null;
        render();
        changed();
      },
    },
    "清除",
  );
  function render() {
    if (obj[key] === null) {
      chip.className = "chip err";
      chip.textContent = "保存后清除";
      clear.textContent = "撤销清除";
    } else if (obj[key]) {
      chip.className = "chip warn";
      chip.textContent = "将更新";
      clear.textContent = "清除";
    } else {
      chip.className = hasValue ? "chip ok" : "chip dashed";
      chip.textContent = hasValue ? "已设置" : "未设置";
      clear.textContent = "清除";
    }
    clear.hidden = !hasValue && obj[key] !== null;
  }
  render();
  // A form keeps browsers' password heuristics quiet; it is never submitted.
  return h("form", { class: "secret", onsubmit: (event) => event.preventDefault() }, input, chip, clear);
}

/* ---- tag input ---- */

function tagInput(values, onChange, placeholder = "输入后按回车添加") {
  let tags = [...(values || [])];
  const list = h("div", { class: "tags" });
  const entry = h("input", { class: "tag-entry", placeholder, "aria-label": placeholder });
  const wrap = h("div", {
    class: "tag-input",
    onclick: (event) => {
      if (event.target === wrap) entry.focus();
    },
  }, list);

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
            "aria-label": `删除 ${tag}`,
            onclick: () => {
              tags.splice(index, 1);
              render();
              emit();
              entry.focus();
            },
          }, "×"),
        ),
      ),
      entry,
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
    el: wrap,
    set(next) {
      tags = [...(next || [])];
      render();
    },
  };
}

/* ---- table editor ---- */

/// columns: [{ key, label, placeholder, mono }]. Rows are plain objects.
function tableEditor({ columns, rows, onChange, emptyText = "还没有条目", addLabel = "添加一行" }) {
  let data = (rows || []).map((row) => ({ ...row }));
  const tbody = h("tbody");
  const table = h(
    "table",
    { class: "edit-table" },
    h("thead", {}, h("tr", {}, columns.map((column) => h("th", { text: column.label })), h("th", { "aria-label": "操作" }))),
    tbody,
  );
  const empty = h("div", { class: "table-empty", text: emptyText });
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
                class: column.mono ? "mono" : null,
                value: row[column.key] ?? "",
                placeholder: column.placeholder || "",
                spellcheck: "false",
                "aria-label": `${column.label}（第 ${index + 1} 行）`,
                oninput: (event) => {
                  row[column.key] = event.target.value;
                  onChange(snapshot());
                },
              }),
            ),
          ),
          h(
            "td",
            { class: "row-actions" },
            h("button", {
              type: "button",
              class: "icon-btn danger",
              title: "删除此行",
              "aria-label": `删除第 ${index + 1} 行`,
              onclick: () => {
                data.splice(index, 1);
                render();
                onChange(snapshot());
              },
            }, "×"),
          ),
        ),
      ),
    );
    table.hidden = data.length === 0;
    empty.hidden = data.length > 0;
  }

  const add = h("button", {
    type: "button",
    class: "btn small",
    onclick: () => {
      data.push(Object.fromEntries(columns.map((column) => [column.key, ""])));
      render();
      onChange(snapshot());
      tbody.lastElementChild?.querySelector("input")?.focus();
    },
  }, `＋ ${addLabel}`);

  render();
  return {
    el: h("div", { class: "table-editor" }, table, empty, add),
    set(next) {
      data = (next || []).map((row) => ({ ...row }));
      render();
    },
    markInvalid(index, message) {
      $$("tr", tbody).forEach((tr, i) => {
        tr.classList.toggle("invalid", i === index);
        tr.title = i === index ? message : "";
      });
    },
    clearInvalid() {
      $$("tr", tbody).forEach((tr) => {
        tr.classList.remove("invalid");
        tr.title = "";
      });
    },
  };
}

/* ---- save bar ---- */

function saveBar({ onSave, onDiscard, message = "有未保存的更改" }) {
  const save = h("button", { type: "button", class: "btn primary small" }, "保存");
  const discard = h("button", { type: "button", class: "btn ghost small" }, "放弃");
  const el = h("div", { class: "savebar", role: "region", "aria-label": "保存更改" }, h("span", { class: "msg", text: message }), discard, save);
  save.addEventListener("click", () => busy(save, onSave));
  discard.addEventListener("click", async () => {
    if (await confirmDialog("放弃所有未保存的更改？", { okText: "放弃更改", danger: true })) onDiscard();
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

/* ---- character diff (LCS) for comparisons ---- */

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
/* live connection: one EventSource shared by every page                  */
/* ====================================================================== */

const app = {
  state: null, // idle | recording | stopping | down | null (unknown)
  mode: "quick",
  status: null,
};

const live = {
  source: null,
  listeners: new Set(),
  retry: 1000,
  connected: false,
  connect() {
    if (this.source) return;
    const source = new EventSource(withToken("/api/events"));
    this.source = source;
    source.onopen = () => {
      this.retry = 1000;
      this.connected = true;
      refreshNavStatus();
    };
    source.onmessage = (message) => {
      let event;
      try {
        event = JSON.parse(message.data);
      } catch {
        return;
      }
      if (event.type === "state") {
        app.state = event.state;
        if (event.mode) app.mode = event.mode;
        renderNavStatus();
      }
      for (const listener of this.listeners) {
        try {
          listener(event);
        } catch (error) {
          console.error(error);
        }
      }
    };
    source.onerror = () => {
      source.close();
      this.source = null;
      this.connected = false;
      refreshNavStatus();
      setTimeout(() => this.connect(), this.retry);
      this.retry = Math.min(this.retry * 2, 10_000);
    };
  },
  subscribe(listener) {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  },
};

async function fetchStatus() {
  try {
    const status = await api("/api/status", { quiet: true });
    app.status = status;
    app.state = status.state;
    app.mode = status.mode || app.mode;
    $("#version").textContent = status.version ? `v${status.version}` : "";
    return status;
  } catch (error) {
    app.status = null;
    if (isDaemonDown(error)) app.state = "down";
    throw error;
  } finally {
    renderNavStatus();
  }
}

const refreshNavStatus = debounce(() => fetchStatus().catch(() => {}), 300);

function renderNavStatus() {
  const el = $("#navStatus");
  const state = app.state || "unknown";
  el.className = `nav-status ${state}`;
  $(".label", el).textContent = STATE_LABELS[state] || (live.connected ? "已连接" : "未连接");
}

/* ====================================================================== */
/* shared helpers for config pages                                        */
/* ====================================================================== */

async function loadModes() {
  try {
    const modes = await api("/api/modes", { quiet: true });
    return [...modes].sort((a, b) => (a.sort_order ?? 0) - (b.sort_order ?? 0));
  } catch {
    return [{ id: "quick", name: "快速", builtin: true }];
  }
}

/// Removes the masked-secret markers before a config section is sent back.
function stripMarkers(section) {
  const out = {};
  for (const [key, value] of Object.entries(section || {})) {
    if (!key.startsWith("has_")) out[key] = value;
  }
  return out;
}

/// PUT the changed sections of `draft` relative to `saved`.
async function saveSections(saved, draft, sections) {
  const patch = {};
  for (const name of sections) {
    if (!same(saved[name], draft[name])) patch[name] = stripMarkers(draft[name]);
  }
  if (Object.keys(patch).length === 0) return { ok: true, restart_engine: false, changed: false };
  const result = await api("/api/config", { method: "PUT", body: patch });
  return { ...result, changed: true };
}

async function offerEngineReload() {
  const choice = await openDialog({
    title: "需要重新加载引擎",
    body: "这些更改涉及识别引擎、模型、热词或快捷键，重新加载后才会生效（大约几秒，期间无法录音）。",
    actions: [
      { label: "稍后", value: "" },
      { label: "立即重新加载", value: "reload", class: "primary" },
    ],
  });
  if (choice === "reload") await reloadEngine();
}

async function reloadEngine(button) {
  return busy(button, async () => {
    try {
      const result = await api("/api/engine/reload", { method: "POST" });
      toast(`引擎已重新加载：${result.engine || "—"}${result.load_seconds != null ? `（${fmtSeconds(result.load_seconds)} 秒）` : ""}`, "ok");
      refreshNavStatus();
      return result;
    } catch {
      return null;
    }
  });
}

/// Secrets come back as "" plus has_<key>; keep "" in the draft (= unchanged).
function secretFlag(section, key) {
  return Boolean(section?.[`has_${key}`]);
}

/* ====================================================================== */
/* page: 实时                                                              */
/* ====================================================================== */

function LivePage() {
  const els = {};
  let unsubscribe = null;
  let statusTimer = null;
  let frame = null;
  let levelTarget = 0;
  let level = 0;
  let logLines = [];
  let state = null;
  let modes = [];

  function setState(next) {
    state = next;
    const badge = els.badge;
    badge.className = `state-badge ${next || ""}`;
    $(".text", badge).textContent = STATE_LABELS[next] || "未知";
    const down = next === "down";
    els.banner.hidden = !down;
    els.primary.disabled = down || next === "stopping" || next == null;
    els.cancel.disabled = down || next === "idle" || next == null;
    els.mode.disabled = down || next !== "idle";
    if (next === "recording") {
      els.primary.className = "btn rec big";
      els.primary.textContent = "停止并识别";
    } else if (next === "stopping") {
      els.primary.className = "btn primary big";
      els.primary.textContent = "识别中…";
    } else {
      els.primary.className = "btn primary big";
      els.primary.textContent = "开始录音";
    }
    if (next !== "recording") levelTarget = 0;
  }

  function showTranscript(snapshot) {
    const box = els.transcript;
    const final = Boolean(snapshot.is_final);
    let confirmed = Array.isArray(snapshot.confirmed_segments) ? snapshot.confirmed_segments.join("") : "";
    let partial = snapshot.partial_text || "";
    if (final) {
      confirmed = snapshot.authoritative_text || confirmed + partial;
      partial = "";
    } else if (!confirmed && !partial) {
      confirmed = snapshot.authoritative_text || "";
    }
    box.classList.toggle("final", final);
    box.replaceChildren(
      h("span", { class: "confirmed", text: confirmed }),
      partial ? h("span", { class: "partial", text: partial }) : null,
    );
    box.scrollTop = box.scrollHeight;
  }

  function resetTranscript() {
    els.transcript.classList.remove("final");
    els.transcript.replaceChildren(h("span", { class: "placeholder", text: "正在聆听…" }));
    note("", "");
  }

  function note(text, kind = "") {
    els.note.textContent = text;
    els.note.className = `transcript-note ${kind}`;
  }

  function showSession(session) {
    if (!session) {
      els.statFinal.replaceChildren("–");
      els.statAudio.replaceChildren("–");
      els.statChars.replaceChildren("–");
      return;
    }
    els.statFinal.replaceChildren(session.stop_to_final_ms != null ? String(Math.round(session.stop_to_final_ms)) : "–", h("small", { text: "ms" }));
    els.statAudio.replaceChildren(fmtSeconds(session.audio_seconds), h("small", { text: "秒" }));
    els.statChars.replaceChildren(session.chars != null ? String(session.chars) : "–", h("small", { text: "字" }));
  }

  function renderEngine(status) {
    const engine = status.engine || {};
    const rows = [
      ["引擎", backendLabel(status.backend) + (status.fallback_backend ? ` · 备用 ${backendLabel(status.fallback_backend)}` : "")],
      ["模型", engine.description || (status.backend === "local-offline" ? "未加载" : "云端，无需加载")],
      ["加载耗时", engine.load_seconds != null ? `${fmtSeconds(engine.load_seconds)} 秒` : "–"],
      ["运行时长", fmtUptime(status.uptime_seconds)],
    ];
    const list = h("dl", { class: "kv" }, rows.map(([k, v]) => [h("dt", { text: k }), h("dd", { text: v })]));
    const children = [list];
    if (engine.error) {
      children.push(h("div", { class: "banner warn", style: { marginTop: "14px", marginBottom: "0" } }, h("span", { class: "banner-icon", text: "⚠" }), h("div", {}, h("b", { text: "引擎加载失败" }), h("p", { text: engine.error }))));
    }
    if (status.gpu && status.gpu.length) {
      children.push(h("div", { style: { marginTop: "14px" } }, status.gpu.map(gpuBar)));
    }
    els.engine.replaceChildren(...children);
  }

  function setLevel(rms) {
    // Perceptual scaling: speech RMS sits around 0.02–0.3.
    levelTarget = Math.min(1, Math.pow(Math.max(0, rms) * 3, 0.6));
  }

  function animate() {
    level += (levelTarget - level) * (levelTarget > level ? 0.5 : 0.12);
    levelTarget *= 0.92; // decays between level events
    els.meterFill.style.transform = `scaleX(${level.toFixed(3)})`;
    frame = requestAnimationFrame(animate);
  }

  function appendLog(line) {
    logLines.push(line);
    if (logLines.length > 300) logLines = logLines.slice(-300);
    const atBottom = els.log.scrollHeight - els.log.scrollTop - els.log.clientHeight < 30;
    els.log.textContent = logLines.join("\n");
    if (atBottom) els.log.scrollTop = els.log.scrollHeight;
  }

  function onEvent(event) {
    switch (event.type) {
      case "state":
        setState(event.state);
        if (event.state === "recording") {
          resetTranscript();
          if (event.mode) els.mode.value = event.mode;
        }
        break;
      case "ready":
        if (state === "recording") note("麦克风已就绪，开始说话吧", "");
        break;
      case "transcript":
        if (event.transcript) showTranscript(event.transcript);
        break;
      case "level":
        setLevel(event.rms || 0);
        break;
      case "finalized": {
        const injection = event.injection;
        if (injection == null) note("已完成（未上屏）", "");
        else if (injection.ok) note(`已上屏 · ${injection.method}`, "ok");
        else note(`上屏失败（${injection.method}）：${injection.message || "未知原因"}`, "err");
        break;
      }
      case "warning":
        note(event.message === "no speech text was recognized" ? "没有识别到语音" : event.message, "warn");
        break;
      case "error":
        note(event.message, "err");
        toast(`识别出错：${event.message}`, "err");
        break;
      case "cancelled":
        note("已取消，没有上屏任何文字", "warn");
        break;
      case "session":
        showSession(event);
        break;
      case "log":
        appendLog(event.line);
        break;
      default:
        break;
    }
  }

  async function refresh() {
    try {
      const status = await fetchStatus();
      setState(status.state);
      renderEngine(status);
      showSession(status.last_session);
      if (status.mode && els.mode.value !== status.mode && status.state !== "idle") els.mode.value = status.mode;
    } catch (error) {
      if (isDaemonDown(error)) {
        setState("down");
        els.engine.replaceChildren(emptyState("后台进程未运行", "启动后这里会显示模型与显存。"));
      }
    }
  }

  async function primaryAction() {
    const mode = els.mode.value;
    storage.set("liveMode", mode);
    const path = state === "recording" ? "/api/session/stop" : "/api/session/start";
    await busy(els.primary, async () => {
      try {
        await api(path, { method: "POST", body: state === "recording" ? undefined : { mode } });
      } catch { /* toast already shown */ }
    });
    setState(state); // restore the label busy() cleared
  }

  return {
    async mount(view) {
      els.badge = h("span", { class: "state-badge" }, h("span", { class: "dot" }), h("span", { class: "text", text: "连接中…" }));
      els.banner = h(
        "div",
        { class: "banner", hidden: true, role: "alert" },
        h("span", { class: "banner-icon", text: "◌" }),
        h("div", {}, h("b", { text: "后台进程未运行" }), h("p", {}, "实时识别、开始/停止和引擎信息需要 ", h("code", { text: "voiceless daemon" }), "。运行 ", h("code", { text: "systemctl --user start voiceless" }), " 后这里会自动恢复。")),
      );
      els.mode = h("select", { "aria-label": "模式" });
      els.primary = h("button", { type: "button", class: "btn primary big", onclick: primaryAction }, "开始录音");
      els.cancel = h("button", {
        type: "button",
        class: "btn danger",
        onclick: () => busy(els.cancel, () => api("/api/session/cancel", { method: "POST" }).catch(() => {})).then(() => setState(state)),
      }, "取消");
      els.transcript = h("div", { class: "transcript", role: "log", "aria-live": "polite", "aria-label": "识别文字" }, h("span", { class: "placeholder", text: "按下快捷键或点击“开始录音”，说的话会实时出现在这里。" }));
      els.note = h("div", { class: "transcript-note" });
      els.meterFill = h("div", { class: "meter-fill" });
      els.statFinal = h("div", { class: "v", text: "–" });
      els.statAudio = h("div", { class: "v", text: "–" });
      els.statChars = h("div", { class: "v", text: "–" });
      els.engine = h("div", {}, h("div", { class: "hint", text: "读取中…" }));
      els.log = h("pre", { class: "log", tabindex: "0", "aria-label": "实时日志" });
      const reload = h("button", { type: "button", class: "btn small", onclick: (event) => reloadEngine(event.currentTarget).then(refresh) }, "重新加载引擎");

      view.append(
        pageHead("实时", "当前会话、识别文字和引擎状态"),
        els.banner,
        h(
          "section",
          { class: "card" },
          h("div", { class: "session-bar" }, els.badge, h("span", { class: "grow" }), h("label", { class: "hint", style: { margin: 0 }, for: "liveMode", text: "模式" }), els.mode, els.cancel, els.primary),
          els.transcript,
          els.note,
          h("div", { class: "meter" }, h("span", { class: "label", text: "麦克风" }), h("div", { class: "meter-track", role: "presentation" }, els.meterFill)),
        ),
        h(
          "div",
          { class: "card-row" },
          card("上次会话", null, h("div", { class: "stats" },
            h("div", { class: "stat" }, h("div", { class: "k", text: "松手到出字" }), els.statFinal),
            h("div", { class: "stat" }, h("div", { class: "k", text: "语音时长" }), els.statAudio),
            h("div", { class: "stat" }, h("div", { class: "k", text: "字数" }), els.statChars),
          )),
          h("section", { class: "card" }, h("h2", {}, "引擎", h("span", { class: "section", text: "" }), h("span", { style: { marginLeft: "auto" } }, reload)), els.engine),
        ),
        h("section", { class: "card" }, h("details", { class: "logs" }, h("summary", { text: "实时日志" }), els.log)),
      );
      els.mode.id = "liveMode";

      modes = await loadModes();
      const remembered = storage.get("liveMode", null);
      els.mode.replaceChildren(...modes.map((mode) => h("option", { value: mode.id }, mode.name || mode.id)));
      if (remembered && modes.some((mode) => mode.id === remembered)) els.mode.value = remembered;

      unsubscribe = live.subscribe(onEvent);
      live.connect();
      frame = requestAnimationFrame(animate);
      await refresh();
      statusTimer = setInterval(refresh, 5000);
      try {
        const logs = await api("/api/logs", { quiet: true });
        logLines = (logs.lines || []).slice(-300);
        els.log.textContent = logLines.join("\n");
      } catch { /* daemon down: the banner explains */ }
    },
    unmount() {
      unsubscribe?.();
      clearInterval(statusTimer);
      cancelAnimationFrame(frame);
    },
  };
}

function gpuBar(gpu) {
  const used = gpu.memory_used_mib ?? 0;
  const total = gpu.memory_total_mib || 1;
  const ratio = Math.min(1, used / total);
  return h(
    "div",
    { class: "gpu" },
    h("div", { class: "gpu-line" }, h("span", { text: gpu.name || "GPU" }), h("span", { text: `${(used / 1024).toFixed(1)} / ${(total / 1024).toFixed(1)} GB` })),
    h("div", { class: "bar", role: "meter", "aria-valuemin": "0", "aria-valuemax": String(total), "aria-valuenow": String(used), "aria-label": "显存占用" }, h("span", { style: { width: `${(ratio * 100).toFixed(1)}%` } })),
  );
}

/* ====================================================================== */
/* page: 识别引擎                                                          */
/* ====================================================================== */

const MODEL_KIND = {
  gguf: ["GPU · GGUF", "ok"],
  sherpa: ["CPU · Sherpa", ""],
  vad: ["语音检测", ""],
  streaming: ["流式 · CPU", ""],
};

const DOUBAO_RESOURCES = [
  ["volc.seedasr.sauc.duration", "Seed ASR 2.0 · 按时长"],
  ["volc.seedasr.sauc.concurrent", "Seed ASR 2.0 · 按并发"],
  ["volc.bigasr.sauc.duration", "ASR 1.0 · 按时长"],
  ["volc.bigasr.sauc.concurrent", "ASR 1.0 · 按并发"],
];

function EnginePage() {
  const SECTIONS = ["asr", "local", "doubao", "cloud"];
  let saved = null;
  let draft = null;
  let models = [];
  let bar = null;
  let pollTimer = null;
  let view = null;
  let modelTable = null;

  const dirty = () => saved && !same(saved, draft);
  const changed = () => bar?.set(dirty());

  function localModelOptions() {
    const offline = models.filter((model) => (model.kind === "gguf" || model.kind === "sherpa"));
    const options = offline
      .filter((model) => model.installed)
      .map((model) => [model.id, `${model.id}${model.kind === "gguf" ? "（GPU）" : "（CPU）"}`]);
    const current = draft.local?.model;
    if (current && !options.some(([id]) => id === current)) options.unshift([current, `${current}（未安装）`]);
    if (!options.length) options.push(["", "没有已安装的离线模型"]);
    return options;
  }

  function build() {
    const a = draft.asr;
    const l = draft.local;
    const d = draft.doubao;
    const c = draft.cloud;

    const fallbackOptions = [[null, "不使用"], ...BACKENDS];
    const resourceList = h("datalist", { id: "doubaoResources" }, DOUBAO_RESOURCES.map(([value, label]) => h("option", { value, label })));
    const resourceInput = textInput(d, "resource_id", changed, { mono: true });
    resourceInput.setAttribute("list", "doubaoResources");

    modelTable = h("div", { class: "table-wrap" });

    view.replaceChildren(
      pageHead("识别引擎", "选择识别方式、本地模型和云端账号"),
      card(
        "识别方式",
        "[asr]",
        h("p", { class: "hint", text: "主引擎打不开（缺模型、没有密钥、断网）时，会自动改用备用引擎。" }),
        h(
          "div",
          { class: "fields" },
          field("主引擎", selectInput(a, "streaming_backend", BACKENDS, changed)),
          field("备用引擎", selectInput(a, "fallback_backend", fallbackOptions, changed)),
        ),
      ),
      card(
        "本地模型",
        "[local]",
        h("p", { class: "hint" }, "GGUF 模型在 GPU 上运行（需要 CUDA 版软件包）；Sherpa 模型只用 CPU。词库页的热词会自动合并到这里。"),
        h(
          "div",
          { class: "fields" },
          field("模型", selectInput(l, "model", localModelOptions(), changed)),
          field("设备", selectInput(l, "provider", [[null, "按模型默认"], ["cuda", "GPU（cuda）"], ["cpu", "CPU"]], changed)),
          field("实时刷新间隔（毫秒）", numberInput(l, "partial_interval_ms", changed, { min: 100, max: 5000, step: 50 }), { help: "说话时多久重新识别一次当前句" }),
          field("单句上限（秒）", numberInput(l, "max_segment_seconds", changed, { min: 1, max: 60, step: 1, nullable: true, placeholder: "按模型默认" }), { help: "GGUF 最多 60，Sherpa 最多 25" }),
          field("CPU 线程数", numberInput(l, "num_threads", changed, { min: 1, max: 32 }), { help: "仅 Sherpa 模型使用" }),
          field("本地专用热词", tagInput(l.hotwords, (values) => { l.hotwords = values; changed(); }).el, { wide: true }),
        ),
      ),
      card(
        "豆包（火山引擎）",
        "[doubao]",
        h("p", { class: "hint" }, "新版控制台只需 APP Key；旧版控制台填 App ID 和 Access Token。资源 ID 要和开通的服务一致，否则连接会被拒绝（403）。"),
        resourceList,
        h(
          "div",
          { class: "fields" },
          field("APP Key（新版）", secretInput(d, "api_key", secretFlag(saved.doubao, "api_key"), changed)),
          field("App ID（旧版）", textInput(d, "app_key", changed, { mono: true })),
          field("Access Token（旧版）", secretInput(d, "access_key", secretFlag(saved.doubao, "access_key"), changed)),
          field("资源 ID", resourceInput, { help: "可从下拉建议中选择" }),
          field("接口地址", textInput(d, "endpoint", changed, { mono: true }), { wide: true }),
          field("断句静音（毫秒）", numberInput(d, "end_window_size", changed, { min: 200, max: 5000, step: 50, nullable: true, placeholder: "服务默认 800" })),
        ),
        h(
          "div",
          { class: "switches", style: { marginTop: "14px" } },
          toggleInput(d, "enable_punc", changed, "自动标点"),
          toggleInput(d, "enable_itn", changed, "数字规整", "“二十三”写成 23"),
          toggleInput(d, "enable_ddc", changed, "语义顺滑", "去掉口头语和重复"),
          toggleInput(d, "enable_nonstream", changed, "二遍识别", "确认句更准，略慢"),
        ),
        h("div", { class: "field", style: { marginTop: "14px" } }, h("span", { class: "label", text: "豆包专用热词" }), tagInput(d.hotwords, (values) => { d.hotwords = values; changed(); }).el),
      ),
      card(
        "百炼（阿里云）",
        "[cloud]",
        h("p", { class: "hint", text: "实时识别与批量转写使用；免费额度用完后需要在阿里云开通按量付费。" }),
        h(
          "div",
          { class: "fields" },
          field("API Key", secretInput(c, "api_key", secretFlag(saved.cloud, "api_key"), changed)),
          field("实时模型", textInput(c, "realtime_model", changed, { mono: true })),
          field("实时接口", textInput(c, "realtime_endpoint", changed, { mono: true }), { wide: true }),
          field("批量模型", textInput(c, "model", changed, { mono: true })),
          field("批量接口", textInput(c, "base_url", changed, { mono: true })),
          field("超时（秒）", numberInput(c, "timeout_seconds", changed, { min: 1, max: 600, step: 1 })),
        ),
      ),
      card("模型管理", null, h("p", { class: "hint", text: "下载会校验文件大小和 SHA-256；正在使用的模型不能删除。" }), modelTable),
    );
    renderModels();
  }

  function renderModels() {
    if (!modelTable) return;
    if (!models.length) {
      modelTable.replaceChildren(emptyState("没有可用的模型目录", null));
      return;
    }
    const rows = models.map((model) => {
      const [kindLabel, kindClass] = MODEL_KIND[model.kind] || [model.kind || "—", ""];
      let statusCell;
      if (model.installing) {
        const progress = model.progress;
        statusCell = h(
          "div",
          { style: { display: "flex", alignItems: "center", gap: "10px" } },
          h("div", { class: `progress${progress == null ? " indeterminate" : ""}`, role: "progressbar", "aria-valuemin": "0", "aria-valuemax": "100", "aria-valuenow": progress == null ? null : String(Math.round(progress * 100)) }, h("span", { style: progress == null ? {} : { width: `${(progress * 100).toFixed(1)}%` } })),
          h("span", { class: "mono", style: { fontSize: "12px" }, text: progress == null ? "校验中" : `${Math.round(progress * 100)}%` }),
        );
      } else if (model.error) {
        statusCell = h("span", { class: "chip err", title: model.error, text: "安装失败" });
      } else if (model.installed) {
        statusCell = h("span", { class: model.in_use ? "chip warn" : "chip ok", text: model.in_use ? "使用中" : "已安装" });
      } else {
        statusCell = h("span", { class: "chip dashed", text: "未安装" });
      }
      const actions = h("div", { class: "actions", style: { justifyContent: "flex-end" } });
      if (!model.installed && !model.installing) {
        const install = h("button", { type: "button", class: "btn small", onclick: () => installModel(model, install) }, model.error ? "重试" : "下载");
        actions.append(install);
      } else if (model.installed) {
        const remove = h("button", {
          type: "button",
          class: "btn small danger",
          disabled: model.in_use,
          title: model.in_use ? "正在使用的模型不能删除" : null,
          onclick: () => deleteModel(model, remove),
        }, "删除");
        actions.append(remove);
      }
      return h(
        "tr",
        {},
        h("td", {}, h("div", { class: "mono", style: { fontSize: "13.5px" }, text: model.id }), h("div", { class: "help", style: { fontSize: "12px", color: "var(--faint)" }, title: model.license_status || "", text: model.version || "" })),
        h("td", {}, h("span", { class: `chip ${kindClass}`, text: kindLabel })),
        h("td", { class: "num", text: fmtBytes(model.size_bytes) }),
        h("td", {}, statusCell),
        h("td", {}, actions),
      );
    });
    modelTable.replaceChildren(
      h("table", {}, h("thead", {}, h("tr", {}, h("th", { text: "模型" }), h("th", { text: "类型" }), h("th", { class: "num", text: "大小" }), h("th", { text: "状态" }), h("th", { "aria-label": "操作" }))), h("tbody", {}, rows)),
    );
    schedulePoll();
  }

  function schedulePoll() {
    clearTimeout(pollTimer);
    if (models.some((model) => model.installing)) {
      pollTimer = setTimeout(async () => {
        try {
          const before = new Set(models.filter((m) => m.installing).map((m) => m.id));
          models = await api("/api/models", { quiet: true });
          for (const model of models) {
            if (before.has(model.id) && model.installed) toast(`${model.id} 安装完成`, "ok");
            if (before.has(model.id) && model.error) toast(`${model.id} 安装失败：${model.error}`, "err");
          }
          renderModels();
        } catch {
          schedulePoll();
        }
      }, 1500);
    }
  }

  async function installModel(model, button) {
    await busy(button, async () => {
      try {
        await api(`/api/models/${encodeURIComponent(model.id)}/install`, { method: "POST" });
        toast(`开始下载 ${model.id}（${fmtBytes(model.size_bytes)}）`, "info");
        models = await api("/api/models", { quiet: true });
      } catch { /* toast shown */ }
    });
    renderModels();
  }

  async function deleteModel(model, button) {
    if (!(await confirmDialog(`删除模型 ${model.id}？之后要用需要重新下载 ${fmtBytes(model.size_bytes)}。`, { okText: "删除", danger: true }))) return;
    await busy(button, async () => {
      try {
        await api(`/api/models/${encodeURIComponent(model.id)}`, { method: "DELETE" });
        toast(`已删除 ${model.id}`, "ok");
        models = await api("/api/models", { quiet: true });
      } catch { /* toast shown */ }
    });
    renderModels();
  }

  async function save() {
    try {
      const result = await saveSections(saved, draft, SECTIONS);
      await reloadConfig();
      toast("已保存", "ok");
      if (result.restart_engine) await offerEngineReload();
    } catch { /* toast shown; draft kept */ }
  }

  async function reloadConfig() {
    const config = await api("/api/config");
    saved = Object.fromEntries(SECTIONS.map((name) => [name, clone(config[name] || {})]));
    for (const name of ["doubao", "cloud"]) {
      // Secrets are write-only: the draft starts "unchanged".
      saved[name].api_key = "";
      if (name === "doubao") saved[name].access_key = "";
    }
    draft = clone(saved);
    // has_* markers survive in both; they are stripped on save.
    build();
    changed();
  }

  return {
    async mount(target) {
      view = target;
      view.append(pageHead("识别引擎", "读取配置中…"));
      bar = saveBar({ onSave: save, onDiscard: () => { draft = clone(saved); build(); changed(); } });
      try {
        models = await api("/api/models", { quiet: true });
      } catch {
        models = [];
      }
      try {
        await reloadConfig();
      } catch (error) {
        view.replaceChildren(pageHead("识别引擎"), emptyState("读取配置失败", humanError(error)));
      }
    },
    unmount() {
      clearTimeout(pollTimer);
      bar?.remove();
    },
    isDirty: dirty,
  };
}

/* ====================================================================== */
/* page: 词库                                                              */
/* ====================================================================== */

function TextPage() {
  let saved = null;
  let draft = null;
  let bar = null;
  let view = null;
  let rulesTable = null;
  let sample = storage.get("previewSample", "呃，我去找张三峰聊聊，然后现在，呃，我会给你发一份 receive test。");
  const output = h("div", { class: "preview-out", "aria-live": "polite" });
  const outputMeta = h("span", {});

  const dirty = () => saved && !same(saved, draft);
  const changed = () => {
    bar?.set(dirty());
    schedulePreview();
  };

  const schedulePreview = debounce(runPreview, 300);

  async function runPreview() {
    if (!draft) return;
    const started = performance.now();
    try {
      const result = await api("/api/text/preview", { method: "POST", body: { text: sample, config: draft.text }, quiet: true });
      output.className = "preview-out";
      output.replaceChildren(sample.trim() ? renderDiff(sample, result.text ?? "") : h("span", { class: "placeholder", style: { color: "var(--faint)" }, text: "在上面输入一句话" }));
      outputMeta.textContent = `${Math.round(performance.now() - started)} ms`;
      rulesTable?.clearInvalid();
    } catch (error) {
      output.className = "preview-out err";
      output.textContent = humanError(error);
      outputMeta.textContent = "";
      // Point at the offending regex row when the server names its pattern.
      const index = (draft.text.rules || []).findIndex((rule) => rule.pattern && error.message.includes(JSON.stringify(rule.pattern)));
      if (index >= 0) rulesTable?.markInvalid(index, error.message);
    }
  }

  function build() {
    const t = draft.text;
    rulesTable = tableEditor({
      columns: [
        { key: "pattern", label: "正则表达式", placeholder: "(\\d+)点(\\d+)分", mono: true },
        { key: "replace", label: "替换为", placeholder: "$1:$2", mono: true },
      ],
      rows: t.rules,
      emptyText: "还没有正则规则。可以用 $1、$2 引用捕获组。",
      addLabel: "添加规则",
      onChange: (rows) => { t.rules = rows; changed(); },
    });
    const replacements = tableEditor({
      columns: [
        { key: "from", label: "识别结果中的", placeholder: "receive test" },
        { key: "to", label: "替换为", placeholder: "resume text" },
      ],
      rows: t.replacements,
      emptyText: "还没有替换规则。历史页的“纠错”可以一键添加。",
      addLabel: "添加替换",
      onChange: (rows) => { t.replacements = rows; changed(); },
    });
    const sampleInput = h("textarea", {
      rows: "4",
      value: sample,
      "aria-label": "试一试的输入",
      oninput: (event) => {
        sample = event.target.value;
        storage.set("previewSample", sample);
        schedulePreview();
      },
    });

    view.replaceChildren(
      pageHead("词库", "热词、替换规则和语气词清理，对每一次识别结果生效"),
      h(
        "div",
        { class: "split" },
        h(
          "div",
          { class: "stack" },
          card(
            "热词",
            "[text]",
            h("p", { class: "hint" }, "同一份热词既会让识别模型更倾向这些词，也会把读音相近的中文片段纠正过来（例如“张三峰”→“张三丰”）。"),
            tagInput(t.hotwords, (values) => { t.hotwords = values; changed(); }, "输入热词后按回车，可一次粘贴多个").el,
            h("div", { class: "field", style: { marginTop: "14px" } },
              h("span", { class: "label", text: "读音相似度阈值" }),
              rangeInput(t, "hotword_threshold", changed, { min: 0.5, max: 1, step: 0.01, format: (v) => Number(v).toFixed(2) }),
              h("div", { class: "help", text: "1.00 = 拼音完全相同才纠正；zh/z、n/l、前后鼻音等混淆每个字记 0.8。越低越激进。" }),
            ),
          ),
          card(
            "清理",
            null,
            h("div", { class: "switches" },
              toggleInput(t, "remove_fillers", changed, "去掉语气词", "删除所有“呃”和单独的“嗯”"),
              toggleInput(t, "strip_trailing_period", changed, "去掉句末句号", "适合聊天框"),
            ),
          ),
          card("精确替换", null, h("p", { class: "hint", text: "按顺序替换识别结果中出现的文字。" }), replacements.el),
          card("正则规则", null, h("p", { class: "hint", text: "在精确替换之后按顺序执行，使用 Rust 正则语法。写错的规则会在右侧预览时标红。" }), rulesTable.el),
        ),
        h(
          "div",
          { class: "sticky" },
          card(
            "试一试",
            null,
            h("p", { class: "hint", text: "用当前（未保存的）设置清理这句话，红色是删掉的，绿色是新加的。" }),
            sampleInput,
            h("div", { class: "preview-label" }, h("span", { text: "清理结果" }), outputMeta),
            output,
          ),
        ),
      ),
    );
    runPreview();
  }

  async function save() {
    try {
      const result = await saveSections(saved, draft, ["text"]);
      await reloadConfig();
      toast("词库已保存，下一次识别生效", "ok");
      if (result.restart_engine) await offerEngineReload();
    } catch { /* toast shown */ }
  }

  async function reloadConfig() {
    const config = await api("/api/config");
    const text = config.text || {};
    saved = {
      text: {
        ...text,
        hotwords: text.hotwords || [],
        replacements: text.replacements || [],
        rules: text.rules || [],
      },
    };
    draft = clone(saved);
    build();
    changed();
  }

  return {
    async mount(target) {
      view = target;
      view.append(pageHead("词库", "读取配置中…"));
      bar = saveBar({ onSave: save, onDiscard: () => { draft = clone(saved); build(); changed(); } });
      try {
        await reloadConfig();
      } catch (error) {
        view.replaceChildren(pageHead("词库"), emptyState("读取配置失败", humanError(error)));
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
/* page: 模式与提示词                                                       */
/* ====================================================================== */

function ModesPage() {
  let view = null;
  let modes = [];
  let bar = null;
  let saved = null; // { processing, prompt }
  let draft = null;
  const list = h("div", { class: "mode-list" });

  const dirty = () => saved && !same(saved, draft);
  const changed = () => bar?.set(dirty());

  function renderModes() {
    if (!modes.length) {
      list.replaceChildren(emptyState("没有模式", null));
      return;
    }
    list.replaceChildren(
      ...modes.map((mode) => {
        const item = h("div", { class: "mode-item" });
        const actions = h("div", { class: "actions" });
        if (mode.builtin) {
          actions.append(h("button", { type: "button", class: "btn small ghost", onclick: () => item.classList.toggle("expanded") }, "展开"));
        } else {
          actions.append(
            h("button", { type: "button", class: "btn small", onclick: () => editMode(mode) }, "编辑"),
            h("button", { type: "button", class: "btn small danger", onclick: (event) => deleteMode(mode, event.currentTarget) }, "删除"),
          );
        }
        item.append(
          h(
            "div",
            { class: "mode-head" },
            h("span", { class: "mode-name", text: mode.name || mode.id }),
            h("span", { class: "mode-id", text: mode.id }),
            mode.builtin ? h("span", { class: "chip", text: "内置 · 只读" }) : h("span", { class: "chip ok", text: "自定义" }),
            mode.processing_label ? h("span", { class: "chip dashed", text: `处理中：${mode.processing_label}` }) : null,
            actions,
          ),
          mode.prompt ? h("div", { class: "mode-prompt", text: mode.prompt }) : h("div", { class: "mode-prompt", text: mode.id === "quick" ? "直接输出识别结果，不调用大模型。" : "（使用下方的整理提示词）" }),
        );
        return item;
      }),
    );
  }

  async function refreshModes() {
    try {
      modes = [...(await api("/api/modes"))].sort((a, b) => (a.sort_order ?? 0) - (b.sort_order ?? 0));
    } catch {
      modes = [];
    }
    renderModes();
  }

  async function editMode(mode) {
    const form = { name: mode?.name || "", processing_label: mode?.processing_label || "", prompt: mode?.prompt || "" };
    const name = h("input", { value: form.name, placeholder: "例如：翻译成英文", oninput: (e) => { form.name = e.target.value; } });
    const label = h("input", { value: form.processing_label, placeholder: "例如：翻译中", oninput: (e) => { form.processing_label = e.target.value; } });
    const prompt = h("textarea", { class: "prompt", value: form.prompt, placeholder: "把下面的内容翻译成自然的英文，只输出译文：{text}", oninput: (e) => { form.prompt = e.target.value; } });
    const body = h(
      "div",
      { class: "stack" },
      field("名称", name),
      field("处理中提示", label, { help: "浮层在等待大模型时显示的文字" }),
      field("提示词", prompt, { help: "{text} 会被替换为识别结果" }),
    );
    const result = await openDialog({
      title: mode ? `编辑模式：${mode.name}` : "新建模式",
      body,
      actions: [
        { label: "取消", value: "" },
        { label: mode ? "保存" : "创建", value: "save", class: "primary" },
      ],
      onOpen: () => name.focus(),
    });
    if (result !== "save") return;
    if (!form.name.trim()) {
      toast("名称不能为空", "warn");
      return;
    }
    if (!form.prompt.includes("{text}")) {
      toast("提示词里没有 {text}，识别结果不会被带进去", "warn", 6000);
    }
    try {
      if (mode) await api(`/api/modes/${encodeURIComponent(mode.id)}`, { method: "PUT", body: form });
      else await api("/api/modes", { method: "POST", body: form });
      toast(mode ? "模式已更新" : "模式已创建", "ok");
      await refreshModes();
    } catch { /* toast shown */ }
  }

  async function deleteMode(mode, button) {
    if (!(await confirmDialog(`删除模式“${mode.name}”？`, { okText: "删除", danger: true }))) return;
    await busy(button, async () => {
      try {
        await api(`/api/modes/${encodeURIComponent(mode.id)}`, { method: "DELETE" });
        toast("已删除", "ok");
      } catch { /* toast shown */ }
    });
    await refreshModes();
  }

  function buildSettings() {
    const p = draft.processing;
    return [
      card(
        "整理提示词",
        "prompt-optimize",
        h("p", { class: "hint" }, "“提示词优化”模式使用的模板，", h("code", { text: "{text}" }), " 会被替换为识别结果。保存后下次语音输入生效。"),
        h("textarea", {
          class: "prompt",
          rows: "7",
          value: draft.prompt,
          spellcheck: "false",
          "aria-label": "整理提示词",
          oninput: (event) => { draft.prompt = event.target.value; changed(); },
        }),
      ),
      card(
        "整理用的大模型",
        "[processing]",
        h("p", { class: "hint", text: "非“快速”模式会把识别结果发给这个模型。支持 OpenAI 兼容接口和本地 Ollama。" }),
        h(
          "div",
          { class: "fields" },
          field("服务类型", selectInput(p, "provider", [["none", "不使用"], ["openai-compatible", "OpenAI 兼容接口"], ["ollama", "Ollama（本地）"]], changed)),
          field("模型", textInput(p, "model", changed, { mono: true })),
          field("接口地址", textInput(p, "base_url", changed, { mono: true }), { wide: true }),
          field("API Key", secretInput(p, "api_key", secretFlag(saved.processing, "api_key"), changed)),
          field("或从环境变量读取", textInput(p, "api_key_env", changed, { mono: true, placeholder: "例如 DASHSCOPE_API_KEY" })),
          field("超时（秒）", numberInput(p, "timeout_seconds", changed, { min: 1, max: 600 })),
        ),
      ),
    ];
  }

  function build() {
    const add = h("button", { type: "button", class: "btn primary", onclick: () => editMode(null) }, "＋ 新建模式");
    view.replaceChildren(
      pageHead("模式与提示词", "说完之后，识别结果按所选模式处理再上屏", add),
      card("模式", null, h("p", { class: "hint", text: "内置模式不能修改；自定义模式可以在实时页、快捷键页选用。" }), list),
      ...buildSettings(),
    );
    renderModes();
  }

  async function save() {
    try {
      let restart = false;
      if (saved.prompt !== draft.prompt) {
        await api("/api/prompt", { method: "POST", body: { prompt: draft.prompt } });
      }
      if (!same(saved.processing, draft.processing)) {
        const result = await saveSections({ processing: saved.processing }, { processing: draft.processing }, ["processing"]);
        restart = result.restart_engine;
      }
      await reload();
      toast("已保存", "ok");
      if (restart) await offerEngineReload();
    } catch { /* toast shown */ }
  }

  async function reload() {
    const [config, prompt] = await Promise.all([api("/api/config"), api("/api/prompt")]);
    const processing = clone(config.processing || {});
    delete processing.prompt; // edited through /api/prompt
    processing.api_key = "";
    saved = { processing, prompt: prompt.prompt || "" };
    draft = clone(saved);
    build();
    changed();
  }

  return {
    async mount(target) {
      view = target;
      view.append(pageHead("模式与提示词", "读取中…"));
      bar = saveBar({ onSave: save, onDiscard: () => { draft = clone(saved); build(); changed(); } });
      await refreshModes();
      try {
        await reload();
      } catch (error) {
        view.replaceChildren(pageHead("模式与提示词"), card("模式", null, list), emptyState("读取配置失败", humanError(error)));
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

function HotkeyPage() {
  const SECTIONS = ["hotkey", "inject", "audio"];
  let view = null;
  let saved = null;
  let draft = null;
  let bar = null;
  let keymap = [];
  let modes = [];
  let status = null;
  let capturing = null; // keydown handler while recording a key

  const dirty = () => saved && !same(saved, draft);
  const changed = () => bar?.set(dirty());

  function keyLabel(evdev) {
    if (!evdev) return "未设置";
    const entry = keymap.find((item) => item.evdev === evdev);
    return entry ? entry.code : evdev.replace(/^KEY_/, "");
  }

  function stopCapture() {
    if (capturing) window.removeEventListener("keydown", capturing, true);
    capturing = null;
  }

  function keyCard() {
    const k = draft.hotkey;
    const display = h("kbd", { text: keyLabel(k.key) });
    const record = h("button", { type: "button", class: "btn" }, "录制按键");
    const clear = h("button", {
      type: "button",
      class: "btn ghost",
      disabled: !k.key,
      onclick: () => {
        k.key = null;
        display.textContent = keyLabel(null);
        clear.disabled = true;
        changed();
      },
    }, "清除");
    const tip = h("div", { class: "help", text: "点“录制按键”后按下想用的键（Esc 取消）。建议选平时用不到的键，如 ScrollLock、Pause。" });
    record.addEventListener("click", () => {
      if (capturing) {
        stopCapture();
        display.classList.remove("listening");
        display.textContent = keyLabel(k.key);
        record.textContent = "录制按键";
        return;
      }
      display.classList.add("listening");
      display.textContent = "请按键…";
      record.textContent = "取消录制";
      capturing = (event) => {
        event.preventDefault();
        event.stopPropagation();
        if (["ControlLeft", "ControlRight"].includes(event.code)) return; // reserved as the mode modifier
        stopCapture();
        display.classList.remove("listening");
        record.textContent = "录制按键";
        if (event.key === "Escape") {
          display.textContent = keyLabel(k.key);
          return;
        }
        const entry = keymap.find((item) => item.code === event.code);
        if (!entry) {
          display.textContent = keyLabel(k.key);
          toast(`“${event.code || event.key}”无法映射到 Linux 按键，请换一个键`, "warn");
          return;
        }
        k.key = entry.evdev;
        display.textContent = keyLabel(k.key);
        clear.disabled = false;
        changed();
      };
      window.addEventListener("keydown", capturing, true);
    });

    const modeOptions = [["", "后台默认模式"], ...modes.map((mode) => [mode.id, mode.name || mode.id])];
    const ctrlOptions = modes.map((mode) => [mode.id, mode.name || mode.id]);
    if (k.ctrl_mode && !ctrlOptions.some(([id]) => id === k.ctrl_mode)) ctrlOptions.push([k.ctrl_mode, k.ctrl_mode]);

    return card(
      "按住说话键",
      "[hotkey]",
      h("p", { class: "hint", text: "按住超过阈值：松开即结束识别；短按一下：开始录音，再按一下结束。按住 Ctrl 再按，使用 Ctrl 模式。" }),
      listenerStatus(),
      h("div", { class: "inline", style: { margin: "6px 0 4px" } }, display, record, clear),
      tip,
      h(
        "div",
        { class: "fields", style: { marginTop: "16px" } },
        field("按住阈值", rangeInput(k, "hold_threshold_ms", changed, { min: 100, max: 1000, step: 10, format: (v) => `${v} ms` }), { wide: true }),
        field("直接按下时的模式", selectInput(k, "mode", modeOptions, changed)),
        field("按住 Ctrl 时的模式", selectInput(k, "ctrl_mode", ctrlOptions, changed)),
      ),
    );
  }

  function listenerStatus() {
    if (!status) {
      return h("div", { class: "banner warn" }, h("span", { class: "banner-icon", text: "◌" }), h("div", {}, h("b", { text: "后台进程未运行" }), h("p", { text: "设置可以先保存，启动 voiceless daemon 后生效。" })));
    }
    const hotkey = status.hotkey || {};
    if (!hotkey.key) return h("p", { class: "hint", text: "当前未启用按键监听。" });
    if (hotkey.error || !hotkey.keyboards) {
      return h(
        "div",
        { class: "banner" },
        h("span", { class: "banner-icon", text: "⚠" }),
        h("div", {}, h("b", { text: `读不到键盘（${keyLabel(hotkey.key)}）` }), h("p", {}, hotkey.error || "", hotkey.error ? " " : "", "需要把当前用户加入 ", h("code", { text: "input" }), " 组并重新登录一次。")),
      );
    }
    return h("p", { class: "hint" }, h("span", { class: "chip ok", text: "监听中" }), ` ${keyLabel(hotkey.key)} · ${hotkey.keyboards} 个键盘`);
  }

  function injectCard() {
    const i = draft.inject;
    const methods = [["fcitx5", "fcitx5 输入法（推荐）"], ["clipboard", "剪贴板粘贴"], ["wtype", "模拟键盘（wtype）"]];
    const segmented = h(
      "div",
      { class: "segmented", role: "group", "aria-label": "上屏方式" },
      methods.map(([value, label]) =>
        h("button", {
          type: "button",
          "aria-pressed": String(i.prefer === value),
          onclick: (event) => {
            i.prefer = value;
            $$("button", segmented).forEach((b) => b.setAttribute("aria-pressed", "false"));
            event.currentTarget.setAttribute("aria-pressed", "true");
            preedit.hidden = value !== "fcitx5";
            changed();
          },
        }, label),
      ),
    );
    const preedit = h("div", { hidden: i.prefer !== "fcitx5" }, toggleInput(i, "preedit", changed, "输入框内实时显示", "识别中的文字以下划线显示在光标处，说完正式上屏"));
    return card(
      "上屏方式",
      "[inject]",
      h("p", { class: "hint", text: "fcitx5 方式和用输入法打字走同一条路，不占用剪贴板；失败时可回退剪贴板。" }),
      segmented,
      h("div", { class: "switches", style: { marginTop: "16px" } }, preedit, toggleInput(i, "clipboard_fallback", changed, "失败时回退剪贴板")),
      h(
        "details",
        { style: { marginTop: "16px" } },
        h("summary", { class: "hint", style: { cursor: "pointer" }, text: "高级：剪贴板与 wtype 命令" }),
        h(
          "div",
          { class: "fields", style: { marginTop: "12px" } },
          field("粘贴工具", textInput(i, "paste_tool", changed, { mono: true })),
          field("粘贴参数", textInput(i, "paste_command", changed, { mono: true })),
          field("wl-copy 命令", textInput(i, "wl_copy_command", changed, { mono: true })),
          field("wtype 命令", textInput(i, "wtype_command", changed, { mono: true })),
          field("X11 剪贴板命令", textInput(i, "x11_clipboard_command", changed, { mono: true, placeholder: "例如 xsel --clipboard --input" })),
          field("粘贴前聚焦命令", textInput(i, "focus_command", changed, { mono: true })),
          field("超时（秒）", numberInput(i, "timeout_seconds", changed, { min: 0.5, max: 60, step: 0.5 })),
        ),
      ),
    );
  }

  function testCard() {
    const text = h("input", { value: storage.get("injectTest", "Voiceless 上屏测试 ✓"), "aria-label": "测试文字" });
    const result = h("div", { class: "transcript-note" });
    const run = h("button", { type: "button", class: "btn" }, "测试上屏");
    run.addEventListener("click", () =>
      busy(run, async () => {
        storage.set("injectTest", text.value);
        if (dirty()) toast("提示：测试使用的是已保存的设置", "warn");
        const overlay = h("div", { class: "countdown", role: "status" }, h("div", { class: "box" }, h("div", { class: "n", text: "3" }), h("div", { class: "t", text: "请切换到一个输入框…" })));
        document.body.append(overlay);
        const request = api("/api/inject/test", { method: "POST", body: { text: text.value, delay_ms: 3000 } });
        for (const n of [3, 2, 1]) {
          $(".n", overlay).textContent = String(n);
          await sleep(1000);
        }
        overlay.remove();
        try {
          const response = await request;
          const injection = response.injection || {};
          result.className = `transcript-note ${injection.ok ? "ok" : "err"}`;
          result.textContent = injection.ok ? `成功 · ${injection.method}` : `失败（${injection.method}）：${injection.message || "未知原因"}`;
        } catch (error) {
          result.className = "transcript-note err";
          result.textContent = humanError(error);
        }
      }),
    );
    return card("测试上屏", null, h("p", { class: "hint", text: "点击后有 3 秒切到任意输入框，然后用当前上屏方式输入这段文字。" }), h("div", { class: "inline" }, text, run), result);
  }

  function audioCard() {
    const a = draft.audio;
    return card(
      "声音",
      "[audio]",
      h("div", { class: "switches" },
        toggleInput(a, "pause_media", changed, "录音时暂停音乐", "结束后自动恢复播放"),
        toggleInput(a, "cues", changed, "开始/结束提示音"),
      ),
    );
  }

  function build() {
    view.replaceChildren(pageHead("快捷键与上屏", "怎样开始说话，结果怎样进入输入框"), keyCard(), injectCard(), testCard(), audioCard());
  }

  async function save() {
    try {
      const result = await saveSections(saved, draft, SECTIONS);
      await reload();
      toast("已保存", "ok");
      if (result.restart_engine) await offerEngineReload();
    } catch { /* toast shown */ }
  }

  async function reload() {
    const config = await api("/api/config");
    saved = Object.fromEntries(SECTIONS.map((name) => [name, clone(config[name] || {})]));
    if (saved.hotkey.key === undefined) saved.hotkey.key = null;
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
      view.append(pageHead("快捷键与上屏", "读取中…"));
      bar = saveBar({ onSave: save, onDiscard: () => { draft = clone(saved); build(); changed(); } });
      const [keys, modeList] = await Promise.all([api("/api/hotkey/keys", { quiet: true }).catch(() => []), loadModes()]);
      keymap = keys || [];
      modes = modeList;
      try {
        await reload();
      } catch (error) {
        view.replaceChildren(pageHead("快捷键与上屏"), emptyState("读取配置失败", humanError(error)));
      }
    },
    unmount() {
      stopCapture();
      bar?.remove();
    },
    isDirty: dirty,
  };
}

/* ====================================================================== */
/* page: 历史                                                              */
/* ====================================================================== */

function HistoryPage() {
  let view = null;
  let feed = null;
  let feedStatus = null;
  let totalsEl = null;
  let cursor = null;
  let loading = false;
  let exhausted = false;
  let activeAudio = null; // { article, audio, record }
  const waveformCache = new Map();

  const dateFmt = new Intl.DateTimeFormat("zh-CN", { month: "long", day: "numeric" });
  const weekdayFmt = new Intl.DateTimeFormat("zh-CN", { weekday: "short" });
  const timeFmt = new Intl.DateTimeFormat("zh-CN", { hour: "2-digit", minute: "2-digit", second: "2-digit", hour12: false });
  const audioUrl = (record) => withToken(`/api/audio/${encodeURIComponent(record.id)}.wav`);
  const hasAudio = (record) => record.has_audio ?? Boolean(record.audio_path);

  async function loadTotals() {
    try {
      const totals = await api("/api/totals", { quiet: true });
      totalsEl.replaceChildren(
        h("div", {}, h("b", { text: String(totals.records) }), "条记录"),
        h("div", {}, h("b", { text: Number(totals.characters).toLocaleString("zh-CN") }), "字"),
        h("div", {}, h("b", { text: fmtDuration(totals.duration_seconds) }), "语音时长"),
      );
    } catch { /* decorative */ }
  }

  function dayGroup(record) {
    const label = dateFmt.format(new Date(record.created_at));
    let group = $$(".day", feed).find((el) => el.dataset.day === label);
    if (group) return group;
    group = h("section", { class: "day", dataset: { day: label } }, h("div", { class: "label" }, label, h("span", { class: "weekday", text: weekdayFmt.format(new Date(record.created_at)) })));
    feed.append(group);
    return group;
  }

  function recordRow(record) {
    const audio = hasAudio(record);
    const time = timeFmt.format(new Date(record.created_at));
    const processed = record.processed_text && record.processed_text !== record.raw_text;
    const article = h("article", { class: "record", dataset: { id: record.id } });
    const play = h("button", { class: "play", disabled: !audio, "aria-label": "播放", text: "▶" });
    const row = h(
      "div",
      { class: "row", tabindex: "0", role: "button", "aria-expanded": "false", "aria-label": `展开记录：${record.final_text.slice(0, 30)}` },
      play,
      h("div", { class: "main" }, h("div", { class: "text", text: record.final_text }), h("div", { class: "wave-line" }, audio ? h("canvas", { class: "wave" }) : null)),
      h("div", { class: "meta" },
        h("span", { text: time }),
        h("span", { text: fmtDuration(record.duration_seconds) }),
        h("span", { class: "chip", text: record.processing_mode || "quick" }),
        h("span", { class: "chip", text: backendLabel(record.asr_provider) }),
        audio ? null : h("span", { class: "chip dashed", text: "无录音" }),
      ),
    );
    const compareSlot = h("div");
    const retranscribe = h("button", { type: "button", class: "btn small", disabled: !audio, title: audio ? null : "这条记录没有录音" }, "用当前模型重新识别");
    const correct = h("button", { type: "button", class: "btn small" }, "纠错");
    const remove = h("button", { type: "button", class: "btn small danger" }, "删除");
    const expand = h(
      "div",
      { class: "expand" },
      audio ? [
        h("canvas", { class: "wave-big" }),
        h("div", { class: "player-line" }, h("span", { class: "t-cur", text: "0:00" }), h("input", { type: "range", min: "0", max: "1000", value: "0", "aria-label": "播放进度" }), h("span", { class: "t-dur", text: fmtDuration(record.duration_seconds) })),
      ] : null,
      h("div", { class: "final-text", text: record.final_text }),
      processed || record.raw_text !== record.final_text ? h("div", { class: "raw-text", text: record.raw_text }) : null,
      h("div", { class: "record-actions" }, retranscribe, correct, remove),
      compareSlot,
    );
    article.append(row, expand);

    const toggle = () => {
      article.classList.toggle("open");
      row.setAttribute("aria-expanded", String(article.classList.contains("open")));
      if (audio && article.classList.contains("open")) drawWaveforms(article, record);
    };
    row.addEventListener("click", (event) => {
      if (event.target.closest(".play")) return;
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
      play.addEventListener("click", () => togglePlay(article, record));
      drawWaveforms(article, record);
    }
    retranscribe.addEventListener("click", () => busy(retranscribe, () => retranscribeRecord(record, compareSlot)));
    correct.addEventListener("click", () => correctRecord(record));
    remove.addEventListener("click", () => deleteRecord(record, article, remove));
    dayGroup(record).append(article);
  }

  async function retranscribeRecord(record, slot) {
    try {
      const result = await api(`/api/records/${encodeURIComponent(record.id)}/retranscribe`, { method: "POST" });
      const next = result.cleaned ?? result.text ?? "";
      slot.replaceChildren(
        h(
          "div",
          { class: "compare" },
          h("div", { class: "compare-head" },
            h("span", { text: `当前模型：${result.engine || "—"}` }),
            h("span", { text: `耗时 ${fmtSeconds(result.seconds, 2)} 秒` }),
            h("span", { text: next === record.final_text ? "结果与原来相同" : "红色为原结果，绿色为新结果" }),
          ),
          renderDiff(record.final_text, next),
        ),
      );
    } catch (error) {
      if (isDaemonDown(error)) slot.replaceChildren(h("div", { class: "transcript-note err", text: "需要后台进程加载本地模型后才能重新识别" }));
    }
  }

  async function correctRecord(record) {
    let corrected = record.final_text;
    let suggestions = [];
    const textarea = h("textarea", { rows: "4", value: corrected, "aria-label": "正确的文字", oninput: (e) => { corrected = e.target.value; } });
    const list = h("div", { class: "suggestions" });
    const hint = h("p", { class: "hint", style: { margin: 0 }, text: "把文字改成正确的样子，然后生成建议。" });
    const save = { label: "加入词库", value: "save", class: "primary" };

    function renderSuggestions() {
      if (!suggestions.length) {
        list.replaceChildren();
        hint.textContent = "没有找到可以加入词库的差异。";
        return;
      }
      hint.textContent = "勾选要加入的条目，选择加入替换规则还是热词：";
      list.replaceChildren(
        ...suggestions.map((item) => {
          const target = h("select", { "aria-label": "加入到", onchange: (e) => { item.target = e.target.value; } }, h("option", { value: "replacement" }, "替换规则"), h("option", { value: "hotword" }, "热词"));
          target.value = item.target;
          return h(
            "label",
            { class: "suggestion" },
            h("input", { type: "checkbox", checked: item.checked, onchange: (e) => { item.checked = e.target.checked; } }),
            h("span", { class: "pair" }, h("del", { class: "diff", text: item.from || "（空）" }), h("span", { class: "arrow", text: "→" }), h("ins", { class: "diff", text: item.to })),
            target,
          );
        }),
      );
    }

    const result = await openDialog({
      title: "纠错",
      body: h("div", { class: "stack" }, h("div", { class: "diff", style: { fontSize: "13px", color: "var(--muted)" }, text: `原结果：${record.final_text}` }), textarea, hint, list),
      actions: [
        { label: "取消", value: "" },
        {
          label: "生成建议",
          close: false,
          onClick: async (event) => {
            const button = event.currentTarget;
            await busy(button, async () => {
              try {
                const response = await api("/api/corrections", { method: "POST", body: { original: record.final_text, corrected } });
                suggestions = (response.suggestions || []).map((item) => ({
                  ...item,
                  checked: true,
                  // A Chinese word of 2+ chars makes a good hotword; others a replacement.
                  target: /^[一-鿿]{2,}$/.test(item.to || "") && (item.from || "").length === (item.to || "").length ? "hotword" : "replacement",
                }));
                renderSuggestions();
              } catch { /* toast shown */ }
            });
          },
        },
        save,
      ],
      onOpen: () => textarea.focus(),
    });
    if (result !== "save") return;
    const chosen = suggestions.filter((item) => item.checked && item.to);
    if (!chosen.length) {
      toast("没有选中任何条目", "warn");
      return;
    }
    try {
      const config = await api("/api/config");
      const text = config.text || {};
      const replacements = [...(text.replacements || [])];
      const hotwords = [...(text.hotwords || [])];
      for (const item of chosen) {
        if (item.target === "hotword") {
          if (!hotwords.includes(item.to)) hotwords.push(item.to);
        } else if (item.from && !replacements.some((r) => r.from === item.from)) {
          replacements.push({ from: item.from, to: item.to });
        }
      }
      const response = await api("/api/config", { method: "PUT", body: { text: { replacements, hotwords } } });
      toast(`已加入词库（${chosen.length} 条）`, "ok");
      if (response.restart_engine) await offerEngineReload();
    } catch { /* toast shown */ }
  }

  async function deleteRecord(record, article, button) {
    if (!(await confirmDialog("删除这条记录和它的录音？此操作不能撤销。", { okText: "删除", danger: true }))) return;
    await busy(button, async () => {
      try {
        await api(`/api/records/${encodeURIComponent(record.id)}`, { method: "DELETE" });
        if (activeAudio?.article === article) stopActive();
        const group = article.parentElement;
        article.remove();
        if (group && !$(".record", group)) group.remove();
        toast("已删除", "ok");
        loadTotals();
      } catch { /* toast shown */ }
    });
  }

  /* ---- playback & waveform (ported from the history console) ---- */

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
    article.classList.add("open");
    setPlaying(article, true);
    audio.addEventListener("ended", () => stopActive());
    audio.addEventListener("timeupdate", () => updateProgress());
    audio.play().catch(() => {
      toast("无法播放这段录音", "err");
      stopActive();
    });
    drawWaveforms(article, record);
  }

  function setPlaying(article, playing) {
    article.classList.toggle("playing", playing);
    const button = $(".play", article);
    button.textContent = playing ? "⏸" : "▶";
    button.setAttribute("aria-label", playing ? "暂停" : "播放");
  }

  function stopActive() {
    if (!activeAudio) return;
    const { article, audio } = activeAudio;
    audio.pause();
    audio.removeAttribute("src");
    setPlaying(article, false);
    activeAudio = null;
  }

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

  function paintWave(canvas, peaks, progress, active) {
    const dpr = window.devicePixelRatio || 1;
    const width = canvas.clientWidth;
    const height = canvas.clientHeight;
    if (!width || !height) return;
    canvas.width = width * dpr;
    canvas.height = height * dpr;
    const ctx = canvas.getContext("2d");
    ctx.scale(dpr, dpr);
    const styles = getComputedStyle(document.documentElement);
    const accent = styles.getPropertyValue("--accent").trim() || "#F0C96C";
    const idle = styles.getPropertyValue("--faint").trim() || "#4A5B80";
    const mid = height / 2;
    const buckets = peaks.length / 2;
    const step = width / buckets;
    ctx.lineWidth = Math.max(1, step * 0.55);
    for (let i = 0; i < buckets; i++) {
      const x = i * step;
      ctx.strokeStyle = active && i / buckets <= progress ? accent : idle;
      const min = peaks[i * 2] * (mid - 1);
      const max = peaks[i * 2 + 1] * (mid - 1);
      ctx.beginPath();
      ctx.moveTo(x, mid + min);
      ctx.lineTo(x, mid + max || mid + min - 0.6);
      ctx.stroke();
    }
  }

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

  async function loadPage() {
    if (loading || exhausted) return;
    loading = true;
    feedStatus.replaceChildren("正在读取记录…");
    try {
      const params = new URLSearchParams({ limit: "100" });
      if (cursor) params.set("cursor", cursor);
      const body = await api(`/api/records?${params}`, { quiet: true });
      if (!body.records.length && !cursor) {
        feed.replaceChildren(emptyState("还没有记录", "说一句话，记录就会出现在这里。"));
        feedStatus.replaceChildren();
        exhausted = true;
        return;
      }
      body.records.forEach(recordRow);
      cursor = body.next_cursor;
      if (!cursor) {
        exhausted = true;
        feedStatus.replaceChildren("已经到最早的一条了");
      } else {
        feedStatus.replaceChildren(h("button", { type: "button", class: "btn", onclick: loadPage }, "加载更早的记录"));
      }
    } catch (error) {
      feedStatus.replaceChildren(h("span", { class: "transcript-note err", text: `读取失败：${humanError(error)}` }));
    } finally {
      loading = false;
    }
  }

  async function settingsCard() {
    let config;
    try {
      config = await api("/api/config", { quiet: true });
    } catch {
      return null;
    }
    const historyConfig = clone(config.history || {});
    const persist = async () => {
      try {
        await api("/api/config", { method: "PUT", body: { history: historyConfig } });
        toast("记录设置已保存", "ok");
      } catch { /* toast shown */ }
    };
    return card(
      "记录设置",
      "[history]",
      h("div", { class: "switches" },
        toggleInput(historyConfig, "enabled", persist, "保存识别记录"),
        toggleInput(historyConfig, "save_audio", persist, "同时保存录音", "用于回放、重新识别和回放基准"),
      ),
    );
  }

  return {
    async mount(target) {
      view = target;
      totalsEl = h("div", { class: "totals" });
      feed = h("div", {});
      feedStatus = h("div", { class: "feed-status" });
      const head = h("header", { class: "page-head" }, h("div", {}, h("h1", { text: "历史" }), h("div", { class: "sub", text: "每一次识别的结果和录音" })), totalsEl);
      view.append(head);
      const settings = await settingsCard();
      if (settings) view.append(settings);
      view.append(feed, feedStatus);
      loadTotals();
      await loadPage();
    },
    unmount() {
      stopActive();
    },
  };
}

/// PCM16 mono payload of a WAV container.
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

function DiagnosticsPage() {
  let view = null;
  const doctorSlot = h("div", {}, h("div", { class: "hint", text: "检查中…" }));
  const gpuSlot = h("div", {}, h("div", { class: "hint", text: "读取中…" }));
  const logPre = h("pre", { class: "log", tabindex: "0", "aria-label": "后台日志" });
  const benchSlot = h("div");

  async function loadDoctor(button) {
    await busy(button, async () => {
      try {
        const checks = await api("/api/doctor", { quiet: true });
        doctorSlot.replaceChildren(
          h("table", {}, h("tbody", {}, checks.map((check) => {
            const ok = check.present;
            const icon = ok ? ["✓", "ok", "通过"] : check.required ? ["✗", "fail", "失败"] : ["–", "opt", "可选，未安装"];
            return h(
              "tr",
              {},
              h("td", { class: `check-icon ${icon[1]}`, "aria-label": icon[2], text: icon[0] }),
              h("td", { style: { whiteSpace: "nowrap" }, text: check.name }),
              h("td", { class: "mono", style: { fontSize: "12.5px", color: "var(--muted)", overflowWrap: "anywhere" }, text: check.detail }),
            );
          }))),
        );
      } catch (error) {
        doctorSlot.replaceChildren(h("div", { class: "transcript-note err", text: humanError(error) }));
      }
    });
  }

  async function loadGpu() {
    try {
      const status = await fetchStatus();
      const gpus = status.gpu || [];
      const engine = status.engine || {};
      gpuSlot.replaceChildren(
        h("dl", { class: "kv", style: { marginBottom: "12px" } },
          h("dt", { text: "引擎" }), h("dd", { text: engine.description || backendLabel(status.backend) }),
          h("dt", { text: "状态" }), h("dd", { text: engine.error ? `加载失败：${engine.error}` : engine.loaded ? "已加载" : "未加载" }),
          h("dt", { text: "版本" }), h("dd", { text: status.version || "–" }),
        ),
        gpus.length ? h("div", {}, gpus.map(gpuBar)) : h("div", { class: "hint", text: "没有检测到 NVIDIA GPU（或缺少 nvidia-smi）。" }),
      );
    } catch (error) {
      gpuSlot.replaceChildren(h("div", { class: "hint", text: isDaemonDown(error) ? "后台进程未运行，无法读取引擎和 GPU 信息。" : humanError(error) }));
    }
  }

  async function loadLogs(button) {
    await busy(button, async () => {
      try {
        const logs = await api("/api/logs", { quiet: true });
        logPre.textContent = (logs.lines || []).join("\n") || "（暂无日志）";
        logPre.scrollTop = logPre.scrollHeight;
      } catch (error) {
        logPre.textContent = isDaemonDown(error) ? "后台进程未运行。可以用 journalctl --user -u voiceless 查看系统日志。" : humanError(error);
      }
    });
  }

  async function copyLogs() {
    try {
      await navigator.clipboard.writeText(logPre.textContent);
      toast("日志已复制", "ok");
    } catch {
      const range = document.createRange();
      range.selectNodeContents(logPre);
      const selection = getSelection();
      selection.removeAllRanges();
      selection.addRange(range);
      toast("已选中日志，按 Ctrl+C 复制", "info");
    }
  }

  async function runBenchmark(button, limitInput) {
    const limit = Math.max(1, Math.min(50, Number(limitInput.value) || 10));
    storage.set("benchLimit", limit);
    benchSlot.replaceChildren(h("div", { class: "hint", text: `正在用当前引擎识别最近 ${limit} 条录音…` }));
    await busy(button, async () => {
      try {
        const result = await api("/api/benchmark", { method: "POST", body: { limit } });
        renderBenchmark(result);
      } catch (error) {
        benchSlot.replaceChildren(h("div", { class: "transcript-note err", text: isDaemonDown(error) ? "需要后台进程加载本地模型后才能运行回放基准。" : humanError(error) }));
      }
    });
  }

  function renderBenchmark(result) {
    const rows = result.results || [];
    if (!rows.length) {
      benchSlot.replaceChildren(emptyState("没有可用的录音", "历史记录中需要有保存了录音的条目。"));
      return;
    }
    const cerClass = (cer) => (cer == null ? "" : cer <= 0.05 ? "ok" : cer <= 0.15 ? "warn" : "err");
    benchSlot.replaceChildren(
      h("div", { class: "stats", style: { margin: "6px 0 16px" } },
        h("div", { class: "stat" }, h("div", { class: "k", text: "引擎" }), h("div", { class: "v", style: { fontSize: "14px", fontFamily: "var(--sans)" }, text: result.engine || "–" })),
        h("div", { class: "stat" }, h("div", { class: "k", text: "平均实时率" }), h("div", { class: "v", text: fmtSeconds(result.mean_rtf, 3) })),
        h("div", { class: "stat" }, h("div", { class: "k", text: "平均字错率" }), h("div", { class: "v" }, result.mean_cer != null ? (result.mean_cer * 100).toFixed(1) : "–", h("small", { text: "%" }))),
        h("div", { class: "stat" }, h("div", { class: "k", text: "条数" }), h("div", { class: "v", text: String(rows.length) })),
      ),
      h("p", { class: "hint", text: "实时率 = 识别耗时 ÷ 语音时长，越小越快。字错率以这条记录当时的识别原文为参考（可能来自云端），只作相对比较。" }),
      h("div", { class: "table-wrap" }, h(
        "table",
        {},
        h("thead", {}, h("tr", {}, h("th", { class: "num", text: "时长" }), h("th", { class: "num", text: "耗时" }), h("th", { class: "num", text: "实时率" }), h("th", { class: "num", text: "字错率" }), h("th", { text: "对比（红：参考 · 绿：本次）" }))),
        h("tbody", {}, rows.map((row) =>
          h(
            "tr",
            {},
            h("td", { class: "num", text: `${fmtSeconds(row.audio_seconds)}s` }),
            h("td", { class: "num", text: `${fmtSeconds(row.decode_seconds, 2)}s` }),
            h("td", { class: "num", text: fmtSeconds(row.rtf, 3) }),
            h("td", { class: "num" }, h("span", { class: `chip ${cerClass(row.cer)}`, text: row.cer != null ? `${(row.cer * 100).toFixed(1)}%` : "–" })),
            h("td", { style: { fontSize: "13.5px", minWidth: "260px" } }, renderDiff(row.reference || "", row.text || "")),
          ),
        )),
      )),
    );
  }

  return {
    async mount(target) {
      view = target;
      const doctorButton = h("button", { type: "button", class: "btn small", onclick: (event) => loadDoctor(event.currentTarget) }, "重新检查");
      const logButton = h("button", { type: "button", class: "btn small", onclick: (event) => loadLogs(event.currentTarget) }, "刷新");
      const copyButton = h("button", { type: "button", class: "btn small", onclick: copyLogs }, "复制");
      const limitInput = h("input", { type: "number", min: "1", max: "50", value: String(storage.get("benchLimit", 10)), style: { width: "90px", flex: "none" }, "aria-label": "录音条数" });
      const benchButton = h("button", { type: "button", class: "btn primary", onclick: (event) => runBenchmark(event.currentTarget, limitInput) }, "运行回放基准");
      view.append(
        pageHead("诊断", "依赖检查、GPU、日志和回放基准"),
        h("section", { class: "card" }, h("h2", {}, "环境检查", h("span", { style: { marginLeft: "auto" } }, doctorButton)), doctorSlot),
        h("div", { class: "card-row" },
          card("引擎与 GPU", null, gpuSlot),
          h("section", { class: "card" }, h("h2", {}, "后台日志", h("span", { class: "actions", style: { marginLeft: "auto" } }, logButton, copyButton)), logPre),
        ),
        card(
          "回放基准",
          null,
          h("p", { class: "hint", text: "用当前加载的本地模型重新识别最近的历史录音，测速度和准确率。识别期间会占用 GPU。" }),
          h("div", { class: "inline", style: { marginBottom: "14px" } }, h("label", { class: "hint", style: { margin: 0 }, text: "最近" }), limitInput, h("span", { class: "hint", style: { margin: 0 }, text: "条录音" }), benchButton),
          benchSlot,
        ),
      );
      await Promise.all([loadDoctor(doctorButton), loadGpu(), loadLogs(logButton)]);
    },
  };
}

/* ====================================================================== */
/* router                                                                 */
/* ====================================================================== */

const PAGES = [
  { id: "live", title: "实时", icon: "◉", create: LivePage },
  { id: "engine", title: "识别引擎", icon: "◈", create: EnginePage },
  { id: "words", title: "词库", icon: "❝", create: TextPage },
  { id: "modes", title: "模式与提示词", icon: "✎", create: ModesPage },
  { id: "hotkey", title: "快捷键与上屏", icon: "⌨", create: HotkeyPage },
  { id: "history", title: "历史", icon: "☰", create: HistoryPage },
  { id: "diagnostics", title: "诊断", icon: "✚", create: DiagnosticsPage },
];

let current = null; // { id, page }
let navigating = false;

function renderNav() {
  $("#nav").replaceChildren(
    ...PAGES.map((page) =>
      h("li", {}, h("a", { href: `#${page.id}`, dataset: { page: page.id } }, h("span", { class: "icon", "aria-hidden": "true", text: page.icon }), page.title)),
    ),
  );
}

async function navigate() {
  if (navigating) return;
  const requested = location.hash.slice(1) || storage.get("page", "live");
  const target = PAGES.find((page) => page.id === requested) || PAGES[0];
  if (current && current.id === target.id) return;
  navigating = true;
  try {
    if (current?.page.isDirty?.()) {
      const leave = await confirmDialog("当前页面有未保存的更改，确定离开吗？", { okText: "放弃更改并离开", danger: true });
      if (!leave) {
        history.replaceState(null, "", `#${current.id}`);
        return;
      }
    }
    current?.page.unmount?.();
    const view = $("#view");
    view.replaceChildren();
    const page = target.create();
    current = { id: target.id, page };
    storage.set("page", target.id);
    if (location.hash !== `#${target.id}`) history.replaceState(null, "", `#${target.id}`);
    document.title = `${target.title} · Voiceless 控制台`;
    $$("#nav a").forEach((a) => {
      if (a.dataset.page === target.id) a.setAttribute("aria-current", "page");
      else a.removeAttribute("aria-current");
    });
    view.focus({ preventScroll: true });
    window.scrollTo(0, 0);
    await page.mount(view);
  } catch (error) {
    console.error(error);
    $("#view").append(emptyState("页面加载失败", humanError(error)));
  } finally {
    navigating = false;
    // A nav click that arrived while this page was mounting.
    const wanted = location.hash.slice(1);
    if (current && wanted && wanted !== current.id) setTimeout(navigate);
  }
}

window.addEventListener("hashchange", navigate);
window.addEventListener("beforeunload", (event) => {
  if (current?.page.isDirty?.()) {
    event.preventDefault();
    event.returnValue = "";
  }
});

document.addEventListener("DOMContentLoaded", () => {
  renderNav();
  renderNavStatus();
  live.connect();
  fetchStatus().catch(() => {});
  setInterval(() => fetchStatus().catch(() => {}), 15_000);
  navigate();
});
