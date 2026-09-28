# 前端实现备注（假设与取舍）

仅记录超出 `docs/API.md` 字面契约的假设与实现决策。文件：`index.html` / `style.css` / `api.js` / `app.js`。

## API 假设

1. **`has_api_key` 的位置**：契约写「另带 has_api_key」但未明说层级。前端按 `cfg.has_api_key`（顶层）读取，并兜底 `cfg.translate.has_api_key`（POST 返回值同理）。
2. **API Key 提交规则**：输入框留空且 `has_api_key=true` → 发送 `"***"`；字面输入 `***` → 原样发送；其余按输入值发送。前端永不回填 key（GET 返回的 `***` 不写入表单）。
3. **错误消息字段**：非 2xx 时优先取响应 JSON 的 `detail`（FastAPI 惯例），其次 `message`，否则 `HTTP <status>`。
4. **`PATCH segments/{index}`**：假定 2xx 即成功（契约返回 `{ok:true}`），未校验 body 内容。
5. **`retranslate`**：假定返回完整 Segment 对象（含 index/text/translated/flagged），前端据此整行替换。
6. **取消/重试**：假定 2xx 即受理，随后单独 `GET /api/tasks/{id}` 拉取最新状态（返回体忽略）。
7. **`POST /api/tasks/local` 与 `/api/tasks`**：假定返回 `{"tasks":[Task,...]}`，按创建时间倒序本地重排，新任务自然出现在列表顶部。
8. **SSE `data` 字段**：假定每条事件 data 为 JSON（`ping` 心跳忽略；`progress/done/error` 均带 `task`）。
9. **`GET /api/tasks?limit=100`**：仅在返回条数 < 100 时清理本地已消失的任务，避免误删第 100 条之外的缓存。
10. **导出**：`<a href download>` 直连 `GET /api/tasks/{id}/download?type=...`，除要求的 5 种外补充了 `bilingual_vtt`（契约支持，且用于字幕轨）。`has_result=false` 时禁用。

## 实现决策

- **SSE 并发上限 6**：浏览器对同源 HTTP/1.1 并发连接约 6 条（每条 SSE 占一条），超出部分仅依赖 3s 轮询刷新；轮询同时负责重建被判死的 EventSource。若后端上 HTTP/2 可放宽。
- **轮询/健康检查静默失败**：后台 3s 任务轮询、30s 健康检查、模型进度轮询、单任务拉取均 `silent`（不弹 toast），失败状态由顶部健康灯与 UI 占位文案呈现，避免后端离线时 toast 刷屏。用户主动操作（上传/保存/删除等）失败必弹 toast。
- **进度条**：done → 100% 绿色渐变；error → 红色；canceled → 灰色；其余用主色渐变 + 流光动画。
- **flagged 行**：左侧红色内嵌条 + ⚠（title="疑似 ASR 幻觉"），行内 hover 增强背景。
- **译文编辑**：`<input>` 单行，失焦/回车触发 PATCH（仅变更时），失败还原并弹 toast；`dirty`（黄）/`saving`/`saved`（绿）三态样式。「重翻」前若有未保存修改会先 PATCH 再重翻，避免互相覆盖。
- **分页**：每页 200 条，页码导航（« ‹ 1/N › »），翻页前自动保存焦点行未提交的修改。
- **窄屏 (<1280px)**：详情面板变为全屏覆盖层（Esc / ✕ 关闭）；左栏堆叠其下。
- **本地工具假设**：删除等破坏性操作用原生 `confirm()`；上传用 XHR（fetch 无上传进度）。
- **自检结果**：`node --check` api.js / app.js 均通过；`python -m http.server 8931` 下 index.html / app.js / api.js / style.css 均 HTTP 200（已验证后关闭）；另做了 ID/类名交叉静态检查（app.js 引用的 68 个元素 ID 与全部 JS 侧类名均存在）。浏览器渲染冒烟测试因当前环境无浏览器能力而跳过。

## URL 下载（POST /api/tasks/url）追加假设

11. **链接校验交给后端**：前端只按行拆分并去空，不做协议级预校验；无效链接以响应 `invalid` 数组为准，toast 提示「N 个链接无效」（现有 toast 无 warning 类型，用 error 样式呈现）。仅当 `tasks` 非空才清空输入框并收起面板（无效链接保留在输入框中便于修正）；`tasks` 与 `invalid` 均为空时提示错误。
12. **`downloading` 状态**：徽章/圆点用 sky 蓝 `#38bdf8`（区别于 extracting 的 `#60a5fa` 与 translating 的 `#22d3ee`），圆点带呼吸动画；该状态计入 `ACTIVE_STATUSES`（可取消、开 SSE、占位文案显示进行中）。模型下载徽章（`MODEL_STATUS_META.downloading`）同步复用该配色，原借用翻译中的青色。进度条/百分比/消息复用现有渲染，不区分下载阶段。
13. **来源 chip**：`task.source_url` 存在时置于详情 chips 首位，`<a target="_blank" rel="noopener">`，文案 = 去 `www.` 的 hostname + 任务名（任务名与 URL 相同即尚未拿到视频标题时不拼标题，标题截断 18 字），两者皆缺回退「来源链接」；完整 URL 放在 `title`。
14. **下载配置**：`height` 前端 clamp 144~2160（四舍五入取整）；`cookies_from_browser` 提交前 trim + 转小写（合法值 chrome/edge/firefox/空由后端校验）；`proxy` 仅 trim。

