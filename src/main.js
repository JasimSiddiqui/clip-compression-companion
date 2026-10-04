// ===========================================================================
// Clip Compression Companion frontend logic
// Keeps the queue, sends one job at a time to the Rust backend over Tauri's
// invoke bridge, and updates rows in place as progress events arrive.
// ===========================================================================

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;
const dialog = window.__TAURI__.dialog;
const { getCurrentWebview } = window.__TAURI__.webview;
const { getCurrentWindow } = window.__TAURI__.window;

const REPO_URL = "https://github.com/JasimSiddiqui/clip-compression-companion";
const SETTINGS_KEY = "ccc-settings";
const THEME_KEY = "ccc-theme";

const DEFAULTS = {
  video: {
    mode: "size",
    targetMb: 10,
    quality: 3,
    codec: "h264",
    resolution: "auto",
    fps: "original",
    audio: "128",
    speed: "balanced",
    gpu: false,
  },
  audio: { format: "mp3", bitrate: 128, mono: false },
  image: { format: "keep", quality: 80, maxSize: 0, reduceColors: false },
  other: { level: "normal" },
  output: { dir: null, suffix: "-compressed" },
};

const QUALITY_LABELS = ["Smallest", "Small", "Balanced", "High", "Best"];
const ACTIVE = ["probing", "ready", "queued", "working"];
const FINISHED = ["done", "skipped", "failed", "cancelled"];

const state = {
  items: [],
  nextId: 1,
  running: false,
  tab: "video",
  caps: null,
  settings: loadSettings(),
};

const el = {
  summary: document.getElementById("summary"),
  themeToggle: document.getElementById("theme-toggle"),
  addFiles: document.getElementById("add-files"),
  notice: document.getElementById("notice"),
  jobs: document.getElementById("jobs"),
  queueCount: document.getElementById("queue-count"),
  clearFinished: document.getElementById("clear-finished"),
  cancelAll: document.getElementById("cancel-all"),
  dropzone: document.getElementById("dropzone"),
  browse: document.getElementById("browse"),
  options: document.querySelector(".options"),
  compress: document.getElementById("compress"),
  outputLabel: document.getElementById("output-label"),
  chooseDir: document.getElementById("choose-dir"),
  resetDir: document.getElementById("reset-dir"),
  dropOverlay: document.getElementById("drop-overlay"),
  toasts: document.getElementById("toasts"),
  sourceLink: document.getElementById("source-link"),
  qualityLabel: document.getElementById("quality-label"),
  imageQualityLabel: document.getElementById("image-quality-label"),
  targetHint: document.getElementById("target-hint"),
  resolutionHint: document.getElementById("resolution-hint"),
  gpuHint: document.getElementById("gpu-hint"),
  codecHint: document.getElementById("codec-hint"),
  audioPerMinute: document.getElementById("audio-per-minute"),
};

const ICONS = {
  video:
    '<rect x="3" y="3" width="18" height="18" rx="2"/><path d="M7 3v18M17 3v18M3 7.5h4M3 12h18M3 16.5h4M17 7.5h4M17 16.5h4"/>',
  audio: '<path d="M9 18V5l12-2v13"/><circle cx="6" cy="18" r="3"/><circle cx="18" cy="16" r="3"/>',
  image:
    '<rect x="3" y="3" width="18" height="18" rx="2"/><circle cx="9" cy="9" r="2"/><path d="m21 15-3.1-3.1a2 2 0 0 0-2.8 0L6 21"/>',
  other: '<rect x="2" y="3" width="20" height="5" rx="1"/><path d="M4 8v11a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8M10 12h4"/>',
  folder:
    '<path d="M4 20h16a2 2 0 0 0 2-2V8a2 2 0 0 0-2-2h-7.9a2 2 0 0 1-1.7-.9l-.8-1.2A2 2 0 0 0 7.9 3H4a2 2 0 0 0-2 2v13c0 1.1.9 2 2 2Z"/>',
  close: '<path d="M18 6 6 18M6 6l12 12"/>',
};

const svg = (paths, size = 18) =>
  `<svg viewBox="0 0 24 24" width="${size}" height="${size}" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">${paths}</svg>`;

// ----------------------------------------------------------------- helpers

const pad = (n) => String(n).padStart(2, "0");

function formatBytes(bytes) {
  if (bytes < 1024) return `${bytes} B`;
  const units = ["KB", "MB", "GB", "TB"];
  let value = bytes / 1024;
  let i = 0;
  while (value >= 1024 && i < units.length - 1) {
    value /= 1024;
    i++;
  }
  return `${value >= 100 ? Math.round(value) : value.toFixed(1)} ${units[i]}`;
}

function formatClock(seconds) {
  const s = Math.round(seconds);
  const h = Math.floor(s / 3600);
  const m = Math.floor(s / 60) % 60;
  return h ? `${h}:${pad(m)}:${pad(s % 60)}` : `${m}:${pad(s % 60)}`;
}

function formatEta(seconds) {
  if (seconds < 60) return `${Math.max(1, Math.round(seconds))}s`;
  const m = Math.floor(seconds / 60);
  if (m >= 60) return `${Math.floor(m / 60)}h ${m % 60}m`;
  return `${m}m ${pad(Math.round(seconds % 60))}s`;
}

const plural = (n, word) => `${n} ${word}${n === 1 ? "" : "s"}`;
const basename = (path) => path.split(/[\\/]/).filter(Boolean).pop() || path;
const extension = (name) => (name.includes(".") ? name.split(".").pop().toUpperCase() : "");
const byId = (id) => state.items.find((i) => i.id === id);

function toast(message, kind = "") {
  const node = document.createElement("div");
  node.className = `toast ${kind ? `is-${kind}` : ""}`;
  const text = document.createElement("span");
  text.textContent = message;
  node.append(text);
  el.toasts.append(node);
  setTimeout(() => {
    node.classList.add("is-leaving");
    setTimeout(() => node.remove(), 220);
  }, 4200);
}

// ----------------------------------------------------------------- settings

function loadSettings() {
  const settings = structuredClone(DEFAULTS);
  try {
    const saved = JSON.parse(localStorage.getItem(SETTINGS_KEY) || "{}");
    for (const group of Object.keys(settings)) {
      Object.assign(settings[group], saved[group] || {});
    }
  } catch {
    // A damaged settings blob just means defaults.
  }
  return settings;
}

function saveSettings() {
  try {
    localStorage.setItem(SETTINGS_KEY, JSON.stringify(state.settings));
  } catch {}
}

function getSetting(key) {
  const [group, name] = key.split(".");
  return state.settings[group][name];
}

function setSetting(key, value) {
  const [group, name] = key.split(".");
  state.settings[group][name] = value;
  saveSettings();
  syncControls();
  renderChrome();
}

function parseValue(node, raw) {
  return node.dataset.type === "number" ? Number(raw) : raw;
}

/** "video.mode=size", "image.format=keep|png", or "image.format!=png". */
function isShown(expr) {
  const negate = expr.includes("!=");
  const [key, values] = expr.split(negate ? "!=" : "=");
  const match = values.split("|").includes(String(getSetting(key)));
  return negate ? !match : match;
}

/** Brings every control in the options panel in line with `state.settings`. */
function syncControls() {
  for (const node of el.options.querySelectorAll("[data-setting]")) {
    const value = getSetting(node.dataset.setting);
    if (node.matches(".seg, .chips")) {
      for (const btn of node.querySelectorAll("[data-value]")) {
        btn.classList.toggle("is-active", String(parseValue(node, btn.dataset.value)) === String(value));
      }
    } else if (node.type === "checkbox") {
      node.checked = Boolean(value);
    } else if (document.activeElement !== node) {
      node.value = value;
    }
  }
  for (const node of el.options.querySelectorAll("[data-show]")) {
    node.hidden = !isShown(node.dataset.show);
  }

  const v = state.settings.video;
  el.qualityLabel.textContent = QUALITY_LABELS[v.quality - 1] || "";
  el.imageQualityLabel.textContent = state.settings.image.quality;
  el.targetHint.textContent =
    {
      10: "Fits Discord's free upload limit.",
      25: "Fits most email attachments.",
    }[v.targetMb] || "Sound included. Two passes keep it on target.";
  el.resolutionHint.textContent =
    v.mode === "size" && v.resolution === "auto"
      ? "Auto lowers the resolution when the size is tight, so it stays sharp."
      : "";
  el.audioPerMinute.textContent = formatBytes((state.settings.audio.bitrate * 1000 * 60) / 8);

  // Hardware encoding is per codec; only offer it where a test encode worked.
  const gpuToggle = el.options.querySelector('[data-setting="video.gpu"]');
  const caps = state.caps;
  const gpu = caps && caps.gpu[v.codec];
  gpuToggle.disabled = !gpu;
  if (!gpu) gpuToggle.checked = false;
  el.gpuHint.textContent = !caps
    ? "Checking for a hardware encoder…"
    : gpu
      ? `${gpu.label}. Much faster, slightly lower quality for the size.`
      : "No hardware encoder for this format on this computer.";
  el.codecHint.textContent =
    v.codec === "av1" && !(gpu && v.gpu) && caps?.software.av1 === "libaom-av1"
      ? `AV1 on the processor is slow: expect a few minutes per minute of video.${gpu ? " The graphics card is much faster." : ""}`
      : "";

  if (caps) {
    for (const option of el.options.querySelectorAll('[data-setting="video.codec"] option')) {
      option.disabled = !caps.software[option.value];
    }
  }

  const dir = state.settings.output.dir;
  el.outputLabel.textContent = dir || "Next to the original";
  el.outputLabel.title = dir || "";
  el.resetDir.hidden = !dir;
}

function setTab(tab) {
  state.tab = tab;
  for (const btn of el.options.querySelectorAll("[data-tab]")) {
    btn.classList.toggle("is-active", btn.dataset.tab === tab);
    btn.setAttribute("aria-selected", btn.dataset.tab === tab);
  }
  for (const pane of el.options.querySelectorAll("[data-pane]")) {
    pane.hidden = pane.dataset.pane !== tab;
  }
}

// ----------------------------------------------------------------- queue

function addPaths(paths) {
  const pending = new Set(state.items.filter((i) => ACTIVE.includes(i.status)).map((i) => i.path));
  for (const path of paths) {
    if (pending.has(path)) continue;
    pending.add(path);
    const item = { id: state.nextId++, path, name: basename(path), status: "probing", kind: null, size: 0 };
    state.items.push(item);
    renderItem(item);
    probe(item);
  }
  renderChrome();
}

async function probe(item) {
  item.status = "probing";
  item.error = null;
  renderItem(item);
  try {
    const info = await invoke("probe", { path: item.path });
    Object.assign(item, {
      name: info.name,
      kind: info.kind,
      size: info.size,
      isDir: info.isDir,
      media: info.media,
      status: "ready",
      unreadable: false,
    });
    // Show the options for the first file still waiting, whichever finishes reading first.
    const first = state.items.find((i) => ACTIVE.includes(i.status) && i.kind);
    if (first) setTab(first.kind);
  } catch (e) {
    Object.assign(item, { status: "failed", error: String(e), unreadable: true });
  }
  if (state.items.includes(item)) {
    renderItem(item);
    renderChrome();
  }
}

/** Queues every ready file with a snapshot of the current options for its kind. */
function queueReady() {
  const { dir, suffix } = state.settings.output;
  const cleanSuffix = suffix.replace(/[<>:"/\\|?*\u0000-\u001f]/g, "");
  for (const item of state.items) {
    if (item.status !== "ready") continue;
    item.status = "queued";
    item.job = {
      outputDir: dir,
      suffix: cleanSuffix,
      settings: { kind: item.kind, ...structuredClone(state.settings[item.kind]) },
    };
    renderItem(item);
  }
  renderChrome();
  pump();
}

async function pump() {
  if (state.running) return;
  state.running = true;
  renderChrome();
  const finished = [];

  let item;
  while ((item = state.items.find((i) => i.status === "queued"))) {
    item.status = "working";
    item.progress = { percent: 0, phase: "Starting", eta: null };
    renderItem(item);
    renderChrome();
    try {
      const result = await invoke("compress", { job: { id: item.id, path: item.path, ...item.job } });
      item.result = result;
      item.status = result.status;
    } catch (e) {
      if (e === "cancelled") {
        item.status = "cancelled";
      } else {
        item.status = "failed";
        item.error = String(e);
      }
    }
    item.progress = null;
    finished.push(item);
    if (state.items.includes(item)) renderItem(item);
    renderChrome();
  }

  state.running = false;
  renderChrome();

  const done = finished.filter((i) => i.status === "done");
  const saved = done.reduce((sum, i) => sum + Math.max(0, i.result.inputSize - i.result.outputSize), 0);
  if (done.length) toast(`Finished ${plural(done.length, "file")}, ${formatBytes(saved)} smaller.`, "ok");
}

function removeItem(item) {
  if (item.status === "working") return;
  state.items = state.items.filter((i) => i !== item);
  el.jobs.querySelector(`[data-id="${item.id}"]`)?.remove();
  renderChrome();
}

function cancelItem(item) {
  if (item.status === "working") invoke("cancel", { id: item.id });
}

function retryItem(item) {
  if (item.unreadable) {
    probe(item);
  } else {
    item.status = "ready";
    item.error = null;
    item.result = null;
    renderItem(item);
    renderChrome();
  }
}

// ----------------------------------------------------------------- rendering

function describe(item) {
  if (item.status === "probing") return "Reading file details…";
  const m = item.media;
  const parts = [];
  if (item.kind === "video" && m) {
    if (m.width) parts.push(`${m.width}×${m.height}`);
    if (m.fps) parts.push(`${Math.round(m.fps)} fps`);
    if (m.duration) parts.push(formatClock(m.duration));
  } else if (item.kind === "audio" && m) {
    parts.push(extension(item.name));
    if (m.duration) parts.push(formatClock(m.duration));
  } else if (item.kind === "image" && m) {
    parts.push(extension(item.name));
    if (m.width) parts.push(`${m.width}×${m.height}`);
  } else {
    parts.push(item.isDir ? "Folder" : extension(item.name) || "File");
  }
  if (item.size) parts.push(formatBytes(item.size));
  return parts.filter(Boolean).join(" · ");
}

function statusText(item) {
  switch (item.status) {
    case "probing":
      return "Reading…";
    case "ready":
      return "Ready";
    case "queued":
      return "Waiting";
    case "working": {
      const p = item.progress || {};
      let text = p.phase || "Working";
      if (p.percent != null) text += ` ${Math.floor(p.percent)}%`;
      if (p.eta != null && p.percent > 2) text += ` · ${formatEta(p.eta)} left`;
      return text;
    }
    case "done":
      return "Done";
    case "skipped":
      return "Skipped";
    case "failed":
      return "Failed";
    case "cancelled":
      return "Cancelled";
  }
  return "";
}

function createRow(item) {
  const li = document.createElement("li");
  li.dataset.id = item.id;
  li.innerHTML = `
    <div class="job__icon"></div>
    <div class="job__body">
      <div class="job__top"><span class="job__name"></span><span class="job__status"></span></div>
      <div class="job__meta"></div>
      <div class="bar" hidden><div class="bar__fill"></div></div>
      <div class="job__result" hidden><span class="job__sizes"></span><span class="save"></span><span class="job__file"></span></div>
      <p class="job__note" hidden></p>
    </div>
    <div class="job__actions"></div>`;
  el.jobs.append(li);
  return li;
}

function renderItem(item) {
  const li = el.jobs.querySelector(`[data-id="${item.id}"]`) || createRow(item);
  li.className = `job is-${item.status}`;

  const icon = li.querySelector(".job__icon");
  const iconKey = item.isDir ? "folder" : item.kind || "other";
  if (icon.dataset.icon !== iconKey) {
    icon.dataset.icon = iconKey;
    icon.innerHTML = svg(ICONS[iconKey]);
  }

  const name = li.querySelector(".job__name");
  name.textContent = item.name;
  name.title = item.path;
  li.querySelector(".job__meta").textContent = describe(item);
  li.querySelector(".job__status").textContent = statusText(item);

  // Progress bar: determinate when the length is known, sliding otherwise.
  const bar = li.querySelector(".bar");
  const working = item.status === "working";
  bar.hidden = !working;
  if (working) {
    const percent = item.progress?.percent;
    bar.classList.toggle("is-indeterminate", percent == null);
    bar.firstElementChild.style.width = percent == null ? "" : `${percent}%`;
  }

  // Result line.
  const result = li.querySelector(".job__result");
  const r = item.result;
  result.hidden = item.status !== "done";
  if (item.status === "done") {
    const saving = Math.round((1 - r.outputSize / r.inputSize) * 100);
    li.querySelector(".job__sizes").textContent = `${formatBytes(r.inputSize)} → ${formatBytes(r.outputSize)}`;
    const save = li.querySelector(".save");
    save.textContent = `−${saving}%`;
    save.hidden = saving <= 0;
    const file = li.querySelector(".job__file");
    file.textContent = basename(r.output);
    file.title = r.output;
  }

  const note = li.querySelector(".job__note");
  const noteText =
    item.status === "failed" ? item.error : ["done", "skipped"].includes(item.status) ? r?.note : null;
  note.hidden = !noteText;
  note.textContent = noteText || "";

  // Actions only change with the status, so leave them alone during progress.
  const actions = li.querySelector(".job__actions");
  if (actions.dataset.status !== item.status) {
    actions.dataset.status = item.status;
    const buttons = [];
    if (item.status === "working") buttons.push('<button class="act act--cancel" data-act="cancel" type="button">Cancel</button>');
    if (item.status === "done") buttons.push(`<button class="act" data-act="reveal" type="button">${svg(ICONS.folder, 13)}Show</button>`);
    if (["failed", "cancelled"].includes(item.status)) buttons.push('<button class="act" data-act="retry" type="button">Retry</button>');
    if (item.status !== "working")
      buttons.push(`<button class="act act--icon" data-act="remove" type="button" title="Remove from list" aria-label="Remove from list">${svg(ICONS.close, 14)}</button>`);
    actions.innerHTML = buttons.join("");
  }
}

/** Everything around the rows: header summary, counts, buttons. */
function renderChrome() {
  const items = state.items;
  const count = (status) => items.filter((i) => i.status === status).length;
  const ready = count("ready");
  const working = count("working") + count("queued");
  const done = items.filter((i) => i.status === "done");
  const saved = done.reduce((sum, i) => sum + Math.max(0, i.result.inputSize - i.result.outputSize), 0);

  el.dropzone.hidden = items.length > 0;
  el.jobs.hidden = items.length === 0;
  el.queueCount.textContent = items.length ? plural(items.length, "file") : "";
  el.clearFinished.hidden = !items.some((i) => FINISHED.includes(i.status));
  el.cancelAll.hidden = working === 0;

  if (!items.length) {
    el.summary.textContent = "Drop in a clip to get started";
  } else {
    const parts = [plural(items.length, "file")];
    if (working) parts.push(`${working} in progress`);
    if (done.length) parts.push(`${done.length} done`);
    if (saved > 0) parts.push(`${formatBytes(saved)} saved`);
    el.summary.textContent = parts.join(" · ");
  }

  if (ready) {
    el.compress.textContent = state.running ? `Add ${plural(ready, "file")} to the queue` : `Compress ${plural(ready, "file")}`;
    el.compress.disabled = false;
  } else {
    el.compress.textContent = state.running ? "Compressing…" : "Compress";
    el.compress.disabled = true;
  }

  for (const badge of el.options.querySelectorAll("[data-count]")) {
    const n = items.filter((i) => i.kind === badge.dataset.count && i.status === "ready").length;
    badge.textContent = n || "";
  }
}

// ----------------------------------------------------------------- theme

function applyTheme(theme) {
  document.documentElement.setAttribute("data-theme", theme);
  el.themeToggle.title = theme === "dark" ? "Switch to light" : "Switch to dark";
  // Match the native title bar too.
  getCurrentWindow().setTheme(theme).catch(() => {});
}

function initTheme() {
  applyTheme(document.documentElement.getAttribute("data-theme") || "light");
  // Until a choice is made, keep following the system.
  matchMedia("(prefers-color-scheme: dark)").addEventListener("change", (e) => {
    let saved = null;
    try {
      saved = localStorage.getItem(THEME_KEY);
    } catch {}
    if (!saved) applyTheme(e.matches ? "dark" : "light");
  });
}

function toggleTheme() {
  const next = document.documentElement.getAttribute("data-theme") === "dark" ? "light" : "dark";
  try {
    localStorage.setItem(THEME_KEY, next);
  } catch {}
  applyTheme(next);
}

// ----------------------------------------------------------------- events

async function browse() {
  const picked = await dialog.open({ multiple: true, title: "Add files to compress" });
  if (picked) addPaths(Array.isArray(picked) ? picked : [picked]);
}

async function chooseDir() {
  const picked = await dialog.open({ directory: true, title: "Save compressed files to" });
  if (picked) setSetting("output.dir", picked);
}

el.options.addEventListener("click", (e) => {
  const tab = e.target.closest("[data-tab]");
  if (tab) return setTab(tab.dataset.tab);
  const btn = e.target.closest("[data-value]");
  const group = btn?.closest("[data-setting]");
  if (group) setSetting(group.dataset.setting, parseValue(group, btn.dataset.value));
});

el.options.addEventListener("input", (e) => {
  const node = e.target.closest("[data-setting]");
  if (!node || node.matches(".seg, .chips")) return;
  if (node.type === "checkbox") return setSetting(node.dataset.setting, node.checked);
  const value = parseValue(node, node.value);
  if (node.dataset.type === "number" && !(value > 0)) return; // ignore half-typed numbers
  setSetting(node.dataset.setting, value);
});

el.options.addEventListener("focusout", (e) => {
  // Put back the stored value if a number box was left empty or invalid.
  if (e.target.matches("[data-setting]")) syncControls();
});

el.jobs.addEventListener("click", (e) => {
  const btn = e.target.closest("[data-act]");
  if (!btn) return;
  const item = byId(Number(btn.closest("[data-id]").dataset.id));
  if (!item) return;
  const act = btn.dataset.act;
  if (act === "remove") removeItem(item);
  else if (act === "cancel") cancelItem(item);
  else if (act === "retry") retryItem(item);
  else if (act === "reveal") invoke("reveal", { path: item.result.output }).catch((err) => toast(String(err), "error"));
});

el.clearFinished.addEventListener("click", () => {
  for (const item of state.items.filter((i) => FINISHED.includes(i.status))) removeItem(item);
});

el.cancelAll.addEventListener("click", () => {
  for (const item of state.items) {
    if (item.status === "queued") {
      item.status = "ready";
      renderItem(item);
    }
  }
  const working = state.items.find((i) => i.status === "working");
  if (working) cancelItem(working);
  renderChrome();
});

el.compress.addEventListener("click", queueReady);
el.addFiles.addEventListener("click", browse);
el.browse.addEventListener("click", browse);
el.chooseDir.addEventListener("click", chooseDir);
el.resetDir.addEventListener("click", () => setSetting("output.dir", null));
el.themeToggle.addEventListener("click", toggleTheme);
el.sourceLink.addEventListener("click", () => invoke("open_url", { url: REPO_URL }).catch(() => {}));

document.addEventListener("keydown", (e) => {
  if (e.ctrlKey && e.key.toLowerCase() === "o") {
    e.preventDefault();
    browse();
  } else if (e.ctrlKey && e.key === "Enter" && !el.compress.disabled) {
    e.preventDefault();
    queueReady();
  }
});

getCurrentWebview().onDragDropEvent(({ payload }) => {
  if (payload.type === "enter" || payload.type === "over") {
    el.dropOverlay.hidden = false;
  } else {
    el.dropOverlay.hidden = true;
    if (payload.type === "drop" && payload.paths?.length) addPaths(payload.paths);
  }
});

listen("progress", ({ payload }) => {
  const item = byId(payload.id);
  if (!item || item.status !== "working") return;
  item.progress = payload;
  renderItem(item);
});

// Files dropped onto the app icon while it's already open.
listen("open-files", ({ payload }) => addPaths(payload));

// ----------------------------------------------------------------- start

async function init() {
  initTheme();
  setTab("video");
  syncControls();
  renderChrome();

  invoke("launch_files").then((paths) => paths.length && addPaths(paths));

  try {
    state.caps = await invoke("capabilities");
  } catch {
    state.caps = { ffmpeg: false, software: {}, gpu: {} };
  }
  if (!state.caps.ffmpeg) {
    el.notice.textContent =
      "FFmpeg couldn't be found, so only zipping works right now. Reinstalling the app should fix this.";
    el.notice.hidden = false;
  }
  if (!state.caps.software[state.settings.video.codec]) state.settings.video.codec = "h264";
  syncControls();
}

init();
