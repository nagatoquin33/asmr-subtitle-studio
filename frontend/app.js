'use strict';

/* =====================================================
 * ASMR 字幕工作室 — 前端主逻辑
 * 依赖 api.js（须先加载）。无框架、无构建，原生 ES2020+。
 *
 * 实时刷新策略：
 *  - 对「进行中」的任务逐个开 SSE（GET /api/tasks/{id}/events），
 *    done/error 后主动 close；
 *  - 每 3s 轮询 GET /api/tasks 兜底（同时负责重建断掉的 SSE）。
 * ===================================================== */

/* ---------------- 常量 ---------------- */
const STATUS_META = {
  queued:       { label: '排队中',   cls: 'st-queued' },
  downloading:  { label: '下载中',   cls: 'st-downloading' },
  extracting:   { label: '抽取音频', cls: 'st-extracting' },
  asr:          { label: '语音识别', cls: 'st-asr' },
  translating:  { label: '翻译中',   cls: 'st-translating' },
  proofreading: { label: '审校中',   cls: 'st-proofreading' },
  writing:      { label: '写入字幕', cls: 'st-writing' },
  done:         { label: '已完成',   cls: 'st-done' },
  error:        { label: '失败',     cls: 'st-error' },
  canceled:     { label: '已取消',   cls: 'st-canceled' },
};
const ACTIVE_STATUSES = new Set(['queued', 'downloading', 'extracting', 'asr', 'translating', 'proofreading', 'writing']);
const FINAL_STATUSES = new Set(['done', 'error', 'canceled']);

const ACCEPT_EXT = new Set([
  'mp3', 'wav', 'flac', 'm4a', 'aac', 'ogg', 'oga', 'opus', 'wma', 'aiff', 'aif', 'ape',
  'mp4', 'mkv', 'mov', 'avi', 'webm', 'flv', 'ts', 'm4v', 'wmv', 'mpg', 'mpeg', '3gp',
]);

const MODEL_STATUS_META = {
  idle:        { label: '未下载', cls: 'st-queued' },
  downloading: { label: '下载中', cls: 'st-downloading' },
  ready:       { label: '就绪',   cls: 'st-done' },
  error:       { label: '失败',   cls: 'st-error' },
};

const PAGE_SIZE = 200;       // 字幕表分页大小
const POLL_MS = 3000;        // 任务列表兜底轮询
const HEALTH_MS = 30000;     // 健康检查轮询
const MODEL_POLL_MS = 2000;  // 模型下载进度轮询
const SSE_CAP = 6;           // 浏览器同源 HTTP/1.1 并发连接约 6 条，超出的进行中任务靠轮询刷新
const QUEUE_PAGE = 100;      // 每次从后端拉取的任务条数（「加载更多」递增）
const PREF_KEY = 'asmr.prefs.v1'; // 本地偏好（语言、排序、上次任务…）

/* ---------------- 全局状态 ---------------- */
const state = {
  tasks: new Map(),        // id -> Task
  selectedId: null,
  segments: null,          // 当前选中任务的段数组
  segmentsFor: null,       // segments 属于哪个任务 id
  segmentsLoading: false,
  segPage: 1,
  hasApiKey: false,
  mockMode: false,
  uploadBusy: false,
  bootstrapped: false,     // 首次任务列表加载是否完成（期间展示骨架屏）
  cfg: null,               // 最近一次读取的配置（就绪检查用）
  health: null,            // 最近一次健康检查结果
  mediaEl: null,           // 当前详情页的 <video>/<audio>
  ui: {
    search: '',            // 队列搜索词
    filter: 'all',         // all | active | done | failed
    sort: 'created',       // created | updated | name | duration
    batch: false,          // 批量选择模式
    selected: new Set(),   // 批量选中的 task id
    limit: QUEUE_PAGE,     // 当前已请求的任务条数
    total: 0,              // 后端总任务数
    activeSeg: null,       // 当前选中的字幕行 index
    segQuery: '',          // 字幕搜索词
    segMatches: [],        // 命中行 index 列表
    segMatchPos: -1,       // 当前定位到第几个命中
    setupDismissed: false, // 本次运行是否手动关掉了就绪条
    askResolve: null,      // 数值输入弹窗的 resolve
    shortcutsOpen: false,
  },
};

const sse = new Map();     // id -> EventSource
const cardEls = new Map(); // id -> {root, name, badge, badgeText, barWrap, fill, pct, msg, meta}
let modelTimer = null;     // 模型下载轮询定时器
const burnState = new Map(); // taskId -> {phase: running|done|error, progress, path}

const $id = (id) => document.getElementById(id);

/* ---------------- 本地偏好（localStorage） ---------------- */
function loadPrefs() {
  try {
    return JSON.parse(localStorage.getItem(PREF_KEY) || '{}') || {};
  } catch (_) {
    return {};
  }
}
function savePrefs(patch) {
  try {
    const next = { ...loadPrefs(), ...patch };
    localStorage.setItem(PREF_KEY, JSON.stringify(next));
  } catch (_) { /* 隐私模式/禁用存储时静默降级 */ }
}
const prefs = loadPrefs();

/* ---------------- 通用小工具（v2.1） ---------------- */
function debounce(fn, ms) {
  let t = null;
  return (...args) => {
    clearTimeout(t);
    t = setTimeout(() => fn(...args), ms);
  };
}

/** 剩余时间估算：用「已耗时 / 已完成进度」外推；样本太少/进度太小时返回空。 */
function fmtEta(t) {
  const p = clamp01(t && t.progress);
  if (!t || p <= 0.02 || p >= 0.995) return '';
  const start = parseLocalDate(t.created_at);
  const upd = parseLocalDate(t.updated_at) || new Date();
  if (!start) return '';
  const elapsed = (upd.getTime() - start.getTime()) / 1000;
  if (!Number.isFinite(elapsed) || elapsed < 20) return '';
  const remain = (elapsed / p) * (1 - p);
  if (!Number.isFinite(remain) || remain <= 0) return '';
  if (remain < 60) return '约剩 <1 分钟';
  if (remain < 3600) return `约剩 ${Math.round(remain / 60)} 分钟`;
  return `约剩 ${(remain / 3600).toFixed(1)} 小时`;
}

/** 复制文本到剪贴板（兼容无 clipboard API / 权限被拒的 WebView）。 */
async function copyText(text) {
  const withTimeout = (p, ms) => Promise.race([
    p,
    new Promise((_, reject) => setTimeout(() => reject(new Error('clipboard timeout')), ms)),
  ]);
  try {
    await withTimeout(navigator.clipboard.writeText(text), 900);
    return true;
  } catch (_) {
    try {
      const ta = document.createElement('textarea');
      ta.value = text;
      ta.style.position = 'fixed';
      ta.style.top = '-1000px';
      ta.style.opacity = '0';
      document.body.appendChild(ta);
      ta.select();
      const ok = document.execCommand('copy');
      ta.remove();
      return ok;
    } catch (_) {
      return false;
    }
  }
}

/** 任务是否匹配当前队列搜索与筛选。 */
function taskMatches(t) {
  const f = state.ui.filter;
  if (f === 'active' && !isActive(t)) return false;
  if (f === 'done' && t.status !== 'done') return false;
  if (f === 'failed' && !(t.status === 'error' || t.status === 'canceled')) return false;
  const q = state.ui.search.trim().toLowerCase();
  if (!q) return true;
  const hay = [
    t.name, t.source_url, t.id,
    (t.media && t.media.path) || '',
    (t.options && t.options.target_lang) || '',
  ].join(' ').toLowerCase();
  return hay.includes(q);
}

/** 队列排序后的可见任务（倒序/名称/时长）。 */
function visibleTasks() {
  const arr = [...state.tasks.values()].filter(taskMatches);
  const sort = state.ui.sort;
  arr.sort((a, b) => {
    if (sort === 'name') return String(a.name || '').localeCompare(String(b.name || ''), 'zh');
    if (sort === 'duration') return ((b.media && b.media.duration) || 0) - ((a.media && a.media.duration) || 0);
    if (sort === 'updated') return String(b.updated_at || '').localeCompare(String(a.updated_at || ''));
    return String(b.created_at || '').localeCompare(String(a.created_at || ''));
  });
  return arr;
}

/* ---------------- 小工具 ---------------- */
function extOf(name) {
  const i = name.lastIndexOf('.');
  return i >= 0 ? name.slice(i + 1).toLowerCase() : '';
}

function clamp01(v) {
  const n = Number(v);
  if (!Number.isFinite(n)) return 0;
  return Math.max(0, Math.min(1, n));
}

function fmtBytes(n) {
  if (!Number.isFinite(n) || n <= 0) return '—';
  const units = ['B', 'KB', 'MB', 'GB', 'TB'];
  let i = 0;
  let v = n;
  while (v >= 1024 && i < units.length - 1) { v /= 1024; i++; }
  return `${v >= 100 || i === 0 ? Math.round(v) : v.toFixed(1)} ${units[i]}`;
}

function fmtDuration(sec) {
  if (!Number.isFinite(sec) || sec <= 0) return '—';
  const s = Math.round(sec);
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  const ss = s % 60;
  const mm = String(m).padStart(2, '0');
  const sss = String(ss).padStart(2, '0');
  return h ? `${h}:${mm}:${sss}` : `${m}:${sss}`;
}

/** 秒 → 时间码 "MM:SS.cc"（超 1 小时带小时位） */
function fmtTC(sec) {
  const total = Math.max(0, Math.round((Number(sec) || 0) * 100)); // 厘秒
  const cs = total % 100;
  const totalSec = Math.floor(total / 100);
  const h = Math.floor(totalSec / 3600);
  const m = Math.floor((totalSec % 3600) / 60);
  const s = totalSec % 60;
  const base = `${String(m).padStart(2, '0')}:${String(s).padStart(2, '0')}.${String(cs).padStart(2, '0')}`;
  return h ? `${h}:${base}` : base;
}

/** ISO "2026-09-28T15:30:00" → "09-28 15:30"（手动解析，避免时区歧义） */
function fmtDT(iso) {
  const m = /(\d{4})-(\d{2})-(\d{2})[T ](\d{2}):(\d{2})/.exec(String(iso || ''));
  return m ? `${m[2]}-${m[3]} ${m[4]}:${m[5]}` : '';
}

/** ISO "2026-09-28T15:30:00" → Date（按本地时区解析，避免 UTC 偏移） */
function parseLocalDate(iso) {
  const m = /(\d{4})-(\d{2})-(\d{2})[T ](\d{2}):(\d{2}):(\d{2})/.exec(String(iso || ''));
  return m ? new Date(+m[1], +m[2] - 1, +m[3], +m[4], +m[5], +m[6]) : null;
}

/** 相对时间："刚刚 / 3 分钟前 / 5 小时前 / 2 天前"；超过 30 天回退 "09-28 15:30" */
function relTime(iso) {
  const d = parseLocalDate(iso);
  if (!d) return '';
  const diff = Date.now() - d.getTime();
  if (diff < 45e3) return '刚刚';
  const min = Math.floor(diff / 6e4);
  if (min < 60) return `${min} 分钟前`;
  const h = Math.floor(min / 60);
  if (h < 24) return `${h} 小时前`;
  const day = Math.floor(h / 24);
  if (day < 30) return `${day} 天前`;
  return fmtDT(iso);
}

function isActive(t) { return !!t && ACTIVE_STATUSES.has(t.status); }
function isFinal(t) { return !!t && FINAL_STATUSES.has(t.status); }
function statusMeta(t) { return STATUS_META[t.status] || { label: t.status || '未知', cls: 'st-queued' }; }

/** 来源 chip 文案：精简域名 + 截断标题（任务名与 URL 相同即尚未拿到标题，则只显示域名） */
function sourceChipText(t) {
  let host = '';
  try { host = new URL(t.source_url).hostname.replace(/^www\./, ''); } catch (_) { /* 非 URL 时兜底 */ }
  const name = String(t.name || '').trim();
  if (name && name !== t.source_url) {
    const title = name.length > 18 ? `${name.slice(0, 18)}…` : name;
    return host ? `${host} · ${title}` : title;
  }
  return host || '来源链接';
}

/* ---------------- Toast ---------------- */
function toast(msg, type = 'info') {
  const box = $id('toastBox');
  while (box.children.length >= 5) box.firstElementChild.remove();

  const t = document.createElement('div');
  t.className = `toast toast-${type}`;

  const icon = document.createElement('span');
  icon.className = 'toast-icon';
  icon.textContent = type === 'success' ? '✓' : type === 'error' ? '✕' : 'ℹ';

  const body = document.createElement('div');
  body.className = 'toast-msg';
  body.textContent = String(msg);

  const close = document.createElement('button');
  close.type = 'button';
  close.className = 'toast-close';
  close.textContent = '×';
  close.setAttribute('aria-label', '关闭提示');

  t.append(icon, body, close);
  box.appendChild(t);

  const timer = setTimeout(dismiss, type === 'error' ? 6000 : 3500);
  function dismiss() {
    clearTimeout(timer);
    if (!t.isConnected) return;
    t.classList.add('toast-out');
    setTimeout(() => t.remove(), 260);
  }
  t.addEventListener('click', dismiss);
}

/* ---------------- 健康状态 ---------------- */
async function refreshHealth() {
  const dot = $id('healthDot');
  const text = $id('healthText');
  const wrap = $id('health');
  try {
    const h = await API.health();
    state.health = h;
    const gpuName = h && h.gpu && h.gpu.available ? h.gpu.name : null;
    const ffmpegOk = !!(h && h.ffmpeg);
    dot.className = 'h-dot ' + (ffmpegOk ? (gpuName ? 'ok' : 'warn') : 'warn');
    text.textContent = gpuName ? `GPU 就绪 · ${gpuName}` : (ffmpegOk ? '后端已连接' : '后端已连接（缺 ffmpeg）');
    wrap.title = `后端 v${(h && h.version) || '?'} · ffmpeg：${ffmpegOk ? '可用' : '未检测到'} · GPU：${gpuName || '不可用（CPU 模式）'}`;
  } catch (_) {
    state.health = null;
    dot.className = 'h-dot bad';
    text.textContent = '后端未连接';
    wrap.title = '无法连接后端';
  }
  refreshSetupBanner();
}

/** 顶部状态胶囊旁的当前默认 ASR 模型名；同时刷新配置缓存与就绪检查条（读盘，静默失败） */
async function refreshModelChip() {
  const el = $id('modelChip');
  try {
    const cfg = await API.getConfig(true);
    state.cfg = cfg || null;
    state.hasApiKey = !!(cfg && (cfg.has_api_key ?? (cfg.translate && cfg.translate.has_api_key)));
    state.mockMode = !!(cfg && cfg.translate && cfg.translate.mock);
    const m = cfg && cfg.asr && cfg.asr.model ? String(cfg.asr.model) : '';
    if (el) {
      if (m) {
        el.hidden = false;
        el.textContent = m;
        el.title = `当前默认 ASR 模型：${m}`;
      } else {
        el.hidden = true;
      }
    }
  } catch (_) { /* 静默：后端离线时由健康灯呈现 */ }
  try {
    const data = await API.listModels(true);
    state.modelsInfo = (data && data.models) || [];
  } catch (_) { /* 模型列表不可用时只跳过模型检查 */ }
  refreshSetupBanner();
}

/* ---------------- 首启就绪检查 ---------------- */
let setupTimer = null;

/** 根据健康状态 / 配置 / 模型状态，提示「还差什么才能开跑」。 */
function refreshSetupBanner() {
  const banner = $id('setupBanner');
  if (!banner) return;
  const items = [];
  const h = state.health;
  const cfg = state.cfg;

  if (h && !h.ffmpeg) {
    items.push({ text: '没有找到 ffmpeg，无法抽取音频（桌面版会自带，若被删除请重装或放回 tools/）' });
  }
  if (cfg) {
    if (!state.hasApiKey && !state.mockMode) {
      items.push({
        text: '还没配置翻译 API Key —— 识别完成后会停在翻译阶段',
        action: { label: '去配置', fn: () => openSettings() },
      });
      items.push({
        text: '或者先用 Mock 模式跑通全流程（伪翻译，不花钱）',
        action: { label: '启用 Mock', fn: enableMockMode },
      });
    }
    const want = (cfg.asr && cfg.asr.model) || '';
    const info = (state.modelsInfo || []).find((m) => m.name === want);
    if (info && info.status === 'idle') {
      items.push({
        text: `ASR 模型 ${want} 还没下载（${info.size_hint || '需联网下载'}），首次识别会自动下载`,
        action: { label: '现在下载', fn: () => downloadModelQuickly(want) },
      });
    } else if (info && info.status === 'downloading') {
      items.push({ text: `ASR 模型 ${want} 正在下载 ${Math.round(clamp01(info.progress) * 100)}%` });
      startSetupPolling();
    } else if (info && info.status === 'error') {
      items.push({
        text: `ASR 模型 ${want} 下载失败：${info.detail || '网络中断'}`,
        action: { label: '重试', fn: () => downloadModelQuickly(want) },
      });
    }
  }

  if (!items.length || state.ui.setupDismissed) {
    banner.hidden = true;
    stopSetupPolling();
    return;
  }

  const actionable = items.filter((i) => i.action).length;
  const title = $id('sbTitle');
  title.textContent = actionable > 0 && actionable === items.length
    ? `还有 ${actionable} 件事要做，做完就能开跑`
    : '运行前提示';

  const list = $id('sbList');
  list.textContent = '';
  for (const it of items) {
    const row = document.createElement('div');
    row.className = 'sb-item';
    const dot = document.createElement('i');
    dot.className = 'sb-dot';
    const span = document.createElement('span');
    span.textContent = it.text;
    row.append(dot, span);
    if (it.action) {
      const btn = document.createElement('button');
      btn.type = 'button';
      btn.className = 'btn btn-ghost btn-xs';
      btn.textContent = it.action.label;
      btn.addEventListener('click', it.action.fn);
      row.appendChild(btn);
    }
    list.appendChild(row);
  }

  const actions = $id('sbActions');
  actions.textContent = '';

  banner.hidden = false;
  if (items.some((i) => i.text.includes('正在下载'))) startSetupPolling();
  else stopSetupPolling();
}

async function enableMockMode() {
  try {
    await API.saveConfig({ translate: { mock: true } });
    state.mockMode = true;
    toast('已启用 Mock 模式：不开翻译 API 也能跑通全流程', 'success');
    await refreshModelChip();
  } catch (_) { /* toast 已由封装弹出 */ }
}

async function downloadModelQuickly(name) {
  try {
    await API.downloadModel(name);
    toast(`开始下载模型 ${name}，可在「设置 → ASR 模型下载」查看进度`, 'success');
    startSetupPolling();
    refreshModelChip();
  } catch (_) { /* toast 已由封装弹出 */ }
}

function startSetupPolling() {
  if (setupTimer) return;
  setupTimer = setInterval(() => {
    if ($id('setupBanner').hidden && !$id('modalOverlay').hidden === false) { /* 保持轮询直到无下载 */ }
    refreshModelChip();
  }, 2500);
}

function stopSetupPolling() {
  if (setupTimer) {
    clearInterval(setupTimer);
    setupTimer = null;
  }
}

/* ---------------- 任务列表：卡片 ---------------- */
let openMenuClose = null; // 当前展开的卡片菜单清理函数

function ensureCard(t) {
  let c = cardEls.get(t.id);
  if (c) return c;

  const root = document.createElement('div');
  root.className = 'task-card';
  root.dataset.id = t.id;
  root.setAttribute('role', 'listitem');
  root.tabIndex = 0;
  root.innerHTML = [
    '<div class="tc-row1">',
    '<input type="checkbox" class="tc-check" hidden aria-label="选择此任务">',
    '<span class="tc-name"></span>',
    '<span class="badge"><i class="dot"></i><span class="badge-text"></span></span>',
    '<button type="button" class="tc-more" title="更多操作" aria-label="更多操作">⋯</button>',
    '</div>',
    '<div class="tc-bar"><div class="bar"><div class="bar-fill"></div></div><span class="tc-pct mono"></span></div>',
    '<div class="tc-msg"></div>',
    '<div class="tc-meta mono"></div>',
  ].join('');

  const check = root.querySelector('.tc-check');
  const more = root.querySelector('.tc-more');
  c = {
    root,
    check,
    more,
    name: root.querySelector('.tc-name'),
    badge: root.querySelector('.badge'),
    badgeText: root.querySelector('.badge-text'),
    barWrap: root.querySelector('.tc-bar'),
    fill: root.querySelector('.bar-fill'),
    pct: root.querySelector('.tc-pct'),
    msg: root.querySelector('.tc-msg'),
    meta: root.querySelector('.tc-meta'),
  };
  root.addEventListener('click', (e) => {
    if (e.target.closest('.tc-check') || e.target.closest('.tc-more')) return;
    if (state.ui.batch) toggleBatchSelect(t.id);
    else selectTask(t.id);
  });
  root.addEventListener('keydown', (e) => {
    if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
      e.preventDefault();
      const ids = visibleTasks().map((x) => x.id);
      const i = ids.indexOf(t.id);
      if (i < 0) return;
      const ni = Math.max(0, Math.min(ids.length - 1, i + (e.key === 'ArrowDown' ? 1 : -1)));
      const next = cardEls.get(ids[ni]);
      if (next) {
        next.root.focus();
        selectTask(ids[ni]);
      }
      return;
    }
    if (e.key === 'Enter' || e.key === ' ') {
      e.preventDefault();
      if (state.ui.batch) toggleBatchSelect(t.id);
      else selectTask(t.id);
    }
  });
  check.addEventListener('change', () => toggleBatchSelect(t.id, check.checked));
  more.addEventListener('click', (e) => {
    e.stopPropagation();
    openCardMenu(t.id, more);
  });

  const retryBtn = document.createElement('button');
  retryBtn.type = 'button';
  retryBtn.className = 'btn btn-ghost btn-sm tc-retry';
  retryBtn.textContent = '重试';
  retryBtn.hidden = true;
  retryBtn.title = '从失败处继续（自动跳过已完成的阶段）';
  retryBtn.addEventListener('click', async (e) => {
    e.stopPropagation();
    retryBtn.disabled = true;
    try {
      await API.retryTask(t.id);
      toast('已重新入队，将跳过已完成的阶段', 'success');
      refreshSingleTask(t.id);
    } catch (_) { /* apiFetch 已提示 */ } finally {
      retryBtn.disabled = false;
    }
  });
  root.querySelector('.tc-row1').appendChild(retryBtn);
  c.retry = retryBtn;
  cardEls.set(t.id, c);
  $id('taskList').appendChild(root);
  return c;
}

/** 任务卡「⋯」菜单：打开文件夹 / 复制路径 / 复制链接 / 重试 / 删除 */
function openCardMenu(id, anchor) {
  closeCardMenu();
  const t = state.tasks.get(id);
  if (!t) return;

  const menu = document.createElement('div');
  menu.className = 'tc-menu';
  const add = (label, fn, cls = '') => {
    const b = document.createElement('button');
    b.type = 'button';
    b.textContent = label;
    if (cls) b.className = cls;
    b.addEventListener('click', (e) => {
      e.stopPropagation();
      closeCardMenu();
      fn();
    });
    menu.appendChild(b);
  };

  add('在文件夹中显示', async () => {
    try { await API.openFolder(id); } catch (_) { /* toast 已弹 */ }
  });
  add('复制产物路径', async () => {
    const p = t.output_dir || t.dir || '';
    if (!p) return;
    const ok = await copyText(p);
    toast(ok ? '已复制产物路径' : '复制失败', ok ? 'success' : 'error');
  });
  if (t.source_url) {
    add('复制源链接', async () => {
      const ok = await copyText(t.source_url);
      toast(ok ? '已复制源链接' : '复制失败', ok ? 'success' : 'error');
    });
  }
  if (t.status === 'error' || t.status === 'canceled') {
    add('重新入队', async () => {
      try {
        await API.retryTask(id);
        toast('任务已重新入队', 'success');
        refreshSingleTask(id);
      } catch (_) { /* toast 已弹 */ }
    });
  }
  if (isActive(t)) {
    add('取消任务', async () => {
      try {
        await API.cancelTask(id);
        toast('已发送取消请求', 'success');
        refreshSingleTask(id);
      } catch (_) { /* toast 已弹 */ }
    });
  }
  const sep = document.createElement('div');
  sep.className = 'tc-menu-sep';
  menu.appendChild(sep);
  add('删除任务与产物', async () => {
    const yes = await API.confirmBox(`确定删除任务「${t.name || t.id}」及其产物目录吗？`);
    if (!yes) return;
    try {
      await API.deleteTask(id);
      removeTaskLocal(id);
      toast('任务已删除', 'success');
    } catch (_) { /* toast 已弹 */ }
  }, 'is-danger');

  anchor.closest('.task-card').appendChild(menu);
  anchor.closest('.task-card').classList.add('menu-open');

  const onDoc = (e) => {
    if (!menu.contains(e.target)) closeCardMenu();
  };
  const onEsc = (e) => { if (e.key === 'Escape') closeCardMenu(); };
  setTimeout(() => document.addEventListener('click', onDoc), 0);
  document.addEventListener('keydown', onEsc);
  openMenuClose = () => {
    document.removeEventListener('click', onDoc);
    document.removeEventListener('keydown', onEsc);
  };
}

function closeCardMenu() {
  if (openMenuClose) openMenuClose();
  openMenuClose = null;
  document.querySelectorAll('.tc-menu').forEach((m) => m.remove());
  document.querySelectorAll('.task-card.menu-open').forEach((c) => c.classList.remove('menu-open'));
}

function updateCardDOM(t) {
  const c = ensureCard(t);
  const meta = statusMeta(t);
  const pct = t.status === 'done' ? 1 : clamp01(t.progress);

  c.name.textContent = t.name || t.id;
  c.name.title = (t.media && t.media.path) || t.name || t.id;
  c.badge.className = `badge ${meta.cls}`;
  c.badgeText.textContent = meta.label;

  c.fill.style.width = `${Math.round(pct * 100)}%`;
  c.fill.className = 'bar-fill' + (t.status === 'done' ? ' fill-done' : '') + (t.status === 'error' ? ' fill-error' : '') + (t.status === 'canceled' ? ' fill-canceled' : '');
  c.pct.textContent = `${Math.round(pct * 100)}%`;
  if (c.retry) c.retry.hidden = !(t.status === 'error' || t.status === 'canceled');

  c.msg.textContent = t.status === 'error' ? (t.error || '任务失败') : (t.message || meta.label);
  c.msg.classList.toggle('tc-msg-error', t.status === 'error');

  const bits = [];
  if (t.counts && t.counts.segments != null) bits.push(`${t.counts.segments} 段`);
  if (t.counts && t.counts.translated != null) bits.push(`已译 ${t.counts.translated}`);
  const eta = isActive(t) ? fmtEta(t) : '';
  if (eta) bits.push(eta);
  const dt = fmtDT(t.created_at);
  if (dt) bits.push(relTime(t.created_at)); // 卡片上显示相对时间
  c.meta.textContent = bits.join(' · ');
  c.meta.title = [dt ? `创建于 ${dt}` : '', eta ? `预计${eta.replace('约剩 ', '')}` : ''].filter(Boolean).join(' · ');

  c.root.classList.toggle('selected', t.id === state.selectedId);
  c.root.classList.toggle('batch-selected', state.ui.batch && state.ui.selected.has(t.id));
  c.check.hidden = !state.ui.batch;
  c.check.checked = state.ui.selected.has(t.id);
  c.root.hidden = !taskMatches(t);
  // 状态驱动的卡片着色（CSS: .tcst-*）
  c.root.classList.remove('tcst-done', 'tcst-error', 'tcst-canceled');
  if (t.status === 'done' || t.status === 'error' || t.status === 'canceled') {
    c.root.classList.add(`tcst-${t.status}`);
  }
}

/** 按当前排序重排可见卡片；仅当顺序变化时才移动节点，避免轮询打断 hover */
function sortTaskList() {
  const listEl = $id('taskList');
  // 筛选/搜索变化时同步每张卡的可见性（更新任务数据时也会走这里）
  for (const [id, c] of cardEls) {
    const t = state.tasks.get(id);
    c.root.hidden = !t || !taskMatches(t);
  }
  const desired = visibleTasks().map((t) => t.id).filter((id) => cardEls.has(id));
  const current = [...listEl.children]
    .filter((n) => !n.classList.contains('sk-card'))
    .map((n) => n.dataset.id);
  if (current.join('\u0001') !== desired.join('\u0001')) {
    for (const id of desired) listEl.appendChild(cardEls.get(id).root);
  }
  // 计数与空状态
  updateListChrome();
}

function updateListChrome() {
  const all = [...state.tasks.values()];
  const nAll = all.length;
  const nActive = all.filter(isActive).length;
  const nDone = all.filter((t) => t.status === 'done').length;
  const nFailed = all.filter((t) => t.status === 'error' || t.status === 'canceled').length;
  const visible = all.filter(taskMatches).length;

  $id('fAll').textContent = String(nAll);
  $id('fActive').textContent = String(nActive);
  $id('fDone').textContent = String(nDone);
  $id('fFailed').textContent = String(nFailed);
  $id('taskCount').textContent = String(nAll);

  // 批量条
  const batchBar = $id('batchBar');
  batchBar.hidden = !state.ui.batch;
  if (state.ui.batch) {
    $id('batchCount').textContent = `已选 ${state.ui.selected.size}`;
    const box = $id('batchAll');
    box.checked = visible > 0 && state.ui.selected.size === visible;
    box.indeterminate = state.ui.selected.size > 0 && state.ui.selected.size < visible;
    $id('btnBatchRetry').disabled = state.ui.selected.size === 0;
    $id('btnBatchDelete').disabled = state.ui.selected.size === 0;
    $id('btnBatchCancel').disabled = state.ui.selected.size === 0;
  }

  // 「加载更多」与总量
  const foot = $id('queueFoot');
  const more = state.ui.total > state.ui.limit && state.tasks.size >= state.ui.limit;
  foot.hidden = !more;
  $id('btnLoadMore').textContent = `加载更多（已显示 ${state.tasks.size} / ${state.ui.total || '?'}）`;
  $id('queueTotal').textContent = state.ui.search || state.ui.filter !== 'all'
    ? `筛选出 ${visible} 个`
    : '';

  // 空状态：只在首次加载完成后、且没有可见任务时出现
  const empty = $id('emptyState');
  const showEmpty = state.bootstrapped && visible === 0;
  empty.hidden = !showEmpty;
  if (showEmpty) {
    const filtering = nAll > 0;
    $id('esTitle').textContent = filtering ? '没有匹配的任务' : '队列为空';
    $id('esHint').innerHTML = filtering
      ? '换个关键词，或把筛选切回「全部」。'
      : '把音视频拖进上方区域，<br>让今晚的声音留下字幕';
    $id('esActions').hidden = filtering;
  }
}

function toggleBatchSelect(id, force) {
  const on = force === undefined ? !state.ui.selected.has(id) : !!force;
  if (on) state.ui.selected.add(id);
  else state.ui.selected.delete(id);
  const c = cardEls.get(id);
  if (c) {
    c.check.checked = on;
    c.root.classList.toggle('batch-selected', on);
  }
  updateListChrome();
}

function setBatchMode(on) {
  state.ui.batch = !!on;
  state.ui.selected.clear();
  $id('btnBatchMode').classList.toggle('is-on', state.ui.batch);
  $id('btnBatchMode').textContent = state.ui.batch ? '完成' : '批量';
  for (const c of cardEls.values()) {
    c.check.hidden = !state.ui.batch;
    c.check.checked = false;
    c.root.classList.remove('batch-selected');
  }
  updateListChrome();
}

function removeTaskLocal(id) {
  state.tasks.delete(id);
  state.ui.selected.delete(id);
  closeSSE(id);
  const c = cardEls.get(id);
  if (c) { c.root.remove(); cardEls.delete(id); }
  if (state.selectedId === id) {
    state.selectedId = null;
    state.segments = null;
    state.segmentsFor = null;
    $id('detailContent').hidden = true;
    $id('detailEmpty').hidden = false;
    document.body.classList.remove('detail-open');
  }
  savePrefs({ lastTaskId: null });
  updateListChrome();
}

/* ---------------- 骨架屏 ---------------- */
/** 任务列表骨架：仅在首次加载（bootstrapped 之前）展示 */
function showListSkeleton(n = 4) {
  const list = $id('taskList');
  const frag = document.createDocumentFragment();
  for (let i = 0; i < n; i++) {
    const d = document.createElement('div');
    d.className = 'task-card sk-card';
    d.setAttribute('aria-hidden', 'true');
    d.innerHTML = [
      '<div class="sk-row"><span class="sk-line" style="width:56%"></span><span class="sk-line sk-chip"></span></div>',
      '<span class="sk-line" style="width:88%"></span>',
      '<span class="sk-line" style="width:38%"></span>',
    ].join('');
    frag.appendChild(d);
  }
  list.appendChild(frag);
}

function clearListSkeleton() {
  const list = $id('taskList');
  for (const el of [...list.children]) {
    if (el.classList && el.classList.contains('sk-card')) el.remove();
  }
}

/** 字幕表骨架：详情字幕加载期间替代「字幕加载中…」文案 */
function showSegSkeleton(rows = 8) {
  const body = $id('segBody');
  body.textContent = '';
  $id('segEmpty').hidden = true;
  const frag = document.createDocumentFragment();
  for (let i = 0; i < rows; i++) {
    const tr = document.createElement('tr');
    tr.className = 'sk-row';
    tr.setAttribute('aria-hidden', 'true');
    tr.innerHTML =
      `<td class="col-tc"><span class="sk-line" style="width:92%"></span></td>` +
      `<td class="col-orig"><span class="sk-line" style="width:${58 + (i % 3) * 12}%"></span></td>` +
      `<td class="col-trans"><span class="sk-line" style="width:${44 + (i % 4) * 10}%"></span></td>`;
    frag.appendChild(tr);
  }
  body.appendChild(frag);
}

/* ---------------- 任务数据流 ---------------- */
function upsertTask(t) {
  if (!t || !t.id) return;
  state.tasks.set(t.id, t);
  updateCardDOM(t);
  sortTaskList();

  if (t.id === state.selectedId) {
    updateDetailHead(t);
    // 重试/重新入队后结果被清空：丢弃旧字幕
    if (!t.has_result && state.segmentsFor === t.id) {
      state.segments = null;
      state.segmentsFor = null;
      renderSegPlaceholder(t);
    }
    if (t.has_result && state.segmentsFor !== t.id && !state.segmentsLoading) {
      refreshSegments();
    }
  }

  if (isActive(t)) ensureSSE(t);
  else closeSSE(t.id);
}

async function loadTasks() {
  const limit = state.ui.limit;
  let data;
  try {
    data = await API.listTasks(limit);
  } catch (_) {
    // 静默：网络错误由健康灯反映；首次加载失败时撤掉骨架屏、交给空状态+健康灯说明
    if (!state.bootstrapped) {
      state.bootstrapped = true;
      clearListSkeleton();
      updateListChrome();
    }
    return;
  }
  const list = (data && data.tasks) || [];
  state.ui.total = Number.isFinite(data && data.total) ? data.total : list.length;
  if (!state.bootstrapped) {
    state.bootstrapped = true;
    clearListSkeleton();
  }
  for (const t of list) upsertTask(t);
  // 仅在「已取回全量」时清理本地缓存，避免误删尚未加载的第 N 页之外的任务
  const fetchedAll = list.length < limit || state.ui.total <= limit;
  if (fetchedAll) {
    const ids = new Set(list.map((t) => t.id));
    for (const id of [...state.tasks.keys()]) {
      if (!ids.has(id)) removeTaskLocal(id);
    }
  }
  sweepSSE();
  updateListChrome();
}

async function refreshSingleTask(id, withSegments = false) {
  try {
    const t = await API.getTask(id, withSegments);
    upsertTask(t);
    updateListChrome();
  } catch (_) { /* 静默 */ }
}

/* ---------------- SSE ---------------- */
function ensureSSE(t) {
  if (API.isDesktop()) return; // 桌面模式：全局 task-events 事件（见 init）
  if (sse.has(t.id)) return;
  if (sse.size >= SSE_CAP) return; // 超出并发上限的任务依赖 3s 轮询
  let es;
  try {
    es = new EventSource(`/api/tasks/${encodeURIComponent(t.id)}/events`);
  } catch (_) {
    return;
  }
  sse.set(t.id, es);
  es.onmessage = (ev) => {
    let data = null;
    try { data = JSON.parse(ev.data); } catch (_) { return; }
    if (!data || data.type === 'ping') return; // 15s 心跳
    if (data.task) upsertTask(data.task);
    if (data.type === 'done' || data.type === 'error') closeSSE(t.id);
  };
  es.onerror = () => {
    // 断线时 EventSource 自带重连；仅在被判死（CLOSED）时移除，等待轮询重建
    if (es.readyState === EventSource.CLOSED) sse.delete(t.id);
  };
}

function closeSSE(id) {
  const es = sse.get(id);
  if (es) {
    es.close();
    sse.delete(id);
  }
}

function sweepSSE() {
  for (const id of [...sse.keys()]) {
    const t = state.tasks.get(id);
    if (!t || !isActive(t)) closeSSE(id);
  }
}

/* ---------------- 详情面板 ---------------- */
function selectTask(id) {
  if (!state.tasks.has(id)) return;
  document.body.classList.add('detail-open'); // 窄屏下打开覆盖层
  if (state.selectedId === id) return;
  flushDirtyEdit();       // 切换前保存未提交的译文
  closeCardMenu();
  state.selectedId = id;
  state.segments = null;
  state.segmentsFor = null;
  state.segPage = 1;
  state.ui.activeSeg = null;
  state.ui.segQuery = '';
  state.ui.segMatches = [];
  state.ui.segMatchPos = -1;
  if ($id('segSearch')) $id('segSearch').value = '';
  savePrefs({ lastTaskId: id });

  for (const [tid, c] of cardEls) c.root.classList.toggle('selected', tid === id);

  $id('detailEmpty').hidden = true;
  $id('detailContent').hidden = false;

  const t = state.tasks.get(id);
  buildMedia(t);
  renderSegPlaceholder(t);
  if (t) {
    updateDetailHead(t);
    if (t.has_result) refreshSegments();
  }
  refreshSingleTask(id); // 拉最新元数据
}

function buildMedia(t) {
  const slot = $id('mediaSlot');
  slot.textContent = '';
  state.mediaEl = null;
  if (!t) return;

  // 无媒体文件的任务（下载失败/纯历史记录）：占位提示而非挂一个必报错的播放器
  if (!t.media || !t.media.path) {
    const tip = document.createElement('div');
    tip.className = 'media-tip';
    tip.textContent = t.source_url
      ? '该任务没有媒体文件（下载未完成或已失败）——可点「重试」重新下载'
      : '该任务没有关联的媒体文件';
    slot.appendChild(tip);
    return;
  }

  const src = API.mediaUrl(t.id);
  const hasVideo = !!(t.media && t.media.has_video);
  const m = document.createElement(hasVideo ? 'video' : 'audio');
  m.controls = true;
  m.preload = 'metadata';
  m.src = src;

  const track = document.createElement('track');
  track.kind = 'subtitles';
  track.label = '双语字幕';
  track.srclang = 'zh';
  track.default = true;
  track.src = API.vttUrl(t.id);
  m.appendChild(track);

  m.addEventListener('error', () => {
    const tip = document.createElement('div');
    tip.className = 'media-tip';
    tip.style.color = 'var(--red)';
    tip.textContent = '媒体文件加载失败（可能已被移动或删除）';
    slot.appendChild(tip);
  });

  // 播放时高亮当前字幕行，并让表格自动跟随
  m.addEventListener('timeupdate', () => onMediaTime(m.currentTime));
  m.addEventListener('seeked', () => onMediaTime(m.currentTime));

  state.mediaEl = m;
  slot.appendChild(m);
}

/** 播放位置 → 高亮当前句（仅当存在字幕且不在编辑时） */
function onMediaTime(sec) {
  if (!state.mediaEl || !state.segments || state.segmentsFor !== state.selectedId) return;
  const idx = (state.segments || []).findIndex((s) => sec >= (s.start || 0) && sec < (s.end || 0));
  if (idx < 0) return;
  const seg = state.segments[idx];
  if (!seg || seg.index === state.ui.activeSeg) return;
  const page = Math.floor(idx / PAGE_SIZE) + 1;
  if (page !== state.segPage) {
    state.segPage = page;
    renderSegments();
  }
  setActiveSeg(seg.index, { scroll: true, follow: true });
}

/** 点击时间码 / 快捷键：跳到指定秒；没有播放器时忽略 */
function seekTo(sec) {
  const m = state.mediaEl;
  if (!m || !Number.isFinite(sec)) return false;
  try {
    m.currentTime = Math.max(0, sec);
    return true;
  } catch (_) {
    return false;
  }
}

function updateDetailHead(t) {
  const meta = statusMeta(t);
  const pct = t.status === 'done' ? 1 : clamp01(t.progress);

  $id('detailName').textContent = t.name || t.id;
  $id('detailName').title = (t.media && t.media.path) || t.name || t.id;

  const badge = $id('detailBadge');
  badge.className = `badge ${meta.cls}`;
  $id('detailBadgeText').textContent = meta.label;

  const fill = $id('detailBarFill');
  fill.style.width = `${Math.round(pct * 100)}%`;
  fill.className = 'bar-fill' + (t.status === 'done' ? ' fill-done' : '') + (t.status === 'error' ? ' fill-error' : '') + (t.status === 'canceled' ? ' fill-canceled' : '');
  $id('detailPct').textContent = `${Math.round(pct * 100)}%`;

  const msg = $id('detailMessage');
  msg.textContent = t.status === 'error' ? (t.error || '任务失败') : (t.message || '');
  msg.classList.toggle('dh-msg-error', t.status === 'error');

  const errBox = $id('detailError');
  if (t.status === 'error' && t.error) {
    errBox.hidden = false;
    errBox.textContent = t.error;
  } else {
    errBox.hidden = true;
  }

  const chips = $id('detailChips');
  chips.textContent = '';
  const addChip = (label, value) => {
    const chip = document.createElement('span');
    chip.className = 'chip';
    const b = document.createElement('b');
    b.textContent = label;
    const v = document.createElement('span');
    v.textContent = String(value);
    chip.append(b, v);
    chips.appendChild(chip);
  };
  if (t.source_url) {
    const chip = document.createElement('span');
    chip.className = 'chip';
    const b = document.createElement('b');
    b.textContent = '来源';
    const a = document.createElement('a');
    a.href = t.source_url;
    a.target = '_blank';
    a.rel = 'noopener';
    a.title = t.source_url;
    a.textContent = sourceChipText(t);
    chip.append(b, a);
    chips.appendChild(chip);
  }
  if (t.counts && t.counts.segments != null) addChip('段数', t.counts.segments);
  if (t.counts && t.counts.translated != null) addChip('已翻译', t.counts.translated);
  if (t.media && t.media.duration) addChip('时长', fmtDuration(t.media.duration));
  if (t.media && t.media.size) addChip('大小', fmtBytes(t.media.size));
  if (t.asr && t.asr.model) addChip('ASR', t.asr.model);
  if (t.options) addChip('语言', `${t.options.source_lang || 'auto'} → ${t.options.target_lang || 'zh'}`);
  const dt = fmtDT(t.created_at);
  if (dt) addChip('创建', dt);

  const filesRow = $id('filesRow');
  const showDir = t.output_dir || t.dir;   // 完成后优先显示 output/{视频名}/
  if (showDir) {
    filesRow.hidden = false;
    const dp = $id('taskDirPath');
    dp.textContent = showDir;
    dp.title = showDir;
    const mp = t.media && t.media.path;
    const mfr = $id('mediaFileRow');
    if (t.source_url && mp) {
      mfr.hidden = false;
      const mc = $id('mediaFilePath');
      mc.textContent = mp;
      mc.title = mp;
    } else {
      mfr.hidden = true;
    }
  } else {
    filesRow.hidden = true;
    $id('mediaFileRow').hidden = true;
  }

  $id('btnCancel').hidden = !isActive(t);
  $id('btnRetry').hidden = !(t.status === 'error' || t.status === 'canceled');

  updateExports(t);
}

/* 成品视频渲染按钮文案（含进度） */
function burnLabel(type) {
  const base = type === 'burn_mp4' ? '成品视频（硬字幕）' : '软字幕 MKV';
  const id = state.selectedId;
  const b = id && burnState.get(id);
  if (b && b.phase === 'running') return `${base} ${Math.round((b.progress || 0) * 100)}%`;
  return base;
}

function updateExports(t) {
  const ready = !!t.has_result;
  $id('btnCopySub').disabled = !ready;
  $id('btnExportAll').hidden = !API.isDesktop();
  $id('btnExportAll').disabled = !ready;
  const links = [...$id('exportRow').querySelectorAll('a')];
  if (API.isDesktop()) {
    // 桌面模式：点击 → 后端弹保存对话框并写文件
    for (const a of links) {
      const isBurn = a.dataset.type.startsWith('burn_');
      a.classList.toggle('disabled', !ready);
      if (!ready) { a.removeAttribute('href'); continue; }
      a.href = '#';
      if (isBurn) {
        // 需要 a) 已有视频 b) 该任务未在渲染中（渲染中点击 = 取消）
        const b = burnState.get(t.id);
        const busy = b && b.phase === 'running';
        a.textContent = burnLabel(a.dataset.type);
        a.classList.toggle('is-busy', !!busy);
        a.title = busy
          ? '正在渲染，点击可取消'
          : (a.dataset.type === 'burn_mkv'
            ? '字幕作为独立轨道封装进 MKV（秒级）'
            : '将双语 ASS 字幕烧录进画面（N 卡硬件加速）');
        a.onclick = async (e) => {
          e.preventDefault();
          if (burnState.get(t.id)?.phase === 'running') {
            try {
              await API.cancelBurn(t.id);
              toast('已请求取消渲染', 'info');
            } catch (_) { /* 忽略 */ }
            return;
          }
          if (a.dataset.busy === '1') return;
          a.dataset.busy = '1';
          try {
            const res = await API.exportBurned(t.id, a.dataset.type === 'burn_mkv' ? 'mkv' : 'mp4');
            if (res && res.started) {
              burnState.set(t.id, { phase: 'running', progress: 0 });
              toast(res.path === null ? '已取消' : `开始渲染：${res.path}`, res.path === null ? 'info' : 'success');
              updateExports(state.tasks.get(t.id) || t);
            }
          } catch (_) { /* toast 已由封装弹出 */ } finally {
            delete a.dataset.busy;
          }
        };
        continue;
      }
      a.textContent = OUTPUT_FILES[a.dataset.type] ? a.dataset.origText || (a.dataset.origText = a.textContent) : a.textContent;
      a.onclick = async (e) => {
        e.preventDefault();
        if (a.dataset.busy === '1') return;
        a.dataset.busy = '1';
        try {
          const fname = OUTPUT_FILES[a.dataset.type] || 'subtitle.srt';
          const suggested = `${(t.name || '字幕').replace(/\.[^.]+$/, '')}.${fname}`;
          const res = await API.exportOutput(t.id, a.dataset.type, suggested);
          if (res && res.path) toast(`已导出：${res.path}`, 'success');
          else if (res && res.path === null) { /* 用户取消 */ }
        } catch (_) { /* toast 已由封装弹出 */ } finally {
          delete a.dataset.busy;
        }
      };
    }
    return;
  }
  for (const a of links) {
    if (a.dataset.type.startsWith('burn_')) { a.hidden = true; continue; } // 浏览器模式无此功能
    if (ready) {
      a.href = `/api/tasks/${encodeURIComponent(t.id)}/download?type=${a.dataset.type}`;
      a.classList.remove('disabled');
    } else {
      a.removeAttribute('href');
      a.classList.add('disabled');
    }
  }
}

/* 导出文件名（type → 文件名，桌面模式保存对话框用） */
const OUTPUT_FILES = {
  orig_srt: 'orig.srt',
  trans_srt: 'trans.srt',
  bilingual_srt: 'bilingual.srt',
  bilingual_ass: 'bilingual.ass',
  bilingual_vtt: 'bilingual.vtt',
  json: 'segments.json',
};

/* ---------------- 字幕编辑表 ---------------- */
async function refreshSegments(force = false) {
  const id = state.selectedId;
  if (!id) return;
  const t = state.tasks.get(id);
  if (!t) return;
  if (!t.has_result) { renderSegPlaceholder(t); return; }
  if (state.segmentsLoading || (!force && state.segmentsFor === id)) return;

  state.segmentsLoading = true;
  showSegSkeleton(); // 加载期间展示字幕表骨架
  const empty = $id('segEmpty');
  try {
    const full = await API.getTask(id, true);
    if (state.selectedId !== id) return; // 期间用户已切换任务
    state.segments = Array.isArray(full.segments) ? full.segments : [];
    state.segmentsFor = id;
    state.segPage = 1;
    state.ui.activeSeg = null;
    computeSegMatches();
    renderSegments();
  } catch (_) {
    if (state.selectedId === id) {
      $id('segBody').textContent = '';
      empty.hidden = false;
      empty.textContent = '字幕加载失败，可点击「刷新」重试';
    }
  } finally {
    state.segmentsLoading = false;
  }
}

function renderSegPlaceholder(t) {
  $id('segBody').textContent = '';
  $id('segSummary').textContent = '';
  $id('pageInfo').textContent = '— / —';
  $id('segSearchInfo').hidden = true;
  for (const b of ['btnPageFirst', 'btnPagePrev', 'btnPageNext', 'btnPageLast', 'btnRetransPage', 'btnShiftApply']) {
    if ($id(b)) $id(b).disabled = true;
  }
  const box = $id('segEmpty');
  box.hidden = false;
  if (!t) { box.textContent = '暂无任务'; return; }
  if (t.status === 'error') box.textContent = '任务失败，没有可用的字幕。';
  else if (t.status === 'canceled') box.textContent = '任务已取消，没有生成字幕。';
  else if (isActive(t)) box.textContent = `${metaLabel(t)}进行中 — 完成后即可在此编辑、重翻与导出字幕。`;
  else box.textContent = '暂无字幕数据，点击「刷新」重试。';
}

function metaLabel(t) { return STATUS_META[t.status] ? STATUS_META[t.status].label : (t.status || ''); }

/* ---- 字幕搜索 ---- */
function computeSegMatches() {
  const q = state.ui.segQuery.trim().toLowerCase();
  state.ui.segMatches = [];
  state.ui.segMatchPos = -1;
  if (q && state.segments) {
    state.segments.forEach((s, i) => {
      const hay = `${s.text || ''}\n${s.translated || ''}`.toLowerCase();
      if (hay.includes(q)) state.ui.segMatches.push(i);
    });
    if (state.ui.segMatches.length) state.ui.segMatchPos = 0;
  }
  updateSegSearchInfo();
}

function updateSegSearchInfo() {
  const info = $id('segSearchInfo');
  const prev = $id('btnSegSearchPrev');
  const next = $id('btnSegSearchNext');
  const n = state.ui.segMatches.length;
  if (!state.ui.segQuery.trim()) {
    info.hidden = true;
    prev.hidden = true;
    next.hidden = true;
    return;
  }
  info.hidden = false;
  prev.hidden = n === 0;
  next.hidden = n === 0;
  info.textContent = n ? `${state.ui.segMatchPos + 1} / ${n}` : '无匹配';
  prev.disabled = n === 0;
  next.disabled = n === 0;
}

/** 跳到第 pos 个搜索命中（自动翻页并高亮） */
function gotoMatch(pos) {
  const matches = state.ui.segMatches;
  if (!matches.length) return;
  const p = ((pos % matches.length) + matches.length) % matches.length;
  state.ui.segMatchPos = p;
  const i = matches[p];
  const page = Math.floor(i / PAGE_SIZE) + 1;
  if (page !== state.segPage) {
    state.segPage = page;
    renderSegments();
  }
  const seg = (state.segments || [])[i];
  if (seg) setActiveSeg(seg.index, { scroll: true });
  updateSegSearchInfo();
}

/* ---- 行渲染 ---- */
function renderSegments() {
  const segs = state.segments || [];
  const total = segs.length;
  const pages = Math.max(1, Math.ceil(total / PAGE_SIZE));
  if (state.segPage > pages) state.segPage = pages;
  if (state.segPage < 1) state.segPage = 1;

  const q = state.ui.segQuery.trim();
  const hitsBefore = state.ui.segMatches.length;
  $id('segSummary').textContent = total ? `共 ${total} 段 · 每页 ${PAGE_SIZE}${q ? ` · 命中 ${hitsBefore}` : ''}` : '';
  $id('pageInfo').textContent = `${state.segPage} / ${pages}`;
  $id('btnPageFirst').disabled = $id('btnPagePrev').disabled = state.segPage <= 1;
  $id('btnPageNext').disabled = $id('btnPageLast').disabled = state.segPage >= pages;
  $id('btnRetransPage').disabled = !total;
  $id('btnShiftApply').disabled = !total;
  updateSegSearchInfo();

  const body = $id('segBody');
  const empty = $id('segEmpty');
  body.textContent = '';
  if (!total) {
    empty.hidden = false;
    empty.textContent = '任务已完成，但没有解析出字幕段。';
    return;
  }
  empty.hidden = true;

  const start = (state.segPage - 1) * PAGE_SIZE;
  const frag = document.createDocumentFragment();
  for (const s of segs.slice(start, start + PAGE_SIZE)) frag.appendChild(buildSegRow(s));
  body.appendChild(frag);

  // 搜索命中行加 glow；命中不在当前页时给出提示
  if (q) {
    for (const tr of body.querySelectorAll('tr')) {
      if (state.ui.segMatches.includes(Number(tr.dataset.index))) tr.classList.add('seg-hit');
    }
  }
}

function markText(el, text, q) {
  el.textContent = '';
  const str = String(text || '');
  if (!q) { el.textContent = str; return; }
  const lower = str.toLowerCase();
  const needle = q.toLowerCase();
  let pos = 0;
  while (pos < str.length) {
    const at = lower.indexOf(needle, pos);
    if (at < 0) break;
    if (at > pos) el.appendChild(document.createTextNode(str.slice(pos, at)));
    const mark = document.createElement('mark');
    mark.className = 'seg-mark';
    mark.textContent = str.slice(at, at + needle.length);
    el.appendChild(mark);
    pos = at + needle.length;
  }
  if (pos < str.length) el.appendChild(document.createTextNode(str.slice(pos)));
}

function buildSegRow(s) {
  const tr = document.createElement('tr');
  tr.className = 'seg-row'
    + (s.flagged ? ' flagged' : '')
    + (s.index === state.ui.activeSeg ? ' active' : '');
  tr.dataset.index = String(s.index);

  // 时间码（点击跳到播放位置）
  const tdTc = document.createElement('td');
  tdTc.className = 'col-tc mono' + (state.mediaEl ? '' : ' disabled');
  const t1 = document.createElement('span');
  t1.textContent = fmtTC(s.start);
  const arrow = document.createElement('span');
  arrow.className = 'tc-arrow';
  arrow.textContent = '→';
  const t2 = document.createElement('span');
  t2.textContent = fmtTC(s.end);
  tdTc.append(t1, arrow, t2);
  tdTc.title = state.mediaEl ? `点击跳到 ${fmtTC(s.start)}` : '没有可播放的媒体文件';
  if (state.mediaEl) {
    tdTc.addEventListener('click', () => {
      seekTo(s.start);
      setActiveSeg(s.index);
    });
  }

  // 原文（flagged 加 ⚠；双击可就地修正 ASR 错误）
  const tdOrig = document.createElement('td');
  tdOrig.className = 'col-orig';
  if (s.flagged) {
    const warn = document.createElement('span');
    warn.className = 'flag-mark';
    warn.textContent = '⚠';
    warn.title = '疑似 ASR 幻觉';
    tdOrig.appendChild(warn);
  }
  const origText = document.createElement('span');
  origText.className = 'orig-text';
  markText(origText, s.text || '', state.ui.segQuery.trim());
  origText.title = API.isDesktop() ? '双击可修正识别文本' : '';
  if (API.isDesktop()) origText.addEventListener('dblclick', () => startOrigEdit(origText, s));
  tdOrig.appendChild(origText);

  // 译文（可编辑）+ 重翻
  const tdTrans = document.createElement('td');
  tdTrans.className = 'col-trans';
  const wrap = document.createElement('div');
  wrap.className = 'trans-wrap';

  const input = document.createElement('input');
  input.type = 'text';
  input.className = 'trans-input';
  input.value = s.translated || '';
  input.dataset.orig = s.translated || '';
  input.spellcheck = false;
  input.placeholder = '（空译文，可直接填写）';

  const btn = document.createElement('button');
  btn.type = 'button';
  btn.className = 'btn btn-ghost btn-xs btn-retrans';
  btn.textContent = '重翻';
  btn.title = '调用翻译 API，结合上下文重翻此句';

  wrap.append(input, btn);
  tdTrans.appendChild(wrap);

  // 操作：合并 / 拆分 / 复制
  const tdOps = document.createElement('td');
  tdOps.className = 'col-ops';
  const ops = document.createElement('div');
  ops.className = 'seg-ops';
  const mkOp = (label, title, op, disabled) => {
    const b = document.createElement('button');
    b.type = 'button';
    b.className = 'seg-op';
    b.dataset.op = op;
    b.textContent = label;
    b.title = title;
    b.disabled = !!disabled;
    return b;
  };
  const isLast = s.index >= (state.segments || []).length - 1;
  if (API.isDesktop()) {
    ops.append(
      mkOp('⇩', '与下一句合并', 'merge', isLast),
      mkOp('✂', '在此拆分（默认播放位置或中点）', 'split', (s.end - s.start) < 0.15),
      mkOp('⧉', '复制这句（原文 + 译文）', 'copy', false),
    );
  } else {
    ops.appendChild(mkOp('⧉', '复制这句（原文 + 译文）', 'copy', false));
  }
  tdOps.appendChild(ops);

  tr.append(tdTc, tdOrig, tdTrans, tdOps);
  return tr;
}

/** 原文就地编辑：双击进入，回车保存、Esc 取消 */
function startOrigEdit(el, s) {
  if (el.getAttribute('contenteditable') === 'true') return;
  const id = state.selectedId;
  if (!id) return;
  const before = s.text || '';
  el.textContent = before;
  el.setAttribute('contenteditable', 'true');
  el.focus();
  const sel = window.getSelection();
  if (sel && document.createRange) {
    const range = document.createRange();
    range.selectNodeContents(el);
    sel.removeAllRanges();
    sel.addRange(range);
  }
  let done = false;
  const finish = async (save) => {
    if (done) return;
    done = true;
    el.removeAttribute('contenteditable');
    const next = (el.textContent || '').replace(/\s+/g, ' ').trim();
    if (!save || next === before.trim()) {
      el.textContent = before;
      markText(el, before, state.ui.segQuery.trim());
      return;
    }
    try {
      const res = await API.patchSegmentFields(id, s.index, { text: next });
      const segs = (res && res.segments) || null;
      if (segs) applySegmentsFromServer(segs);
      else { s.text = next; markText(el, next, state.ui.segQuery.trim()); }
      toast('原文已更新；如需同步译文可点该行「重翻」', 'success');
    } catch (_) {
      el.textContent = before;
      markText(el, before, state.ui.segQuery.trim());
    }
  };
  el.addEventListener('keydown', (e) => {
    if (e.key === 'Enter') { e.preventDefault(); el.blur(); }
    else if (e.key === 'Escape') { e.preventDefault(); el.textContent = before; el.blur(); }
  }, { once: true });
  el.addEventListener('blur', () => {
    const changed = (el.textContent || '').trim() !== before.trim();
    finish(changed);
  }, { once: true });
}

function setActiveSeg(index, opts = {}) {
  const segs = state.segments || [];
  const i = segs.findIndex((s) => s.index === index);
  if (i < 0) return;
  state.ui.activeSeg = index;
  for (const tr of $id('segBody').querySelectorAll('tr')) {
    tr.classList.toggle('active', Number(tr.dataset.index) === index);
  }
  const tr = $id('segBody').querySelector(`tr[data-index="${index}"]`);
  if (!tr) return;
  if (opts.scroll) tr.scrollIntoView({ block: 'nearest', behavior: opts.follow ? 'smooth' : 'auto' });
  if (opts.focusInput) {
    const input = tr.querySelector('.trans-input');
    if (input) { input.focus(); input.select(); }
  }
}

/** 上下移动当前句（自动翻页） */
function moveActive(delta, opts = {}) {
  const segs = state.segments || [];
  if (!segs.length) return;
  let i = segs.findIndex((s) => s.index === state.ui.activeSeg);
  if (i < 0) i = (state.segPage - 1) * PAGE_SIZE;
  i = Math.max(0, Math.min(segs.length - 1, i + delta));
  const page = Math.floor(i / PAGE_SIZE) + 1;
  if (page !== state.segPage) {
    flushDirtyEdit();
    state.segPage = page;
    renderSegments();
  }
  setActiveSeg(segs[i].index, { scroll: true, ...opts });
}

/** 服务端返回全量 segments 后统一落地：本地缓存 + 搜索 + 重新渲染 */
function applySegmentsFromServer(segments) {
  if (!Array.isArray(segments)) return;
  state.segments = segments;
  state.segmentsFor = state.selectedId;
  computeSegMatches();
  const pages = Math.max(1, Math.ceil(segments.length / PAGE_SIZE));
  if (state.segPage > pages) state.segPage = pages;
  renderSegments();
  const t = state.tasks.get(state.selectedId);
  if (t) updateDetailHead(t);
}

function findSeg(index) {
  return (state.segments || []).find((s) => s.index === index) || null;
}

/** 失焦 / 回车时保存被修改的译文 */
async function maybeSave(input) {
  if (!input || !input.isConnected) return;
  if (input.dataset.saving === '1') return;
  const orig = input.dataset.orig ?? '';
  if (input.value === orig) { input.classList.remove('dirty'); return; }

  const id = state.selectedId;
  if (!id) return;
  const tr = input.closest('tr');
  const index = Number(tr && tr.dataset.index);
  if (!Number.isInteger(index)) return;

  input.dataset.saving = '1';
  input.classList.remove('dirty');
  input.classList.add('saving');
  const val = input.value;
  try {
    await API.patchSegment(id, index, val);
    input.dataset.orig = val;
    input.classList.add('saved');
    setTimeout(() => input.classList.remove('saved'), 1300);
    const seg = findSeg(index);
    if (seg) seg.translated = val;
  } catch (_) {
    input.value = orig; // 失败还原（toast 已由封装弹出）
  } finally {
    delete input.dataset.saving;
    input.classList.remove('saving');
  }
}

async function retranslateRow(btn) {
  const tr = btn.closest('tr');
  const id = state.selectedId;
  if (!tr || !id) return;
  const index = Number(tr.dataset.index);
  if (!Number.isInteger(index)) return;

  // 若该行有未保存的修改，先保存再重翻，避免互相覆盖
  const input = tr.querySelector('.trans-input');
  if (input && input.value !== (input.dataset.orig ?? '')) await maybeSave(input);

  btn.disabled = true;
  const oldText = btn.textContent;
  btn.textContent = '重翻中…';
  tr.classList.add('retranslating');
  try {
    const seg = await API.retranslateSegment(id, index);
    if (seg && Number.isInteger(seg.index)) {
      const old = findSeg(seg.index);
      if (old) Object.assign(old, seg);
      const cur = $id('segBody').querySelector(`tr[data-index="${seg.index}"]`);
      if (cur) cur.replaceWith(buildSegRow(seg));
    }
    toast(`第 ${index + 1} 句重翻完成`, 'success');
  } catch (_) {
    // toast 已由封装弹出
  } finally {
    btn.disabled = false;
    btn.textContent = oldText;
    tr.classList.remove('retranslating');
  }
}

/** 行操作：合并 / 拆分 / 复制 */
async function handleSegOp(btn) {
  const tr = btn.closest('tr');
  const id = state.selectedId;
  if (!tr || !id) return;
  const index = Number(tr.dataset.index);
  const seg = findSeg(index);
  if (!seg) return;
  const op = btn.dataset.op;

  if (op === 'copy') {
    const text = [seg.text || '', seg.translated || ''].filter((x) => x.trim()).join('\n');
    const ok = await copyText(text);
    toast(ok ? '已复制这句字幕' : '复制失败', ok ? 'success' : 'error');
    return;
  }

  if (op === 'merge') {
    if (index >= (state.segments || []).length - 1) return;
    btn.disabled = true;
    try {
      const res = await API.mergeSegments(id, index);
      if (res && res.segments) {
        applySegmentsFromServer(res.segments);
        setActiveSeg(index, { scroll: true });
        toast('已与下一句合并', 'success');
      }
    } catch (_) { /* toast 已弹 */ } finally {
      btn.disabled = false;
    }
    return;
  }

  if (op === 'split') {
    const current = state.mediaEl ? state.mediaEl.currentTime : NaN;
    const inRange = Number.isFinite(current) && current > seg.start + 0.05 && current < seg.end - 0.05;
    const def = inRange ? current : (seg.start + seg.end) / 2;
    const raw = await askValue({
      title: '拆分字幕',
      label: `在第 ${index + 1} 句（${fmtTC(seg.start)} → ${fmtTC(seg.end)}）的哪个时间点拆分？单位：秒`,
      value: def.toFixed(2),
      note: inRange ? '已默认填入播放器当前时间——直接确定即可' : '默认取本句中点；也可先播放到想拆的位置再点 ✂',
      validate: (v) => {
        const n = Number(v);
        if (!Number.isFinite(n)) return '请输入数字';
        if (!(n > seg.start && n < seg.end - 0.05)) return `需满足 ${seg.start.toFixed(2)} < 时间 < ${(seg.end - 0.05).toFixed(2)}`;
        return '';
      },
    });
    if (raw === null) return;
    btn.disabled = true;
    try {
      const res = await API.splitSegment(id, index, Number(raw));
      if (res && res.segments) {
        applySegmentsFromServer(res.segments);
        setActiveSeg(index, { scroll: true });
        toast('已拆分', 'success');
      }
    } catch (_) { /* toast 已弹 */ } finally {
      btn.disabled = false;
    }
  }
}

/** 时间轴整体平移（桌面版） */
async function shiftTimeline() {
  const id = state.selectedId;
  if (!id) return;
  const raw = $id('shiftInput').value.trim();
  const offset = Number(raw);
  if (!raw || !Number.isFinite(offset) || offset === 0) {
    toast('请输入非 0 的秒数（负数为提前，正数为延后）', 'error');
    return;
  }
  const btn = $id('btnShiftApply');
  btn.disabled = true;
  try {
    const res = await API.shiftSegments(id, offset);
    if (res && res.segments) {
      applySegmentsFromServer(res.segments);
      $id('shiftInput').value = '';
      const sign = offset > 0 ? '+' : '';
      toast(`所有时间码已平移 ${sign}${offset}s`, 'success');
    }
  } catch (_) { /* toast 已弹 */ } finally {
    btn.disabled = false;
  }
}

/** 重翻当前页（桌面版批量，一次写盘） */
async function retranslatePage() {
  const id = state.selectedId;
  const segs = state.segments || [];
  if (!id || !segs.length) return;
  flushDirtyEdit();
  const start = (state.segPage - 1) * PAGE_SIZE;
  const pageSegs = segs.slice(start, start + PAGE_SIZE);
  const indices = pageSegs.map((s) => s.index);
  const yes = await API.confirmBox(`重新翻译第 ${state.segPage} 页共 ${indices.length} 句？\n（会用当前翻译 API 覆盖这些句子的译文，需要一点时间）`);
  if (!yes) return;
  const btn = $id('btnRetransPage');
  const old = btn.textContent;
  btn.disabled = true;
  btn.textContent = `重翻中 0/${indices.length}`;
  let tick = 0;
  const timer = setInterval(() => {
    tick = Math.min(indices.length, tick + 1);
    btn.textContent = `重翻中 ${tick}/${indices.length}`;
  }, 900);
  try {
    const res = await API.retranslateBatch(id, indices);
    if (res && res.segments) {
      applySegmentsFromServer(res.segments);
      const failed = (res.failed || []).length;
      toast(failed ? `本页重翻完成，${failed} 句失败` : '本页重翻完成', failed ? 'error' : 'success');
    }
  } catch (_) { /* toast 已弹 */ } finally {
    clearInterval(timer);
    btn.textContent = old;
    btn.disabled = false;
  }
}

function flushDirtyEdit() {
  const active = document.activeElement;
  if (active && active.classList && active.classList.contains('trans-input')) {
    maybeSave(active);
    active.blur();
  }
}

/* ---------------- 上传 / 文件选取 ---------------- */
function setupUpload() {
  const dz = $id('dropzone');
  const fi = $id('fileInput');

  dz.addEventListener('click', async () => {
    if (API.isDesktop()) {
      // 桌面模式：原生文件选择器 → 本地路径直接建任务（零拷贝）
      try {
        const paths = await API.pickMediaFiles();
        addPathsFromDesktop(paths);
      } catch (_) { /* 用户取消 */ }
      return;
    }
    fi.click();
  });
  dz.addEventListener('keydown', (e) => {
    if (e.key === 'Enter' || e.key === ' ') {
      e.preventDefault();
      dz.click();
    }
  });
  dz.addEventListener('dragenter', (e) => { e.preventDefault(); dz.classList.add('dragover'); });
  dz.addEventListener('dragover', (e) => { e.preventDefault(); dz.classList.add('dragover'); });
  dz.addEventListener('dragleave', (e) => {
    if (dz.contains(e.relatedTarget)) return;
    dz.classList.remove('dragover');
  });
  dz.addEventListener('drop', (e) => {
    e.preventDefault();
    dz.classList.remove('dragover');
    if (API.isDesktop()) return; // 桌面拖拽走 Tauri onDragDropEvent（见 init）
    const files = e.dataTransfer ? [...e.dataTransfer.files] : [];
    handleFiles(files);
  });
  fi.addEventListener('change', () => {
    handleFiles([...fi.files]);
    fi.value = '';
  });

  $id('btnAddLocal').addEventListener('click', addLocalPaths);
  $id('btnAddUrls').addEventListener('click', addUrlTasks);

  // 文件夹批量导入（仅桌面模式；浏览器模式隐藏按钮）
  const folderBtn = $id('btnAddFolder');
  if (!API.isDesktop()) {
    folderBtn.hidden = true;
  } else {
    folderBtn.addEventListener('click', async () => {
      try {
        const folder = await API.pickFolder();
        if (folder) addPathsFromDesktop([folder]);
      } catch (_) { /* 用户取消 */ }
    });
  }
}

/** 桌面模式：本地路径（选择器/拖拽给出）直接交给后端——
 *  文件按扩展名过滤建任务，文件夹递归展开为批量任务 */
async function addPathsFromDesktop(paths) {
  if (!paths || !paths.length) return;
  if (!(await ensureReadyForTasks())) return;
  try {
    const data = await API.addLocalTasks(paths, $id('sourceLang').value, $id('targetLang').value);
    const tasks = (data && data.tasks) || [];
    const missing = (data && data.missing) || [];
    const skipped = (data && data.skipped) || 0;
    for (const t of tasks) upsertTask(t);
    updateListChrome();
    if (tasks.length) {
      let msg = `已添加 ${tasks.length} 个任务`;
      if (skipped > 0) msg += `，跳过 ${skipped} 个非媒体文件`;
      toast(msg, 'success');
      selectTask(tasks[0].id);
    } else if (missing.length) {
      toast(`路径不存在：${missing[0]}`, 'error');
    } else if (skipped > 0) {
      toast(`没有可处理的媒体文件（跳过 ${skipped} 项）`, 'error');
    }
  } catch (_) { /* toast 已由封装弹出 */ }
}

function handleFiles(files) {
  const ok = [];
  const rejected = [];
  for (const f of files) {
    (ACCEPT_EXT.has(extOf(f.name)) ? ok : rejected).push(f);
  }
  if (rejected.length) {
    const names = rejected.slice(0, 3).map((f) => f.name).join('、');
    toast(`已跳过 ${rejected.length} 个不支持的文件：${names}${rejected.length > 3 ? ' 等' : ''}`, 'error');
  }
  if (ok.length) startUpload(ok);
}

function startUpload(files) {
  if (state.uploadBusy) { toast('有上传正在进行，请稍候', 'error'); return; }
  state.uploadBusy = true;

  const fd = new FormData();
  for (const f of files) fd.append('files[]', f, f.name);
  fd.append('source_lang', $id('sourceLang').value);
  fd.append('target_lang', $id('targetLang').value);

  const wrap = $id('uploadProgress');
  const fill = $id('uploadBarFill');
  const pct = $id('uploadPct');
  const label = $id('uploadLabel');
  wrap.hidden = false;
  fill.style.width = '0%';
  fill.className = 'bar-fill';
  pct.textContent = '0%';
  label.textContent = files.length === 1 ? `正在上传 ${files[0].name}` : `正在上传 ${files.length} 个文件…`;

  // fetch 不支持上传进度，用 XHR
  const xhr = new XMLHttpRequest();
  xhr.open('POST', '/api/tasks');
  xhr.upload.addEventListener('progress', (e) => {
    if (!e.lengthComputable) return;
    const p = e.loaded / e.total;
    fill.style.width = `${(p * 100).toFixed(1)}%`;
    pct.textContent = `${Math.round(p * 100)}%`;
  });
  xhr.addEventListener('load', () => {
    state.uploadBusy = false;
    wrap.hidden = true;
    if (xhr.status >= 200 && xhr.status < 300) {
      let tasks = [];
      try {
        const data = JSON.parse(xhr.responseText);
        tasks = (data && data.tasks) || [];
      } catch (_) { /* ignore */ }
      for (const t of tasks) upsertTask(t);
      updateListChrome();
      if (tasks.length) {
        toast(`已添加 ${tasks.length} 个任务`, 'success');
        selectTask(tasks[0].id);
      } else {
        toast('上传成功，但服务端未返回任务', 'error');
      }
    } else {
      let msg = `上传失败（HTTP ${xhr.status}）`;
      try {
        const j = JSON.parse(xhr.responseText);
        if (j && typeof j.detail === 'string') msg = j.detail;
      } catch (_) { /* ignore */ }
      toast(msg, 'error');
    }
  });
  xhr.addEventListener('error', () => {
    state.uploadBusy = false;
    wrap.hidden = true;
    toast('网络错误：上传失败，请确认后端已启动', 'error');
  });
  xhr.addEventListener('abort', () => {
    state.uploadBusy = false;
    wrap.hidden = true;
  });
  xhr.send(fd);
}

async function addLocalPaths() {
  const ta = $id('localPaths');
  const paths = ta.value.split(/\r?\n/).map((s) => s.trim()).filter(Boolean);
  if (!paths.length) { toast('请先输入至少一个本地路径（每行一个）', 'error'); return; }
  if (!(await ensureReadyForTasks())) return;

  const btn = $id('btnAddLocal');
  btn.disabled = true;
  try {
    const data = await API.addLocalTasks(paths, $id('sourceLang').value, $id('targetLang').value);
    const tasks = (data && data.tasks) || [];
    for (const t of tasks) upsertTask(t);
    updateListChrome();
    toast(`已添加 ${tasks.length} 个本地任务`, 'success');
    ta.value = '';
    $id('localPathBox').open = false;
    if (tasks.length) selectTask(tasks[0].id);
  } catch (_) {
    // toast 已由封装弹出
  } finally {
    btn.disabled = false;
  }
}

/** 链接下载：POST /api/tasks/url（yt-dlp 解析下载），后端逐条校验并返回 invalid 列表 */
async function addUrlTasks() {
  const ta = $id('downloadUrls');
  const urls = ta.value.split(/\r?\n/).map((s) => s.trim()).filter(Boolean);
  if (!urls.length) { toast('请先输入至少一个视频页链接（每行一个）', 'error'); return; }
  if (!(await ensureReadyForTasks())) return;

  const btn = $id('btnAddUrls');
  btn.disabled = true;
  try {
    const data = await API.addUrlTasks(urls, $id('sourceLang').value, $id('targetLang').value);
    const tasks = (data && data.tasks) || [];
    const invalid = (data && data.invalid) || [];
    for (const t of tasks) upsertTask(t);
    updateListChrome();
    if (tasks.length) {
      toast(`已添加 ${tasks.length} 个下载任务`, 'success');
      ta.value = '';
      $id('urlDownloadBox').open = false;
      selectTask(tasks[0].id);
    } else if (!invalid.length) {
      toast('未添加任何任务（服务端未返回有效任务）', 'error');
    }
    if (invalid.length) toast(`${invalid.length} 个链接无效`, 'error');
  } catch (_) {
    // toast 已由封装弹出
  } finally {
    btn.disabled = false;
  }
}

/* ---------------- 详情操作 ---------------- */
function setupDetailActions() {
  $id('btnCancel').addEventListener('click', async () => {
    const id = state.selectedId;
    if (!id) return;
    try {
      await API.cancelTask(id);
      toast('已发送取消请求，将在当前阶段结束后停止', 'success');
      refreshSingleTask(id);
    } catch (_) { /* ignore */ }
  });

  $id('btnRetry').addEventListener('click', async () => {
    const id = state.selectedId;
    if (!id) return;
    try {
      await API.retryTask(id);
      toast('任务已重新入队', 'success');
      refreshSingleTask(id);
    } catch (_) { /* ignore */ }
  });

  $id('btnDelete').addEventListener('click', async () => {
    const id = state.selectedId;
    if (!id) return;
    const t = state.tasks.get(id);
    const yes = await API.confirmBox(`确定删除任务「${(t && t.name) || id}」及其产物目录吗？`);
    if (!yes) return;
    try {
      await API.deleteTask(id);
      removeTaskLocal(id);
      toast('任务已删除', 'success');
    } catch (_) { /* ignore */ }
  });

  $id('btnRefreshDetail').addEventListener('click', async () => {
    const id = state.selectedId;
    if (!id) return;
    await refreshSingleTask(id);
    await refreshSegments(true);
    toast('已刷新', 'info');
  });

  $id('btnCloseDetail').addEventListener('click', () => {
    document.body.classList.remove('detail-open');
  });

  $id('btnOpenFolder').addEventListener('click', async () => {
    const id = state.selectedId;
    if (!id) return;
    try { await API.openFolder(id); } catch (_) { /* apiFetch 已提示 */ }
  });

  $id('btnCopyPath').addEventListener('click', async () => {
    const t = state.tasks.get(state.selectedId);
    if (!t || !t.dir) return;
    try {
      await navigator.clipboard.writeText(t.dir);
      toast('已复制目录路径', 'success');
    } catch (_) {
      toast('复制失败，请手动选中路径复制', 'error');
    }
  });

  // 一键导出全部格式（桌面版：选一个文件夹，一次写 6 个文件）
  $id('btnExportAll').addEventListener('click', async () => {
    const id = state.selectedId;
    if (!id) return;
    try {
      const res = await API.exportAll(id);
      if (res && res.dir) toast(`已导出 ${res.count || 6} 个文件到 ${res.dir}`, 'success');
      else if (res && res.path === null) { /* 用户取消 */ }
    } catch (_) { /* toast 已弹 */ }
  });

  // 复制双语文本（原文/译文交替，方便贴到别处）
  $id('btnCopySub').addEventListener('click', async () => {
    const segs = state.segments || [];
    if (!segs.length) { toast('字幕还没加载出来', 'error'); return; }
    const lines = [];
    for (const s of segs) {
      const o = (s.text || '').trim();
      const tr = (s.translated || '').trim();
      if (o) lines.push(o);
      if (tr && tr !== o) lines.push(tr);
      lines.push('');
    }
    const ok = await copyText(lines.join('\n').trim());
    toast(ok ? `已复制 ${segs.length} 句双语文本` : '复制失败', ok ? 'success' : 'error');
  });
}

/* ---------------- 队列工具条 / 批量操作 ---------------- */
function setupQueueTools() {
  const search = $id('queueSearch');
  $id('queueSort').value = state.ui.sort;
  if (state.ui.filter !== 'all') {
    for (const c of $id('queueFilters').querySelectorAll('.qt-chip')) {
      const on = c.dataset.filter === state.ui.filter;
      c.classList.toggle('is-on', on);
      c.setAttribute('aria-selected', on ? 'true' : 'false');
    }
  }

  const apply = debounce(() => {
    state.ui.search = search.value;
    $id('btnQueueClear').hidden = !search.value;
    sortTaskList();
  }, 120);
  search.addEventListener('input', apply);
  search.addEventListener('keydown', (e) => {
    if (e.key === 'Escape') {
      e.preventDefault();
      search.value = '';
      state.ui.search = '';
      $id('btnQueueClear').hidden = true;
      sortTaskList();
      search.blur();
    }
  });
  $id('btnQueueClear').addEventListener('click', () => {
    search.value = '';
    state.ui.search = '';
    $id('btnQueueClear').hidden = true;
    sortTaskList();
    search.focus();
  });

  $id('queueSort').addEventListener('change', () => {
    state.ui.sort = $id('queueSort').value;
    savePrefs({ sort: state.ui.sort });
    sortTaskList();
  });

  $id('queueFilters').addEventListener('click', (e) => {
    const chip = e.target.closest('.qt-chip');
    if (!chip) return;
    state.ui.filter = chip.dataset.filter;
    for (const c of $id('queueFilters').querySelectorAll('.qt-chip')) {
      const on = c === chip;
      c.classList.toggle('is-on', on);
      c.setAttribute('aria-selected', on ? 'true' : 'false');
    }
    savePrefs({ filter: state.ui.filter });
    sortTaskList();
  });

  $id('btnBatchMode').addEventListener('click', () => setBatchMode(!state.ui.batch));
  $id('batchAll').addEventListener('change', () => {
    const on = $id('batchAll').checked;
    state.ui.selected.clear();
    if (on) for (const t of visibleTasks()) state.ui.selected.add(t.id);
    for (const c of cardEls.values()) {
      c.check.checked = on && state.ui.batch && state.ui.selected.has(c.root.dataset.id);
      c.root.classList.toggle('batch-selected', on && state.ui.batch && state.ui.selected.has(c.root.dataset.id));
    }
    updateListChrome();
  });
  $id('btnBatchRetry').addEventListener('click', () => batchAction('retry'));
  $id('btnBatchCancel').addEventListener('click', () => batchAction('cancel'));
  $id('btnBatchDelete').addEventListener('click', () => batchAction('delete'));

  $id('btnLoadMore').addEventListener('click', async () => {
    const btn = $id('btnLoadMore');
    btn.disabled = true;
    state.ui.limit += QUEUE_PAGE;
    try { await loadTasks(); } finally { btn.disabled = false; }
  });

  $id('btnSetupClose').addEventListener('click', () => {
    state.ui.setupDismissed = true;
    $id('setupBanner').hidden = true;
    stopSetupPolling();
  });

  // 空队列引导：三条快速入口
  $id('esPickFile').addEventListener('click', () => $id('dropzone').click());
  $id('esPasteLink').addEventListener('click', () => {
    $id('urlDownloadBox').open = true;
    $id('dropzone').scrollIntoView({ block: 'nearest' });
    $id('downloadUrls').focus();
  });
  $id('esOpenSettings').addEventListener('click', () => openSettings());
}

async function batchAction(kind) {
  const ids = [...state.ui.selected];
  if (!ids.length) return;
  if (kind === 'delete') {
    const yes = await API.confirmBox(`确定删除选中的 ${ids.length} 个任务及其产物目录吗？此操作不可撤销。`);
    if (!yes) return;
  }
  let ok = 0;
  let fail = 0;
  for (const id of ids) {
    const t = state.tasks.get(id);
    if (!t) continue;
    try {
      if (kind === 'retry') {
        if (!(t.status === 'error' || t.status === 'canceled')) continue;
        await API.retryTask(id);
      } else if (kind === 'cancel') {
        if (!isActive(t)) continue;
        await API.cancelTask(id);
      } else if (kind === 'delete') {
        await API.deleteTask(id);
        removeTaskLocal(id);
      }
      ok++;
    } catch (_) {
      fail++;
    }
  }
  if (kind !== 'delete') {
    state.ui.selected.clear();
  }
  await loadTasks();
  const label = kind === 'retry' ? '重新入队' : kind === 'cancel' ? '取消' : '删除';
  if (!ok && !fail) { toast(`没有可${label}的选中任务`, 'info'); return; }
  toast(fail ? `${ok} 个任务已${label}，${fail} 个失败` : `已${label} ${ok} 个任务`, fail ? 'error' : 'success');
}

/** 添加任务前的「配置好了吗」提示：没配 Key 又没开 Mock 时给一次去设置的机会。 */
async function ensureReadyForTasks() {
  if (state.hasApiKey || state.mockMode) return true;
  const yes = await API.confirmBox(
    '还没配置翻译 API Key：任务会在语音识别完成后停在翻译阶段。\n\n'
    + '「确定」= 仍然添加（稍后可在任务卡上点重试续跑）\n「取消」= 先去设置页填写',
  );
  if (!yes) openSettings();
  return yes;
}

/* ---------------- 通用数值输入弹窗 ---------------- */
function askValue({ title, label, value = '', note = '', validate = null }) {
  return new Promise((resolve) => {
    const overlay = $id('askOverlay');
    const input = $id('askInput');
    $id('askTitle').textContent = title || '输入';
    $id('askLabel').textContent = label || '';
    $id('askNote').textContent = note || '';
    $id('askError').textContent = '';
    input.value = value;
    overlay.hidden = false;
    input.focus();
    input.select();

    const done = (result) => {
      overlay.hidden = true;
      input.removeEventListener('keydown', onKey);
      $id('btnAskOk').removeEventListener('click', onOk);
      $id('btnAskCancel').removeEventListener('click', onCancel);
      $id('btnAskClose').removeEventListener('click', onCancel);
      state.ui.askResolve = null;
      resolve(result);
    };
    const onOk = () => {
      const v = input.value.trim();
      const err = validate ? validate(v) : (v ? '' : '不能为空');
      if (err) { $id('askError').textContent = err; input.focus(); return; }
      done(v);
    };
    const onCancel = () => done(null);
    const onKey = (e) => {
      if (e.key === 'Enter') { e.preventDefault(); onOk(); }
      else if (e.key === 'Escape') { e.preventDefault(); onCancel(); }
    };
    input.addEventListener('keydown', onKey);
    $id('btnAskOk').addEventListener('click', onOk);
    $id('btnAskCancel').addEventListener('click', onCancel);
    $id('btnAskClose').addEventListener('click', onCancel);
    state.ui.askResolve = resolve;
  });
}

/* ---------------- 快捷键面板 ---------------- */
function openShortcuts() {
  $id('shortcutOverlay').hidden = false;
  state.ui.shortcutsOpen = true;
}
function closeShortcuts() {
  $id('shortcutOverlay').hidden = true;
  state.ui.shortcutsOpen = false;
}

/** 全局键盘快捷键：Ctrl+K 搜队列 / Ctrl+F 搜字幕 / ↑↓ 选句 / ? 帮助 */
function setupGlobalShortcuts() {
  $id('btnShortcuts').addEventListener('click', openShortcuts);
  $id('btnCloseShortcuts').addEventListener('click', closeShortcuts);
  $id('btnCloseShortcuts2').addEventListener('click', closeShortcuts);
  $id('shortcutOverlay').addEventListener('click', (e) => {
    if (e.target === $id('shortcutOverlay')) closeShortcuts();
  });

  document.addEventListener('keydown', (e) => {
    const tag = (e.target && e.target.tagName) || '';
    const typing = /^(INPUT|TEXTAREA|SELECT)$/.test(tag)
      || (e.target && e.target.isContentEditable);

    // ?：帮助（输入框内不拦）
    if (e.key === '?' && !typing && !e.ctrlKey && !e.metaKey && !e.altKey) {
      e.preventDefault();
      if (state.ui.shortcutsOpen) closeShortcuts();
      else openShortcuts();
      return;
    }
    // Ctrl+K：搜队列
    if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === 'k') {
      e.preventDefault();
      closeSettings();
      closeShortcuts();
      $id('queueSearch').focus();
      $id('queueSearch').select();
      return;
    }
    // Ctrl+F：搜字幕
    if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === 'f') {
      e.preventDefault();
      if (!state.selectedId) return;
      $id('segSearch').focus();
      $id('segSearch').select();
      return;
    }
    // Ctrl+S：保存当前译文
    if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === 's') {
      e.preventDefault();
      flushDirtyEdit();
      return;
    }
    if (e.key === 'Escape') {
      if ($id('askOverlay').hidden === false) { /* askValue 自己处理 */ return; }
      if (!$id('shortcutOverlay').hidden) { closeShortcuts(); return; }
      closeCardMenu();
      return;
    }
    // 字幕/播放快捷键：仅在详情打开且焦点不在输入类元素时生效
    if (!state.selectedId) return;
    if (typing) return;
    if (e.target && e.target.closest && e.target.closest('button, a')) return;
    if (!document.body.classList.contains('detail-open')) return;
    if (e.altKey || e.ctrlKey || e.metaKey) return;
    // 只在字幕表 / 播放器区域内拦截方向键，避免影响左侧队列滚动
    const inSeg = e.target && e.target.closest && e.target.closest('.detail-segments, .detail-media-wrap');
    if (!inSeg) return;

    if (e.key === 'ArrowDown') {
      e.preventDefault();
      moveActive(1);
    } else if (e.key === 'ArrowUp') {
      e.preventDefault();
      moveActive(-1);
    } else if (e.key === 'Enter' && state.ui.activeSeg != null) {
      e.preventDefault();
      moveActive(0, { focusInput: true });
    } else if (e.key === ' ' && state.mediaEl) {
      e.preventDefault();
      if (state.mediaEl.paused) state.mediaEl.play().catch(() => {});
      else state.mediaEl.pause();
    } else if (e.key === 'ArrowLeft' && state.mediaEl) {
      e.preventDefault();
      seekTo((state.mediaEl.currentTime || 0) - 2);
    } else if (e.key === 'ArrowRight' && state.mediaEl) {
      e.preventDefault();
      seekTo((state.mediaEl.currentTime || 0) + 2);
    }
  });
}

function setupSegmentsUI() {
  const body = $id('segBody');

  body.addEventListener('input', (e) => {
    if (e.target.classList && e.target.classList.contains('trans-input')) {
      e.target.classList.add('dirty');
    }
  });
  body.addEventListener('focusin', (e) => {
    const tr = e.target.closest && e.target.closest('tr');
    if (tr && tr.dataset.index != null && e.target.classList.contains('trans-input')) {
      const idx = Number(tr.dataset.index);
      if (Number.isInteger(idx)) state.ui.activeSeg = idx;
    }
  });
  body.addEventListener('focusout', (e) => {
    if (e.target && e.target.classList && e.target.classList.contains('trans-input')) {
      setTimeout(() => maybeSave(e.target), 120);
    }
  });
  body.addEventListener('keydown', (e) => {
    const input = e.target;
    if (!input.classList || !input.classList.contains('trans-input')) return;
    if (e.key === 'Enter' && (e.ctrlKey || !e.shiftKey)) {
      e.preventDefault();
      input.blur();
      if (e.ctrlKey) moveActive(1, { focusInput: true });
    } else if (e.key === 'Escape') {
      e.preventDefault();
      input.value = input.dataset.orig ?? '';
      input.classList.remove('dirty');
      input.blur();
    }
  });
  body.addEventListener('click', (e) => {
    const opBtn = e.target.closest('.seg-op');
    if (opBtn) { handleSegOp(opBtn); return; }
    const btn = e.target.closest('.btn-retrans');
    if (btn) { retranslateRow(btn); return; }
    const tr = e.target.closest('tr.seg-row');
    if (tr && tr.dataset.index != null) setActiveSeg(Number(tr.dataset.index));
  });

  // 字幕搜索（Ctrl+F）：输入即搜，回车跳下一个
  const search = $id('segSearch');
  const runSearch = debounce(() => {
    state.ui.segQuery = search.value || '';
    computeSegMatches();
    renderSegments();             // 先重绘，把命中行打上高亮
    if (state.ui.segMatches.length) gotoMatch(0);
  }, 150);
  search.addEventListener('input', runSearch);
  search.addEventListener('keydown', (e) => {
    if (e.key === 'Enter') {
      e.preventDefault();
      if (state.ui.segMatches.length) {
        gotoMatch(state.ui.segMatchPos + (e.shiftKey ? -1 : 1));
      }
    } else if (e.key === 'Escape') {
      e.preventDefault();
      search.value = '';
      state.ui.segQuery = '';
      computeSegMatches();
      renderSegments();
      search.blur();
    }
  });
  $id('btnSegSearchPrev').addEventListener('click', () => gotoMatch(state.ui.segMatchPos - 1));
  $id('btnSegSearchNext').addEventListener('click', () => gotoMatch(state.ui.segMatchPos + 1));

  $id('btnShiftApply').addEventListener('click', shiftTimeline);
  $id('shiftInput').addEventListener('keydown', (e) => {
    if (e.key === 'Enter') { e.preventDefault(); shiftTimeline(); }
  });
  $id('btnRetransPage').addEventListener('click', retranslatePage);

  const goPage = (p) => {
    flushDirtyEdit();
    state.segPage = p;
    renderSegments();
    const sc = document.querySelector('.seg-scroll');
    if (sc) sc.scrollTop = 0;
  };
  $id('btnPageFirst').addEventListener('click', () => goPage(1));
  $id('btnPagePrev').addEventListener('click', () => goPage(state.segPage - 1));
  $id('btnPageNext').addEventListener('click', () => goPage(state.segPage + 1));
  $id('btnPageLast').addEventListener('click', () => goPage(999999));
}

/* ---------------- 设置弹窗 ---------------- */
function setSelect(sel, val, fallback) {
  sel.value = String(val);
  if (sel.value !== String(val)) sel.value = fallback;
}

function glossaryToText(g) {
  return Object.entries(g || {}).map(([k, v]) => `${k}=${v}`).join('\n');
}

function parseGlossary(text) {
  const out = {};
  for (let line of String(text || '').split(/\r?\n/)) {
    line = line.trim();
    if (!line || line.startsWith('#')) continue;
    const i = line.indexOf('=');
    if (i <= 0) continue;
    const k = line.slice(0, i).trim();
    const v = line.slice(i + 1).trim();
    if (k) out[k] = v;
  }
  return out;
}

async function openSettings() {
  $id('modalOverlay').hidden = false;
  $id('testResult').hidden = true;
  await loadConfigIntoForm();
  loadModels();
}

function closeSettings() {
  $id('modalOverlay').hidden = true;
  stopModelPolling();
}

async function loadConfigIntoForm() {
  let cfg;
  try {
    cfg = await API.getConfig();
  } catch (_) {
    return; // toast 已弹
  }
  fillConfigForm(cfg);
  loadDataInfo();
}

/** 把配置对象填进表单（供「读取配置」与「从剪贴板导入」共用） */
function fillConfigForm(cfg) {
  state.hasApiKey = !!(cfg
    ? (cfg.has_api_key ?? (cfg.translate && cfg.translate.has_api_key))
    : false);
  if (cfg && cfg.translate) state.mockMode = !!cfg.translate.mock;

  const asr = (cfg && cfg.asr) || {};
  const tr = (cfg && cfg.translate) || {};
  const out = (cfg && cfg.output) || {};
  const dl = (cfg && cfg.download) || {};

  setSelect($id('cfgModel'), asr.model ?? 'small', 'small');
  setSelect($id('cfgDevice'), asr.device ?? 'auto', 'auto');
  setSelect($id('cfgCompute'), asr.compute_type ?? 'auto', 'auto');

  $id('cfgBaseUrl').value = tr.base_url ?? '';
  $id('cfgApiKey').value = ''; // 永不回填真实 key；服务端也只返回 ***
  $id('cfgApiKey').placeholder = state.hasApiKey ? '已保存则留空或 ***' : 'sk-…';
  $id('cfgTranslateModel').value = tr.model ?? '';
  $id('cfgTemperature').value = tr.temperature ?? 0.2;
  $id('cfgConcurrency').value = tr.concurrency ?? 3;
  $id('cfgBatchSize').value = tr.batch_size ?? 40;
  $id('cfgProofread').checked = !!tr.proofread;
  $id('cfgMock').checked = !!tr.mock;
  $id('cfgGlossary').value = glossaryToText(tr.glossary);

  $id('cfgBilingual').checked = out.bilingual !== false;
  setSelect($id('cfgBilingualOrder'), out.bilingual_order ?? 'trans_first', 'trans_first');
  $id('cfgMaxLineChars').value = out.max_line_chars ?? 42;

  $id('cfgDownloadHeight').value = dl.height ?? 720;
  $id('cfgDownloadProxy').value = dl.proxy ?? '';
  $id('cfgDownloadCookies').value = dl.cookies_from_browser ?? '';

  $id('cfgKeyState').textContent = state.hasApiKey ? '已保存 API Key（输入框留空即沿用）' : '尚未保存 API Key';
  $id('providerNote').textContent = '';
  $id('cfgProvider').value = '';
}

/** 设置页底部：数据目录位置（桌面版） */
async function loadDataInfo() {
  const el = $id('cfgDataInfo');
  if (!el) return;
  if (!API.isDesktop()) { el.textContent = '浏览器模式下数据目录由 Python 后端管理'; return; }
  try {
    const p = await API.appPaths();
    el.textContent = p && p.data ? `数据目录：${p.data}` : '';
    el.title = p && p.output ? `产物目录：${p.output}` : '';
  } catch (_) { el.textContent = ''; }
}

/** 提交规则：留空且已保存过 → 发送 "***"（复用已保存 key）；字面输入 *** 亦同 */
function collectApiKey() {
  const v = $id('cfgApiKey').value.trim();
  if (v === '***') return '***';
  if (v === '' && state.hasApiKey) return '***';
  return v;
}

function collectConfig() {
  const num = (el, def) => {
    const v = parseFloat(el.value);
    return Number.isFinite(v) ? v : def;
  };
  const int = (el, def, min = 1) => Math.max(min, Math.round(num(el, def)));
  return {
    asr: {
      model: $id('cfgModel').value,
      device: $id('cfgDevice').value,
      compute_type: $id('cfgCompute').value,
    },
    translate: {
      base_url: $id('cfgBaseUrl').value.trim(),
      api_key: collectApiKey(),
      model: $id('cfgTranslateModel').value.trim(),
      temperature: num($id('cfgTemperature'), 0.2),
      concurrency: int($id('cfgConcurrency'), 3),
      batch_size: int($id('cfgBatchSize'), 40),
      proofread: $id('cfgProofread').checked,
      mock: $id('cfgMock').checked,
      glossary: parseGlossary($id('cfgGlossary').value),
    },
    output: {
      bilingual: $id('cfgBilingual').checked,
      bilingual_order: $id('cfgBilingualOrder').value,
      max_line_chars: int($id('cfgMaxLineChars'), 42, 10),
    },
    download: {
      height: Math.min(2160, Math.max(144, Math.round(num($id('cfgDownloadHeight'), 720)))),
      proxy: $id('cfgDownloadProxy').value.trim(),
      cookies_from_browser: $id('cfgDownloadCookies').value.trim().toLowerCase(),
    },
  };
}

async function saveSettings() {
  const btn = $id('btnSaveSettings');
  btn.disabled = true;
  try {
    const saved = await API.saveConfig(collectConfig());
    state.hasApiKey = !!(saved
      ? (saved.has_api_key ?? (saved.translate && saved.translate.has_api_key))
      : false);
    state.mockMode = !!($id('cfgMock').checked);
    $id('cfgKeyState').textContent = state.hasApiKey ? '已保存 API Key（输入框留空即沿用）' : '尚未保存 API Key';
    $id('cfgApiKey').value = '';
    $id('cfgApiKey').placeholder = state.hasApiKey ? '已保存则留空或 ***' : 'sk-…';
    toast('设置已保存', 'success');
    refreshModelChip(); // 默认模型可能已变，同步顶部 chip + 就绪检查
    refreshSetupBanner();
    loadModels(true);   // 切换默认模型后刷新「默认」标记
  } catch (_) {
    // toast 已由封装弹出
  } finally {
    btn.disabled = false;
  }
}

async function testTranslate() {
  const btn = $id('btnTestTranslate');
  const box = $id('testResult');
  btn.disabled = true;
  const old = btn.textContent;
  btn.textContent = '测试中…';
  try {
    const res = await API.testTranslate({
      base_url: $id('cfgBaseUrl').value.trim(),
      api_key: collectApiKey(),
      model: $id('cfgTranslateModel').value.trim(),
    });
    box.hidden = false;
    box.textContent = '';
    box.className = 'test-result ' + (res && res.ok ? 'ok' : 'bad');
    const head = document.createElement('div');
    head.textContent = (res && res.ok ? '✓ 连接成功' : '✕ 连接失败') +
      (res && res.message ? `：${res.message}` : '');
    box.appendChild(head);
    if (res && res.ok && res.sample) {
      const pre = document.createElement('pre');
      pre.textContent = res.sample;
      box.appendChild(pre);
    }
  } catch (_) {
    // 非 2xx 时封装已弹 toast；再显示一个失败结果框
    box.hidden = false;
    box.className = 'test-result bad';
    box.textContent = '✕ 连接失败（请求出错，详见 toast）';
  } finally {
    btn.disabled = false;
    btn.textContent = old;
  }
}

/* ---------------- 服务商预设 / 配置导入导出 ---------------- */
const PROVIDER_PRESETS = {
  deepseek: {
    base_url: 'https://api.deepseek.com/v1',
    model: 'deepseek-flash',
    note: 'DeepSeek 官方：国内直连，模型名可用 deepseek-flash / deepseek-v4-pro',
  },
  moonshot: {
    base_url: 'https://api.moonshot.cn/v1',
    model: 'moonshot-v1-8k',
    note: 'Kimi / Moonshot 开放平台：需先在控制台创建 API Key',
  },
  openai: {
    base_url: 'https://api.openai.com/v1',
    model: 'gpt-4o-mini',
    note: 'OpenAI 官方：国内需代理（下载分组里可填代理，翻译请求会走系统代理）',
  },
  ollama: {
    base_url: 'http://127.0.0.1:11434/v1',
    model: 'qwen2.5:7b',
    note: '本地 Ollama：需要先 ollama serve 并拉取对应模型，API Key 随便填',
  },
  oneapi: {
    base_url: 'http://127.0.0.1:3000/v1',
    model: 'deepseek-chat',
    note: '自建 One API / new-api 中转：把地址和模型名改成你自己的',
  },
};

function applyProviderPreset(key) {
  const p = PROVIDER_PRESETS[key];
  if (!p) { $id('providerNote').textContent = ''; return; }
  $id('cfgBaseUrl').value = p.base_url;
  $id('cfgTranslateModel').value = p.model;
  $id('providerNote').textContent = p.note;
  // 预设不改已保存的 Key；提醒用户填（若还没有）
  if (!state.hasApiKey) {
    $id('cfgApiKey').focus();
  }
}

async function exportConfigToClipboard() {
  const cfg = collectConfig();
  delete cfg.translate.api_key;
  const text = JSON.stringify(cfg, null, 2);
  const ok = await copyText(text);
  toast(ok ? '配置已复制到剪贴板（不含 API Key）' : '复制失败', ok ? 'success' : 'error');
}

async function importConfigFromClipboard() {
  let text = '';
  try {
    text = await navigator.clipboard.readText();
  } catch (_) {
    text = '';
  }
  if (!text.trim()) {
    text = await askValue({
      title: '导入配置',
      label: '把「复制配置」得到的 JSON 粘贴到这里',
      value: '',
      note: '只填入表单，不会自动保存；确认无误后再点「保存设置」',
      validate: (v) => {
        if (!v) return '不能为空';
        try { JSON.parse(v); return ''; } catch (e) { return 'JSON 解析失败：' + (e.message || ''); }
      },
    });
    if (text === null) return;
  }
  let cfg;
  try {
    cfg = JSON.parse(text);
  } catch (e) {
    toast('JSON 解析失败：' + (e.message || ''), 'error');
    return;
  }
  if (!cfg || typeof cfg !== 'object') { toast('配置格式不对：需要一个 JSON 对象', 'error'); return; }
  fillConfigForm(cfg);
  $id('providerNote').textContent = '已从剪贴板导入，确认后点「保存设置」生效';
  toast('配置已填入表单，确认后点保存', 'success');
}

async function openDataDir() {
  try {
    const p = await API.appPaths();
    if (!p || !p.data) { toast('拿不到数据目录', 'error'); return; }
    await API.revealPath(p.output && p.output.length ? p.output : p.data);
  } catch (_) { /* toast 已弹 */ }
}

function setupSettings() {
  $id('btnSettings').addEventListener('click', openSettings);
  $id('btnCloseSettings').addEventListener('click', closeSettings);
  $id('btnCancelSettings').addEventListener('click', closeSettings);
  $id('btnSaveSettings').addEventListener('click', saveSettings);
  $id('btnTestTranslate').addEventListener('click', testTranslate);
  $id('btnModelsRefresh').addEventListener('click', () => loadModels());
  $id('cfgProvider').addEventListener('change', (e) => applyProviderPreset(e.target.value));
  $id('btnExportConfig').addEventListener('click', exportConfigToClipboard);
  $id('btnImportConfig').addEventListener('click', importConfigFromClipboard);
  $id('btnOpenDataDir').addEventListener('click', openDataDir);
  if (!API.isDesktop()) {
    $id('btnOpenDataDir').hidden = true;
  }
  $id('cfgMock').addEventListener('change', () => {
    if ($id('cfgMock').checked) $id('providerNote').textContent = 'Mock 模式：不调用翻译 API，用确定性伪翻译跑通全流程（测试用）';
  });

  $id('modalOverlay').addEventListener('click', (e) => {
    if (e.target === $id('modalOverlay')) closeSettings();
  });
  document.addEventListener('keydown', (e) => {
    if (e.key !== 'Escape') return;
    if (!$id('modalOverlay').hidden) {
      closeSettings();
    } else if (document.body.classList.contains('detail-open') &&
               window.matchMedia('(max-width: 1279px)').matches) {
      document.body.classList.remove('detail-open');
    }
  });
}

/* ---------------- 模型管理 ---------------- */
async function loadModels(silent = false) {
  const list = $id('modelsList');
  let data;
  try {
    data = await API.listModels(silent);
  } catch (_) {
    if (!silent) {
      list.textContent = '';
      const d = document.createElement('div');
      d.className = 'models-loading';
      d.textContent = '模型列表加载失败，请点击「刷新」重试';
      list.appendChild(d);
    }
    return;
  }
  renderModels((data && data.models) || []);
}

function renderModels(models) {
  const list = $id('modelsList');
  list.textContent = '';

  if (!models.length) {
    const d = document.createElement('div');
    d.className = 'models-loading';
    d.textContent = '暂无模型信息';
    list.appendChild(d);
    stopModelPolling();
    return;
  }

  for (const m of models) list.appendChild(buildModelRow(m));

  // 有 downloading 项 → 每 2s 轮询；全部 idle/ready/error → 停止
  if (models.some((m) => m.status === 'downloading')) startModelPolling();
  else stopModelPolling();
}

function buildModelRow(m) {
  const row = document.createElement('div');
  row.className = 'model-row';

  const top = document.createElement('div');
  top.className = 'model-row-top';

  const name = document.createElement('span');
  name.className = 'model-name';
  name.textContent = m.name || '';

  const size = document.createElement('span');
  size.className = 'model-size';
  size.textContent = m.size_hint || '';

  const ms = MODEL_STATUS_META[m.status] || MODEL_STATUS_META.idle;
  const badge = document.createElement('span');
  badge.className = `badge ${ms.cls}`;
  const dot = document.createElement('i');
  dot.className = 'dot';
  const btxt = document.createElement('span');
  btxt.textContent = ms.label;
  badge.append(dot, btxt);

  const btn = document.createElement('button');
  btn.type = 'button';
  btn.className = 'btn btn-ghost btn-sm';
  if (m.status === 'downloading') {
    btn.disabled = true;
    btn.textContent = '下载中…';
  } else {
    btn.textContent = m.status === 'ready' ? '重新下载' : '下载';
    btn.addEventListener('click', async () => {
      btn.disabled = true;
      btn.textContent = '…';
      try {
        await API.downloadModel(m.name);
        toast(`开始下载模型 ${m.name}`, 'success');
        loadModels(true);
      } catch (_) {
        btn.disabled = false;
        btn.textContent = '下载';
      }
    });
  }

  top.append(name, size, badge, btn);

  // 已下载的模型：提供删除（释放磁盘）
  if (m.status === 'ready' && API.isDesktop()) {
    const del = document.createElement('button');
    del.type = 'button';
    del.className = 'btn btn-ghost btn-sm model-del';
    del.textContent = '删除';
    del.title = '删除本地模型文件以释放磁盘空间（下次识别会重新下载）';
    del.addEventListener('click', async () => {
      const yes = await API.confirmBox(`删除本地模型 ${m.name}？下次识别会重新下载。`);
      if (!yes) return;
      del.disabled = true;
      try {
        await API.deleteModel(m.name);
        toast(`已删除模型 ${m.name}`, 'success');
        loadModels(true);
        refreshModelChip();
      } catch (_) {
        del.disabled = false;
      }
    });
    top.appendChild(del);
  }

  // 当前默认模型标记
  const current = state.cfg && state.cfg.asr && state.cfg.asr.model;
  if (current && current === m.name) {
    const tag = document.createElement('span');
    tag.className = 'model-current';
    tag.textContent = '默认';
    tag.title = '设置里当前选中的识别模型';
    top.appendChild(tag);
  }

  row.appendChild(top);

  if (Array.isArray(m.repos) && m.repos.length) {
    const repos = document.createElement('div');
    repos.className = 'model-repos';
    repos.textContent = m.repos.join(', ');
    row.appendChild(repos);
  }

  if (m.status === 'downloading') {
    const barRow = document.createElement('div');
    barRow.className = 'model-bar-row';
    const bar = document.createElement('div');
    bar.className = 'bar';
    const fill = document.createElement('div');
    fill.className = 'bar-fill';
    fill.style.width = `${Math.round(clamp01(m.progress) * 100)}%`;
    bar.appendChild(fill);
    const pct = document.createElement('span');
    pct.className = 'model-pct mono';
    pct.textContent = `${Math.round(clamp01(m.progress) * 100)}%`;
    barRow.append(bar, pct);
    row.appendChild(barRow);
  }

  if (m.detail) {
    const detail = document.createElement('div');
    detail.className = 'model-detail' + (m.status === 'error' ? '' : ' info');
    detail.textContent = m.detail;
    row.appendChild(detail);
  }

  return row;
}

function startModelPolling() {
  if (modelTimer) return;
  modelTimer = setInterval(() => {
    if ($id('modalOverlay').hidden) { stopModelPolling(); return; }
    loadModels(true);
  }, MODEL_POLL_MS);
}

function stopModelPolling() {
  if (modelTimer) {
    clearInterval(modelTimer);
    modelTimer = null;
  }
}

/* ---------------- 启动 ---------------- */
function setupDesktopIntegration() {
  if (!API.isDesktop()) return;
  // 全局任务事件（对应浏览器模式的 SSE）：一条通道收所有任务的进度
  API.onTaskEvents((data) => {
    if (!data || data.type === 'ping') return;
    if (data.task) upsertTask(data.task);
  });
  // 成品视频渲染进度
  API.onBurnEvents((data) => {
    if (!data || !data.taskId) return;
    const b = burnState.get(data.taskId) || {};
    if (data.phase === 'running') {
      burnState.set(data.taskId, { ...b, phase: 'running', progress: data.progress || 0 });
    } else if (data.phase === 'done') {
      burnState.set(data.taskId, { phase: 'done', path: data.path });
      toast(`成品视频已生成：${data.path}`, 'success');
    } else if (data.phase === 'error') {
      burnState.set(data.taskId, { phase: 'error' });
      toast(`成品视频渲染失败：${data.message || '未知错误'}`, 'error');
    } else if (data.phase === 'canceled') {
      burnState.set(data.taskId, { phase: 'canceled' });
      toast('已取消成品视频渲染', 'info');
    }
    if (state.selectedId === data.taskId && state.tasks.get(data.taskId)) {
      updateExports(state.tasks.get(data.taskId));
    }
  });
  // 桌面级拖拽：直接给本地路径
  try {
    const win = window.__TAURI__.window.getCurrentWindow();
    win.onDragDropEvent((ev) => {
      const payload = ev.payload;
      const dz = $id('dropzone');
      if (payload.type === 'over' || payload.type === 'enter') {
        dz.classList.add('dragover');
      } else if (payload.type === 'leave') {
        dz.classList.remove('dragover');
      } else if (payload.type === 'drop') {
        dz.classList.remove('dragover');
        addPathsFromDesktop(payload.paths || []);
      }
    });
  } catch (err) {
    console.warn('拖拽集成不可用', err);
  }
}

/** 恢复上次的语言 / 排序 / 筛选偏好 */
function applyPrefs() {
  const src = ['auto', 'zh', 'en', 'ja'];
  if (prefs.sourceLang && src.includes(prefs.sourceLang)) $id('sourceLang').value = prefs.sourceLang;
  if (prefs.targetLang && ['zh', 'en', 'ja'].includes(prefs.targetLang)) $id('targetLang').value = prefs.targetLang;
  $id('sourceLang').addEventListener('change', () => savePrefs({ sourceLang: $id('sourceLang').value }));
  $id('targetLang').addEventListener('change', () => savePrefs({ targetLang: $id('targetLang').value }));
  if (prefs.sort) state.ui.sort = prefs.sort;
  if (prefs.filter) state.ui.filter = prefs.filter;
}

function init() {
  applyPrefs();
  setupUpload();
  setupDetailActions();
  setupSegmentsUI();
  setupQueueTools();
  setupSettings();
  setupGlobalShortcuts();
  setupDesktopIntegration();

  // 浏览器模式（Python 后端）没有这些桌面能力：整块隐藏，避免点了报错
  if (!API.isDesktop()) {
    $id('segTimeTools').hidden = true;
    const opsTh = document.querySelector('.seg-table th.col-ops');
    if (opsTh) opsTh.hidden = true;
    $id('btnExportAll').hidden = true;
  }

  showListSkeleton();  // 首次任务列表加载前展示骨架
  refreshHealth();
  refreshModelChip();
  loadTasks().then(() => {
    // 恢复上次打开的任务
    if (prefs.lastTaskId && state.tasks.has(prefs.lastTaskId)) selectTask(prefs.lastTaskId);
  });
  setInterval(() => { loadTasks(); }, POLL_MS);
  setInterval(() => { refreshHealth(); }, HEALTH_MS);

  updateListChrome();
}

init();
