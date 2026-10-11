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

/* ---------------- 全局状态 ---------------- */
const state = {
  tasks: new Map(),        // id -> Task
  selectedId: null,
  segments: null,          // 当前选中任务的段数组
  segmentsFor: null,       // segments 属于哪个任务 id
  segmentsLoading: false,
  segPage: 1,
  hasApiKey: false,
  uploadBusy: false,
  bootstrapped: false,     // 首次任务列表加载是否完成（期间展示骨架屏）
};

const sse = new Map();     // id -> EventSource
const cardEls = new Map(); // id -> {root, name, badge, badgeText, barWrap, fill, pct, msg, meta}
let modelTimer = null;     // 模型下载轮询定时器

const $id = (id) => document.getElementById(id);

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
    const gpuName = h && h.gpu && h.gpu.available ? h.gpu.name : null;
    const ffmpegOk = !!(h && h.ffmpeg);
    dot.className = 'h-dot ' + (ffmpegOk ? (gpuName ? 'ok' : 'warn') : 'warn');
    text.textContent = gpuName ? `GPU 就绪 · ${gpuName}` : (ffmpegOk ? '后端已连接' : '后端已连接（缺 ffmpeg）');
    wrap.title = `后端 v${(h && h.version) || '?'} · ffmpeg：${ffmpegOk ? '可用' : '未检测到'} · GPU：${gpuName || '不可用（CPU 模式）'}`;
  } catch (_) {
    dot.className = 'h-dot bad';
    text.textContent = '后端未连接';
    wrap.title = '无法连接后端（FastAPI @ http://127.0.0.1:8765）';
  }
}

/** 顶部状态胶囊旁的当前默认 ASR 模型名（读 /api/config，静默失败） */
async function refreshModelChip() {
  const el = $id('modelChip');
  if (!el) return;
  try {
    const cfg = await API.getConfig(true);
    const m = cfg && cfg.asr && cfg.asr.model ? String(cfg.asr.model) : '';
    if (m) {
      el.hidden = false;
      el.textContent = m;
      el.title = `当前默认 ASR 模型：${m}`;
    }
  } catch (_) { /* 静默：后端离线时由健康灯呈现 */ }
}

/* ---------------- 任务列表：卡片 ---------------- */
function ensureCard(t) {
  let c = cardEls.get(t.id);
  if (c) return c;

  const root = document.createElement('div');
  root.className = 'task-card';
  root.dataset.id = t.id;
  root.setAttribute('role', 'listitem');
  root.tabIndex = 0;
  root.innerHTML = [
    '<div class="tc-row1"><span class="tc-name"></span>',
    '<span class="badge"><i class="dot"></i><span class="badge-text"></span></span></div>',
    '<div class="tc-bar"><div class="bar"><div class="bar-fill"></div></div><span class="tc-pct mono"></span></div>',
    '<div class="tc-msg"></div>',
    '<div class="tc-meta mono"></div>',
  ].join('');

  c = {
    root,
    name: root.querySelector('.tc-name'),
    badge: root.querySelector('.badge'),
    badgeText: root.querySelector('.badge-text'),
    barWrap: root.querySelector('.tc-bar'),
    fill: root.querySelector('.bar-fill'),
    pct: root.querySelector('.tc-pct'),
    msg: root.querySelector('.tc-msg'),
    meta: root.querySelector('.tc-meta'),
  };
  root.addEventListener('click', () => selectTask(t.id));
  root.addEventListener('keydown', (e) => {
    if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); selectTask(t.id); }
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
  const dt = fmtDT(t.created_at);
  if (dt) bits.push(relTime(t.created_at)); // 卡片上显示相对时间
  c.meta.textContent = bits.join(' · ');
  c.meta.title = dt ? `创建于 ${dt}` : '';  // hover 显示绝对时间

  c.root.classList.toggle('selected', t.id === state.selectedId);
  // 状态驱动的卡片着色（CSS: .tcst-*）
  c.root.classList.remove('tcst-done', 'tcst-error', 'tcst-canceled');
  if (t.status === 'done' || t.status === 'error' || t.status === 'canceled') {
    c.root.classList.add(`tcst-${t.status}`);
  }
}

/** 按创建时间倒序排列；仅当顺序变化时才移动节点，避免轮询打断 hover */
function sortTaskList() {
  const listEl = $id('taskList');
  const sorted = [...state.tasks.values()].sort((a, b) =>
    String(b.created_at || '').localeCompare(String(a.created_at || '')));
  const desired = sorted.map((t) => t.id).filter((id) => cardEls.has(id));
  const current = [...listEl.children].map((n) => n.dataset.id);
  if (current.join('\u0001') === desired.join('\u0001')) return;
  for (const id of desired) listEl.appendChild(cardEls.get(id).root);
}

function updateListChrome() {
  $id('taskCount').textContent = String(state.tasks.size);
  // 首次加载完成前不显示空状态（此时是骨架屏）
  $id('emptyState').hidden = !state.bootstrapped || state.tasks.size > 0;
}

function removeTaskLocal(id) {
  state.tasks.delete(id);
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
  let data;
  try {
    data = await API.listTasks(100);
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
  if (!state.bootstrapped) {
    state.bootstrapped = true;
    clearListSkeleton();
  }
  for (const t of list) upsertTask(t);
  // 服务端已不存在的任务（limit 内全量返回时才做清理，避免误删 100 条之外的缓存）
  if (list.length < 100) {
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
  state.selectedId = id;
  state.segments = null;
  state.segmentsFor = null;
  state.segPage = 1;

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
  if (!t) return;

  const src = `/api/media/${encodeURIComponent(t.id)}`;
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
  track.src = `/api/tasks/${encodeURIComponent(t.id)}/download?type=bilingual_vtt`;
  m.appendChild(track);

  m.addEventListener('error', () => {
    const tip = document.createElement('div');
    tip.className = 'media-tip';
    tip.style.color = 'var(--red)';
    tip.textContent = '媒体文件加载失败（可能已被移动或删除）';
    slot.appendChild(tip);
  });

  slot.appendChild(m);
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

function updateExports(t) {
  const ready = !!t.has_result;
  for (const a of $id('exportRow').querySelectorAll('a')) {
    if (ready) {
      a.href = `/api/tasks/${encodeURIComponent(t.id)}/download?type=${a.dataset.type}`;
      a.classList.remove('disabled');
    } else {
      a.removeAttribute('href');
      a.classList.add('disabled');
    }
  }
}

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
  for (const b of ['btnPageFirst', 'btnPagePrev', 'btnPageNext', 'btnPageLast']) $id(b).disabled = true;
  const box = $id('segEmpty');
  box.hidden = false;
  if (!t) { box.textContent = '暂无任务'; return; }
  if (t.status === 'error') box.textContent = '任务失败，没有可用的字幕。';
  else if (t.status === 'canceled') box.textContent = '任务已取消，没有生成字幕。';
  else if (isActive(t)) box.textContent = `${metaLabel(t)}进行中 — 完成后即可在此编辑、重翻与导出字幕。`;
  else box.textContent = '暂无字幕数据，点击「刷新」重试。';
}

function metaLabel(t) { return STATUS_META[t.status] ? STATUS_META[t.status].label : (t.status || ''); }

function renderSegments() {
  const segs = state.segments || [];
  const total = segs.length;
  const pages = Math.max(1, Math.ceil(total / PAGE_SIZE));
  if (state.segPage > pages) state.segPage = pages;
  if (state.segPage < 1) state.segPage = 1;

  $id('segSummary').textContent = total ? `共 ${total} 段 · 每页 ${PAGE_SIZE}` : '';
  $id('pageInfo').textContent = `${state.segPage} / ${pages}`;
  $id('btnPageFirst').disabled = $id('btnPagePrev').disabled = state.segPage <= 1;
  $id('btnPageNext').disabled = $id('btnPageLast').disabled = state.segPage >= pages;

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
}

function buildSegRow(s) {
  const tr = document.createElement('tr');
  tr.className = 'seg-row' + (s.flagged ? ' flagged' : '');
  tr.dataset.index = String(s.index);

  // 时间码
  const tdTc = document.createElement('td');
  tdTc.className = 'col-tc mono';
  const t1 = document.createElement('span');
  t1.textContent = fmtTC(s.start);
  const arrow = document.createElement('span');
  arrow.className = 'tc-arrow';
  arrow.textContent = '→';
  const t2 = document.createElement('span');
  t2.textContent = fmtTC(s.end);
  tdTc.append(t1, arrow, t2);

  // 原文（flagged 加 ⚠）
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
  origText.textContent = s.text || '';
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

  tr.append(tdTc, tdOrig, tdTrans);
  return tr;
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

function flushDirtyEdit() {
  const active = document.activeElement;
  if (active && active.classList && active.classList.contains('trans-input')) {
    maybeSave(active);
    active.blur();
  }
}

/* ---------------- 上传 ---------------- */
function setupUpload() {
  const dz = $id('dropzone');
  const fi = $id('fileInput');

  dz.addEventListener('click', () => fi.click());
  dz.addEventListener('keydown', (e) => {
    if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); fi.click(); }
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
    const files = e.dataTransfer ? [...e.dataTransfer.files] : [];
    handleFiles(files);
  });
  fi.addEventListener('change', () => {
    handleFiles([...fi.files]);
    fi.value = '';
  });

  $id('btnAddLocal').addEventListener('click', addLocalPaths);
  $id('btnAddUrls').addEventListener('click', addUrlTasks);
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
    if (!window.confirm(`确定删除任务「${(t && t.name) || id}」及其产物目录吗？`)) return;
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
}

function setupSegmentsUI() {
  const body = $id('segBody');

  body.addEventListener('input', (e) => {
    if (e.target.classList && e.target.classList.contains('trans-input')) {
      e.target.classList.add('dirty');
    }
  });
  body.addEventListener('focusout', (e) => {
    if (e.target && e.target.classList && e.target.classList.contains('trans-input')) {
      setTimeout(() => maybeSave(e.target), 120);
    }
  });
  body.addEventListener('keydown', (e) => {
    if (e.target.classList && e.target.classList.contains('trans-input') && e.key === 'Enter') {
      e.preventDefault();
      e.target.blur();
    }
  });
  body.addEventListener('click', (e) => {
    const btn = e.target.closest('.btn-retrans');
    if (btn) retranslateRow(btn);
  });

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
  state.hasApiKey = !!(cfg
    ? (cfg.has_api_key ?? (cfg.translate && cfg.translate.has_api_key))
    : false);

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
    $id('cfgKeyState').textContent = state.hasApiKey ? '已保存 API Key（输入框留空即沿用）' : '尚未保存 API Key';
    $id('cfgApiKey').value = '';
    $id('cfgApiKey').placeholder = state.hasApiKey ? '已保存则留空或 ***' : 'sk-…';
    toast('设置已保存', 'success');
    refreshModelChip(); // 默认模型可能已变，同步顶部 chip
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

function setupSettings() {
  $id('btnSettings').addEventListener('click', openSettings);
  $id('btnCloseSettings').addEventListener('click', closeSettings);
  $id('btnCancelSettings').addEventListener('click', closeSettings);
  $id('btnSaveSettings').addEventListener('click', saveSettings);
  $id('btnTestTranslate').addEventListener('click', testTranslate);
  $id('btnModelsRefresh').addEventListener('click', () => loadModels());

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
function init() {
  setupUpload();
  setupDetailActions();
  setupSegmentsUI();
  setupSettings();

  showListSkeleton();  // 首次任务列表加载前展示骨架
  refreshHealth();
  refreshModelChip();
  loadTasks();
  setInterval(() => { loadTasks(); }, POLL_MS);
  setInterval(() => { refreshHealth(); }, HEALTH_MS);

  updateListChrome();
}

init();
