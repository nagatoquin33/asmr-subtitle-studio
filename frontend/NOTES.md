# 前端实现备注（假设与取舍）

仅记录超出 `docs/API.md` 字面契约的假设与实现决策。文件：`index.html` / `style.css` / `api.js` / `app.js`。

## 2026-10 「深夜录音棚 / 深夜广播」重设计

> 功能逻辑零改动：全部 element ID、JS 类名钩子、事件逻辑保持兼容；style.css 全量重写；index.html 仅调整装饰性结构；app.js / api.js 仅做下述增量小改。

### 设计令牌（style.css `:root`）

1. **色彩**：夜色基底 `#0b0d12 → #10141c → #090b10` 线性渐层 + 左上琥珀 / 右上冷青两处极低饱和氛围光（radial ≤7% 透明度）。双强调色 **暖琥珀 `#f5b56b`**（主按钮、进度条、focus 环、选中卡、进行态）× **冷青 `#67e8f9`**（完成/信息态、健康灯 ok、链接、百分比、chip 边框），替换原紫色系；红色仅用于错误，绿色并入青色（success = 完成 = 青）。
2. **状态色重调（10 态）**：queued `#97a1b6` / downloading `#64a9f2` / extracting `#7dd3fc` / asr `#f5b56b`（核心暖段）/ translating `#5eead4` / proofreading `#f8cd5e` / writing `#f2a8bd` / done `#67e8f9` / error `#f87171` / canceled `#7c8595`——冷蓝技术段 → 琥珀识别段 → 青完成段的「管线旅程」映射。
3. **字体（全离线）**：标题/数字 `"Bahnschrift", "Segoe UI Variable Display"`（DIN 风格，Win10/11 自带，`letter-spacing 2~3px` 营造设备铭牌感）；正文 `"Segoe UI", "Microsoft YaHei"`；时间码/技术值 `ui-monospace, "Cascadia Code"`。
4. **布局**：左栏 340px（原 360px）、顶栏 52px（原 56px）、右栏卡片 padding 16-20px、详情 section 间距 16px；详情各 section 统一 `.sec-head`（小字距标签 + 渐隐细线）。
5. **噪点**：内联 SVG `feTurbulence`（fractalNoise + 去饱和）data-URI 平铺 180px，`body::after` 全屏 opacity 0.03，`pointer-events:none`、z-index 9999（覆盖含弹窗的一切表面，胶片颗粒统一）。
6. **动效（ASMR 式的慢）**：全局过渡 200/320/400ms `cubic-bezier(.22,.61,.36,1)` ease-out；进度条流光 3.2s；状态点/健康灯/运行中卡片左缘条统一 2.4s 呼吸（opacity，非 scale 抖动）；顶栏底部 1px 呼吸光晕线 7s；任务卡载入 `cardIn` 420ms + 60ms 间隔 staggered（nth-child 1~10）；页面载入骨架 shimmer 1.8s；`@media (prefers-reduced-motion: reduce)` 全部关停。

### 视觉实现要点

- **上传卡**：虚线大框 → 内部层次卡片（顶部琥珀辉光 + 圆形耳机 SVG 图标带柔光、Bahnschrift 标语「丢进来，或是粘贴一个链接」）；拖拽悬停边框流光 = `mask-composite: exclude` 描边环 + 110° 扫光（2.2s loop，200ms 淡入）；两个折叠面板（链接下载 / 本地路径）字号 12px、faint 色、次级弱化，并调换顺序让「链接下载」更靠前。
- **任务卡**：更扁更密（10-11px padding、8px 间距）；状态徽章去药丸底色 → 极简圆点+文字；进度条 3px；运行中卡片左缘 2px 呼吸色条用 **纯 CSS `:has(.st-*)`** 驱动（JS 零改动；不支持 `:has` 的旧浏览器仅丢失该装饰）。
- **详情页**：标题 Bahnschrift 17px；导出 6 格改为胶囊分段样式（`width:max-content` + 分隔线 + overflow-x 滚动兜底）；字幕表表头 10.5px/1.8px 字距、行 hover 提亮、时间码 `rgba(125,205,220,.78)` 青灰、译文输入无边框 + focus 底部琥珀线（dirty 虚线琥珀 / saved 青）。
- **设置弹窗**：`fieldset/legend` → `section + .cfg-legend`（琥珀小竖标 + 小型大写字 + 细线）；输入框 8px 圆角 + focus 琥珀环；三个 checkbox 重绘为纯 CSS pill switch（`appearance:none` + radial-gradient 圆点 + `background-position` 平移，选中态琥珀）。
- **空状态**：emoji 替换为纯 CSS waveform（21 根条，nth-child 有机高度 + 错峰 4.2s 极缓明暗呼吸）；任务队列为琥珀渐变、详情空态为冷青变体。

### app.js / api.js 增量小改（功能兼容）

1. **任务卡相对时间**：新增 `parseLocalDate()` / `relTime()`（本地时区解析，「刚刚 / N 分钟前 / N 小时前 / N 天前」，>30 天回退 `MM-DD HH:mm`）；`updateCardDOM` 的 meta 行改显相对时间，`c.meta.title` = 绝对时间（hover 显示）。3s 轮询会持续刷新相对时间。
2. **骨架屏**：新增 `showListSkeleton()` / `clearListSkeleton()` / `showSegSkeleton()`；`init` 在首次 `loadTasks` 前渲染 4 张列表骨架卡，首次响应（成功或失败）即撤除；`state.bootstrapped` 期间 `updateListChrome` 不显示空状态；`refreshSegments` 用 8 行字幕表骨架替代「字幕加载中…」文案，失败时清空骨架并回退错误文案。
3. **顶部模型名 chip**：新增 `#modelChip`（HTML 新增元素）+ `refreshModelChip()`（`GET /api/config` 读 `asr.model`，静默失败），`init` 与 `saveSettings` 成功后各刷新一次。
4. **api.js**：`getConfig` 增加 `silent = false` 可选参数（默认行为不变），供模型 chip 后台静默读取，避免后端离线时启动弹 toast。

### HTML 结构调整（ID / 钩子全部保留）

- 新增：`#modelChip`、`.waveform`（空态装饰）、`.sec-rule`/`.cfg-rule`（细分线）、`.cfg-legend`。`fieldset/legend` 改为 `section/div`（无 JS 引用）。
- 删除：`.es-emoji/.es-glow/.de-emoji/.de-glow`（由 CSS `::before` 辉光与 waveform 取代）。
- 保持不变：全部 68+ 原有 element ID、`accept` 属性、表单 option 值、`aria-*`、`details/summary` 结构（JS 依赖 `.open`）、dropzone/fileInput/exportRow/segBody 等钩子。
- `.badge-text` 无专属 CSS 规则：与原版一致，颜色/字号由 `.badge` 继承。

### 重设计后自检结果

- `node --check app.js` / `api.js`：通过。
- `python -m http.server 8932`：index.html / style.css / app.js / api.js 均 HTTP 200（已验证后关闭并确认端口释放）。
- ID 交叉核对（python 脚本）：app.js `$id()` 引用 81 个 id，全部存在于 index.html（缺失列表为空；其中 1 个为本次新增的 `modelChip`）。
- JS 生成类名 → CSS 覆盖核对：81 个类名全部有样式（badge-text 经 `.badge` 继承）。
- 外链资源检查：HTML 无 `http(s)://` 引用，CSS `url()` 全部为 data-URI，零外部资源。
- 浏览器渲染冒烟测试因当前环境无浏览器能力而跳过。

## 2026-11 v2.1「顺手一点」易用性增强

> 目标：不改任务管线语义，只把「找、选、改、导」变顺。新增 `ux.css`（在 `style.css` 之后加载，仅增量覆盖），`app.js` / `index.html` / `api.js` 增量修改。

### 结构增量

- 顶栏：`#btnShortcuts`（快捷键面板）。
- 就绪检查条 `#setupBanner`（`#sbTitle` / `#sbList` / `#sbActions` / `#btnSetupClose`）。
- 队列：`#queueTools`（`#queueSearch` / `#btnQueueClear` / `#queueSort` / `#btnBatchMode`）、`#queueFilters`（4 个 `.qt-chip`，计数 `#fAll/#fActive/#fDone/#fFailed`）、`#batchBar`（`#batchAll` / `#batchCount` / 批量重试·取消·删除）、`#queueFoot`（`#btnLoadMore` / `#queueTotal`）。
- 空状态：`#esTitle` / `#esHint` 改为动态，新增 `#esActions`（选择文件 / 粘贴链接 / 配置 API）。
- 任务卡：`.tc-check`、`.tc-more`、`.tc-menu`（动态生成）。
- 导出：`#btnCopySub` / `#btnExportAll` / `.export-hint`。
- 字幕：`#segSearch` / `#segSearchInfo` / `#btnSegSearchPrev|Next`、`#segTimeTools`（`#shiftInput` / `#btnShiftApply` / `#btnRetransPage`）、表头「操作」列 `.col-ops`。
- 设置：`#cfgProvider` + `#providerNote`、`#btnExportConfig` / `#btnImportConfig` / `#btnOpenDataDir` / `#cfgDataInfo`；模型行新增「删除」与「默认」标记。
- 弹窗：`#shortcutOverlay`（快捷键）、`#askOverlay`（数值输入，`askValue()` Promise 封装）。

### 行为决策 / 假设

1. **筛选与排序纯前端**：`GET list_tasks` 只按 `limit/offset` 取回，筛选（状态/关键字）、排序（创建/更新/名称/时长）都在 `visibleTasks()` 里做；因此状态筛选只管已加载的页，超出部分用「加载更多」。
2. **`total` 兼容**：旧后端（Python FastAPI）不返回 `total` 时回退 `tasks.length`；只有 `list.length < limit || total <= limit` 才清理本地缓存（避免误删未加载的任务）。
3. **批量删除**会连带删除产物目录（与单任务删除一致），批量重试/取消只作用于状态合适的任务，最终以 toast 汇总成功/失败数。
4. **原文编辑**走 `patch_segment_fields(text)`（桌面版专用）；浏览器模式隐藏该能力（双击无响应，`title` 置空）。时间码/合并/拆分/整轨平移/批量重翻同样只对桌面版开放；桌面版不可用时用 `desktop/*` 类 + `init` 里显式 `hidden`。
5. **合并/拆分/平移/批量重翻**返回全量 segments，前端整体替换本地缓存（`applySegmentsFromServer`），再重算搜索命中与页数，不做局部 diff。
6. **拆分默认值**：若播放器当前时间落在该句内则用它，否则取中点；用 `askValue()` 弹窗可改，校验 `start < at < end-0.05`。
7. **时间轴平移**：`#shiftInput` 支持负数（提前）；0 或空值直接 toast 拒绝，不改数据。
8. **重翻本页**：取当前页全部句子的 index 调 `retranslate_batch`，按钮上做 0.9s 假进度（真实进度由后端整体返回），完成后 toast 报失败句数。
9. **播放跟随**：`timeupdate/seeked` 时按时间找当前句，翻页 + `setActiveSeg(scroll)`；只在字幕已加载（`segmentsFor === selectedId`）时生效。
10. **键盘作用域**：↑↓/Space/←→ 仅在焦点位于字幕表或播放器区域内拦截（避免抢走队列滚动）；卡片被聚焦时 ↑↓ 切换选中任务。
11. **偏好持久化**：`localStorage['asmr.prefs.v1']` 存语言、排序、筛选、上次任务 id；`lastTaskId` 在删除任务时清空。
12. **复制文本**：优先 `navigator.clipboard.writeText`（900ms 超时兜底），失败回退 `execCommand('copy')`；WebView 权限拒绝时给出失败 toast，不阻塞 UI。
13. **导出一键全部**：桌面版调 `export_all_outputs`（原生文件夹选择框，一次写 6 个文件）；浏览器模式隐藏该按钮。
14. **服务商预设**只填 `base_url` 与 `model`（不改已保存的 Key），并在 `#providerNote` 写出该服务商的注意事项。
15. **配置导出**剔除 `translate.api_key`；**导入**只填表单不自动保存，需再点「保存设置」。
16. **模型删除**仅对 `ready` 状态且桌面版显示；删除后刷新模型列表与就绪检查。
17. **未配置 Key 拦截**：添加任务前（拖拽/文件夹/本地路径/链接）统一走 `ensureReadyForTasks()`，Mock 模式或已存 Key 时直接放行；选择「取消」会打开设置页。
18. **就绪检查条**：健康状态、`config`（Key/Mock/默认模型）与模型列表三者合成；手动关闭仅本次运行有效（不写偏好）。

### 自检

- `node --check app.js` / `api.js` 通过。
- 元素 ID 交叉核对：app.js 引用 139 个 id，全部存在于 index.html。
- 桌面版实机（WebView2 + CDP）：初始态、详情、搜索命中/高亮、+1s/-1s 平移、合并（4→3 段）、拆分（默认取中点）、原文双击修改与还原、批量选择、设置弹窗与预设、快捷键面板、Ctrl+K/Ctrl+F、↑↓ 选句、Space 播放、筛选空态、任务卡菜单 —— 均通过，无 JS 异常。
- 浏览器模式（Python 后端）仅做静态检查：桌面专用控件隐藏，其余逻辑走原有 REST 路径。

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

