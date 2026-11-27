'use strict';

/* =====================================================
 * ASMR 字幕工作室 — API 封装（双模式）
 *
 * 桌面模式（Tauri v2 WebView2）：
 *  - 所有请求走 window.__TAURI__.core.invoke（IPC，无本地 HTTP 服务、无端口）
 *  - 任务实时事件走 listen('task-events')（对应原 SSE）
 *  - 媒体流/字幕轨走自定义协议 http://asmrmedia.localhost/...（支持 Range）
 *  - 文件选取/拖拽直接给本地路径（零拷贝，不再上传字节）
 *
 * 浏览器模式（开发兜底，连 Python 版 FastAPI）：
 *  - 保持原 fetch('/api/...') 语义不变
 * ===================================================== */

const TAURI = typeof window !== 'undefined' && !!window.__TAURI__;

class ApiError extends Error {
  constructor(message, status) {
    super(message);
    this.name = 'ApiError';
    this.status = status;
  }
}

/** 浏览器模式（Python 后端）下不支持的桌面功能：统一拒绝并提示，不弹多余的 toast。 */
function unsupported(what) {
  return Promise.reject(new ApiError(`${what}仅桌面版支持`, 0));
}

/* ---------------- 桌面 IPC 请求 ---------------- */

const _invoke = TAURI ? window.__TAURI__.core.invoke : null;

/**
 * 桌面模式：invoke 封装（错误统一 toast，可 silent）。
 * @param {string} cmd 命令名
 * @param {object} args 参数
 * @param {{silent?:boolean}} [opts]
 */
async function ipcCall(cmd, args = {}, opts = {}) {
  try {
    return await _invoke(cmd, args);
  } catch (err) {
    const msg = typeof err === 'string' ? err : (err && err.message) || String(err);
    if (!opts.silent) toast(msg || '请求失败', 'error');
    throw new ApiError(msg, 0);
  }
}

/* ---------------- 浏览器 fetch（开发兜底） ---------------- */

/**
 * 通用请求封装（浏览器模式）。
 * @param {string} path 相对路径（/api/...）
 * @param {{method?:string, body?:any, silent?:boolean}} [opts]
 */
async function apiFetch(path, opts = {}) {
  const { method = 'GET', body, silent = false } = opts;
  let res;
  try {
    res = await fetch(path, {
      method,
      headers: body !== undefined ? { 'Content-Type': 'application/json' } : undefined,
      body: body !== undefined ? JSON.stringify(body) : undefined,
    });
  } catch (err) {
    if (!silent) toast('网络错误：无法连接后端（' + path + '），请确认服务已启动', 'error');
    throw err;
  }
  if (!res.ok) {
    let msg = `请求失败（HTTP ${res.status}）`;
    try {
      const data = await res.json();
      if (data && typeof data.detail === 'string') msg = data.detail;
      else if (data && typeof data.message === 'string') msg = data.message;
    } catch (_) { /* 响应体不是 JSON，保留默认消息 */ }
    if (!silent) toast(msg, 'error');
    throw new ApiError(msg, res.status);
  }
  const ct = res.headers.get('content-type') || '';
  if (ct.includes('application/json')) return res.json();
  return res.text();
}

/* ---------------- 对外 API（双模式同形） ---------------- */

const API = {
  /* ---- 运行环境 ---- */
  isDesktop: () => TAURI,

  /* ---- 健康 ---- */
  health: () => (TAURI ? ipcCall('health', {}, { silent: true }) : apiFetch('/api/health', { silent: true })),

  /* ---- 配置 ---- */
  getConfig: (silent = false) => (TAURI ? ipcCall('get_config', {}, { silent }) : apiFetch('/api/config', { silent })),
  saveConfig: (cfg) => (TAURI ? ipcCall('save_config', { patch: cfg }) : apiFetch('/api/config', { method: 'POST', body: cfg })),

  /* ---- 翻译连通性测试 ---- */
  testTranslate: (payload) => (TAURI ? ipcCall('test_translate', payload) : apiFetch('/api/translate/test', { method: 'POST', body: payload })),

  /* ---- 模型管理 ---- */
  listModels: (silent = false) => (TAURI ? ipcCall('list_models', {}, { silent }) : apiFetch('/api/models', { silent })),
  downloadModel: (name) => (TAURI ? ipcCall('download_model', { name }) : apiFetch('/api/models/download', { method: 'POST', body: { name } })),

  /* ---- 任务 ---- */
  listTasks: (limit = 100, offset = 0) => (TAURI ? ipcCall('list_tasks', { limit, offset }) : apiFetch(`/api/tasks?limit=${limit}`, { silent: true })),
  getTask: (id, withSegments = false) => (TAURI
    ? ipcCall('get_task', { taskId: id, segments: withSegments }, { silent: true })
    : apiFetch(`/api/tasks/${encodeURIComponent(id)}${withSegments ? '?segments=1' : ''}`, { silent: true })),
  deleteTask: (id) => (TAURI ? ipcCall('delete_task', { taskId: id }) : apiFetch(`/api/tasks/${encodeURIComponent(id)}`, { method: 'DELETE' })),
  cancelTask: (id) => (TAURI ? ipcCall('cancel_task', { taskId: id }) : apiFetch(`/api/tasks/${encodeURIComponent(id)}/cancel`, { method: 'POST' })),
  retryTask: (id) => (TAURI ? ipcCall('retry_task', { taskId: id }) : apiFetch(`/api/tasks/${encodeURIComponent(id)}/retry`, { method: 'POST' })),
  openFolder: (id) => (TAURI ? ipcCall('open_folder', { taskId: id }) : apiFetch(`/api/tasks/${encodeURIComponent(id)}/open_folder`, { method: 'POST' })),
  patchSegment: (id, index, translated) => (TAURI
    ? ipcCall('patch_segment', { taskId: id, index, translated })
    : apiFetch(`/api/tasks/${encodeURIComponent(id)}/segments/${index}`, { method: 'PATCH', body: { translated } })),
  retranslateSegment: (id, index) => (TAURI
    ? ipcCall('retranslate_segment', { taskId: id, index })
    : apiFetch(`/api/tasks/${encodeURIComponent(id)}/segments/${index}/retranslate`, { method: 'POST' })),

  /* ---- 字幕段增强（桌面版专用：原文/时间码编辑、时间轴、合并拆分、批量重翻） ---- */
  patchSegmentFields: (id, index, fields = {}) => {
    if (!TAURI) return unsupported('编辑原文/时间码');
    // 显式补 null：Tauri 的 Option<T> 参数用 null 表示「不改」
    return ipcCall('patch_segment_fields', {
      taskId: id,
      index,
      text: fields.text ?? null,
      translated: fields.translated ?? null,
      start: fields.start ?? null,
      end: fields.end ?? null,
    });
  },
  shiftSegments: (id, offset) => {
    if (!TAURI) return unsupported('时间轴整体平移');
    return ipcCall('shift_segments', { taskId: id, offset });
  },
  mergeSegments: (id, index) => {
    if (!TAURI) return unsupported('合并字幕');
    return ipcCall('merge_segments', { taskId: id, index });
  },
  splitSegment: (id, index, at) => {
    if (!TAURI) return unsupported('拆分字幕');
    return ipcCall('split_segment', { taskId: id, index, at });
  },
  retranslateBatch: (id, indices) => {
    if (!TAURI) return unsupported('批量重翻');
    return ipcCall('retranslate_batch', { taskId: id, indices });
  },

  /* ---- 桌面版：应用目录 / 定位文件 / 模型删除 / 烧录取消 ---- */
  appPaths: () => (TAURI ? ipcCall('app_paths', {}, { silent: true }) : Promise.resolve(null)),
  revealPath: (path) => (TAURI ? ipcCall('reveal_path', { path }) : Promise.reject(new Error('仅桌面版支持'))),
  deleteModel: (name) => {
    if (!TAURI) return unsupported('删除模型');
    return ipcCall('delete_model', { name });
  },
  cancelBurn: (id) => (TAURI ? ipcCall('cancel_burn', { taskId: id }, { silent: true }) : Promise.resolve(null)),
  addLocalTasks: (paths, sourceLang, targetLang) => (TAURI
    ? ipcCall('create_local_tasks', { paths, sourceLang, targetLang })
    : apiFetch('/api/tasks/local', { method: 'POST', body: { paths, source_lang: sourceLang, target_lang: targetLang } })),
  addUrlTasks: (urls, sourceLang, targetLang) => (TAURI
    ? ipcCall('create_url_tasks', { urls, sourceLang, targetLang })
    : apiFetch('/api/tasks/url', { method: 'POST', body: { urls, source_lang: sourceLang, target_lang: targetLang } })),

  /* ---- 媒体 / 字幕轨 / 导出（桌面模式专用形态） ---- */
  mediaUrl: (id) => (TAURI ? `http://asmrmedia.localhost/${encodeURIComponent(id)}/media` : `/api/media/${encodeURIComponent(id)}`),
  vttUrl: (id) => (TAURI ? `http://asmrmedia.localhost/${encodeURIComponent(id)}/vtt` : `/api/tasks/${encodeURIComponent(id)}/download?type=bilingual_vtt`),
  exportOutput: (id, type, suggestedName) => (TAURI
    ? ipcCall('export_output', { taskId: id, kind: type, suggestedName })
    : Promise.resolve({ ok: false, path: `/api/tasks/${encodeURIComponent(id)}/download?type=${type}` })),

  /** 一键导出全部 6 种字幕到用户选择的文件夹（桌面版）。 */
  exportAll: (id) => {
    if (!TAURI) return unsupported('导出全部');
    return ipcCall('export_all_outputs', { taskId: id });
  },

  /* ---- 成品视频导出（硬字幕烧录 / 软字幕 mkv，桌面模式专用） ---- */
  exportBurned: (id, mode) => ipcCall('export_burned_video', { taskId: id, mode }),

  /* ---- 实时事件（桌面：全局事件；浏览器：每任务 SSE，由 app.js 处理） ---- */
  onTaskEvents: (handler) => {
    if (!TAURI) return null;
    const unlistenP = window.__TAURI__.event.listen('task-events', (ev) => handler(ev.payload));
    return () => { unlistenP.then((un) => un()); };
  },

  /* ---- 成品视频渲染进度（桌面模式） ---- */
  onBurnEvents: (handler) => {
    if (!TAURI) return null;
    const unlistenP = window.__TAURI__.event.listen('burn-events', (ev) => handler(ev.payload));
    return () => { unlistenP.then((un) => un()); };
  },

  /* ---- 桌面文件选择 / 确认框 ---- */
  pickMediaFiles: async () => {
    const filters = [{
      name: '音视频',
      extensions: ['mp3', 'wav', 'flac', 'm4a', 'aac', 'ogg', 'oga', 'opus', 'wma', 'aiff', 'aif', 'ape',
        'mp4', 'mkv', 'mov', 'avi', 'webm', 'flv', 'ts', 'm4v', 'wmv', 'mpg', 'mpeg', '3gp'],
    }];
    const picked = await window.__TAURI__.dialog.open({ multiple: true, directory: false, filters });
    if (!picked) return [];
    return Array.isArray(picked) ? picked : [picked];
  },
  pickFolder: async () => {
    const picked = await window.__TAURI__.dialog.open({ directory: true, multiple: false, title: '选择要批量导入的文件夹' });
    return picked || null;
  },
  confirmBox: async (message) => {
    if (!TAURI) return window.confirm(message);
    const ok = await window.__TAURI__.dialog.ask(message, { title: '确认', kind: 'warning' });
    return !!ok;
  },
};
