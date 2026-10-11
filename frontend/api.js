'use strict';

/* =====================================================
 * ASMR 字幕工作室 — API 封装
 * 严格遵守 docs/API.md 契约（FastAPI @ http://127.0.0.1:8765，前缀 /api）
 * 前端由后端在 / 直接托管，因此全部使用相对路径。
 *
 * 统一错误处理：
 *  - 网络 error / 非 2xx → 弹 toast（可通过 silent 关闭，用于后台轮询）
 *  - 失败时 reject，调用方可用 try/catch 做兜底
 * ===================================================== */

class ApiError extends Error {
  constructor(message, status) {
    super(message);
    this.name = 'ApiError';
    this.status = status;
  }
}

/**
 * 通用请求封装。
 * @param {string} path 相对路径（/api/...）
 * @param {{method?:string, body?:any, silent?:boolean}} [opts]
 * @returns {Promise<any>} 解析后的 JSON（非 JSON 响应返回文本）
 */
async function apiFetch(path, opts = {}) {
  const { method = 'GET', body, silent = false } = opts;

  /** @type {Response} */
  let res;
  try {
    res = await fetch(path, {
      method,
      headers: body !== undefined ? { 'Content-Type': 'application/json' } : undefined,
      body: body !== undefined ? JSON.stringify(body) : undefined,
    });
  } catch (err) {
    if (!silent) toast(`网络错误：无法连接后端（${path}），请确认服务已启动`, 'error');
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

const API = {
  /* ---- 健康 ---- */
  health: () => apiFetch('/api/health', { silent: true }),

  /* ---- 配置 ---- */
  // silent=true 供顶部模型名 chip 等后台读取使用（失败不打扰用户）；表单加载保持默认弹错
  getConfig: (silent = false) => apiFetch('/api/config', { silent }),
  saveConfig: (cfg) => apiFetch('/api/config', { method: 'POST', body: cfg }),

  /* ---- 翻译连通性测试 ---- */
  testTranslate: (payload) => apiFetch('/api/translate/test', { method: 'POST', body: payload }),

  /* ---- 模型管理 ---- */
  listModels: (silent = false) => apiFetch('/api/models', { silent }),
  downloadModel: (name) => apiFetch('/api/models/download', { method: 'POST', body: { name } }),

  /* ---- 任务 ---- */
  listTasks: (limit = 100) => apiFetch(`/api/tasks?limit=${limit}`, { silent: true }),
  // 单任务获取默认静默：多由轮询/SSE 触发，失败通过 UI 状态呈现，避免 toast 刷屏
  getTask: (id, withSegments = false) =>
    apiFetch(`/api/tasks/${encodeURIComponent(id)}${withSegments ? '?segments=1' : ''}`, { silent: true }),
  deleteTask: (id) => apiFetch(`/api/tasks/${encodeURIComponent(id)}`, { method: 'DELETE' }),
  cancelTask: (id) => apiFetch(`/api/tasks/${encodeURIComponent(id)}/cancel`, { method: 'POST' }),
  retryTask: (id) => apiFetch(`/api/tasks/${encodeURIComponent(id)}/retry`, { method: 'POST' }),
  openFolder: (id) => apiFetch(`/api/tasks/${encodeURIComponent(id)}/open_folder`, { method: 'POST' }),
  patchSegment: (id, index, translated) =>
    apiFetch(`/api/tasks/${encodeURIComponent(id)}/segments/${index}`, { method: 'PATCH', body: { translated } }),
  retranslateSegment: (id, index) =>
    apiFetch(`/api/tasks/${encodeURIComponent(id)}/segments/${index}/retranslate`, { method: 'POST' }),
  addLocalTasks: (paths, sourceLang, targetLang) =>
    apiFetch('/api/tasks/local', { method: 'POST', body: { paths, source_lang: sourceLang, target_lang: targetLang } }),
  // 返回 { tasks: [Task, ...], invalid: [无效URL, ...] }，由后端负责逐条校验
  addUrlTasks: (urls, sourceLang, targetLang) =>
    apiFetch('/api/tasks/url', { method: 'POST', body: { urls, source_lang: sourceLang, target_lang: targetLang } }),
  // SSE 事件流由 EventSource 直连，见 app.js
  // 媒体 / 下载由 <video>/<audio>/<a href> 直连：
  //   GET /api/media/{id}
  //   GET /api/tasks/{id}/download?type=orig_srt|trans_srt|bilingual_srt|bilingual_ass|bilingual_vtt|json
};
