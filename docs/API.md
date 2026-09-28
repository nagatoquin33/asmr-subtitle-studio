# ASMR 字幕工作室 — API 契约 v1

本地全流程工具：音视频 → ffmpeg 抽音频 → 本地 ASR（faster-whisper，模型从魔搭下载，支持中/英/日）→ API 翻译 → API 审校 → 输出 SRT/ASS/VTT/JSON 字幕。

- 后端：FastAPI，`http://127.0.0.1:8765`，所有接口前缀 `/api`
- 前端：`frontend/` 静态文件由后端在 `/` 直接托管（单页应用，无构建步骤，完全离线，禁止引用任何 CDN/外网资源）
- 所有请求/响应 JSON，UTF-8。时间戳均为秒（float）。

## 数据结构

### 任务对象 Task

```json
{
  "id": "t_20260928_153000_a1b2c3",
  "name": "video.mp4",
  "status": "asr",
  "stage": "asr",
  "progress": 0.42,
  "message": "识别中 00:12:30 / 01:00:00",
  "error": null,
  "created_at": "2026-09-28T15:30:00",
  "updated_at": "2026-09-28T15:31:20",
  "media": {"path": "D:/...", "duration": 3623.4, "size": 734003200, "has_video": true},
  "asr": {"backend": "faster-whisper", "model": "small", "language": "ja"},
  "options": {"source_lang": "auto", "target_lang": "zh"},
  "counts": {"segments": 512, "translated": 200},
  "has_result": false
}
```

`status` / `stage` 取值：`queued → downloading → extracting → asr → translating → proofreading → writing → done`；失败 `error`；取消 `canceled`。仅 `downloading` 阶段属于 URL 下载任务。

任务对象额外字段：`source_url`（string，URL 下载任务才有）——原视频页面链接。

### 段对象 Segment

```json
{"index": 0, "start": 1.2, "end": 4.8, "text": "こんばんは、おやすみ前の…", "translated": "晚上好，在睡前……", "flagged": false}
```

`flagged=true` 表示疑似 ASR 幻觉（广告/字幕组致谢等）或翻译对齐失败，前端高亮提示。

## 接口

### 配置

- `GET /api/config` → 当前配置（`api_key` 永远返回 `"***"`，另带 `has_api_key: true/false`）
- `POST /api/config` body 为下面结构的任意子集，合并保存后返回完整配置（同样脱敏）

```json
{
  "asr": {"backend": "faster-whisper", "model": "small", "device": "auto", "compute_type": "auto"},
  "translate": {
    "base_url": "https://api.deepseek.com/v1",
    "api_key": "",
    "model": "deepseek-chat",
    "temperature": 0.2,
    "concurrency": 3,
    "batch_size": 40,
    "proofread": true,
    "mock": false,
    "max_retries": 3,
    "glossary": {"囁き": "轻声耳语", "耳かき": "掏耳朵", "タッピング": "敲击音", "トリガー": "触发音"}
  },
  "output": {"bilingual": true, "bilingual_order": "trans_first", "max_line_chars": 42},
  "download": {"height": 720, "proxy": "", "cookies_from_browser": ""},
  "pipeline": {"workers": 1, "keep_audio": false}
}
```

- `asr.model` 可选：`small` / `medium` / `large-v3` / `large-v3-turbo`
- `asr.device`：`auto` / `cpu` / `cuda`；`compute_type`：`auto` / `int8` / `float16`
- `download`：URL 下载配置。`height` 画质上限（默认 720）；`proxy` 代理（如 `http://127.0.0.1:7890`，空=直连/系统代理）；`cookies_from_browser` 从本机浏览器取 cookies（可选 `chrome`/`edge`/`firefox`，用于需要登录的视频）
- `translate.mock=true` 时不联网，返回确定性伪翻译（用于无 API Key 时测试全流程）

### 翻译连通性测试

- `POST /api/translate/test` body `{"base_url": "...", "api_key": "...", "model": "..."}`（`api_key` 为 `"***"` 时复用已保存的 key）
- 返回 `{"ok": true, "message": "...", "sample": "模型回复文本"}` 或 `{"ok": false, "message": "错误原因"}`

### 模型管理

- `GET /api/models` →
```json
{"models": [{"name": "small", "repos": ["gpustack/faster-whisper-small"], "size_hint": "~480MB", "status": "idle|downloading|ready|error", "progress": 0.0, "local_path": "D:/.../data/models/small", "detail": ""}]}
```
- `POST /api/models/download` body `{"name": "small"}` → 立即返回，后台下载，轮询 `GET /api/models` 看进度（status=downloading 时 progress 0~1）

### 任务

- `POST /api/tasks`（multipart）：`files[]` 多个文件 + 表单字段 `source_lang`（auto/zh/en/ja，默认 auto）、`target_lang`（zh/en/ja，默认 zh）→ `{"tasks": [Task, ...]}`（每个文件一个任务，全部入队）
- `POST /api/tasks/local` body `{"paths": ["D:/a.mp4"], "source_lang": "auto", "target_lang": "zh"}` → 同上（引用本地路径，不复制文件）
- `POST /api/tasks/url` body `{"urls": ["https://www.bilibili.com/video/BV..."], "source_lang": "auto", "target_lang": "zh"}` → `{"tasks": [Task, ...], "invalid": [无效URL]}`。yt-dlp 解析下载（支持 bilibili/YouTube 及其它 yt-dlp 支持平台），任务 name 初始为 URL，下载完成后变为视频标题；`media.path` 下载完成后才有值。下载阶段 message 形如 `下载中 45% (12.3/27.1MB)`。
- `GET /api/tasks?limit=100` → `{"tasks": [Task, ...]}`（不含 segments，按创建时间倒序）
- `GET /api/tasks/{id}?segments=1` → Task + `"segments": [Segment, ...]`（仅 `?segments=1` 时包含）
- `DELETE /api/tasks/{id}` → 删除任务与产物目录；运行中的任务先取消再删
- `POST /api/tasks/{id}/cancel` → 设取消标记，阶段间生效
- `POST /api/tasks/{id}/retry` → 失败/取消的任务重新入队
- `PATCH /api/tasks/{id}/segments/{index}` body `{"translated": "新译文"}` → 保存单句修改并重写字幕文件 → 返回 `{ok: true}`
- `POST /api/tasks/{id}/segments/{index}/retranslate` → 用上下文重翻这一句（走已配置的翻译 API）→ 返回更新后的 Segment
- `GET /api/tasks/{id}/events` → SSE，事件 JSON：`{"type": "progress", "task": Task}` / `{"type": "done", "task": Task}` / `{"type": "error", "task": Task}`（task 为最新任务对象；每 15s 发一次心跳 `{"type": "ping"}`）
- `GET /api/tasks/{id}/download?type=` → 文件下载，type 取值：
  - `orig_srt` 原文 SRT / `trans_srt` 译文 SRT / `bilingual_srt` 双语 SRT / `bilingual_ass` 双语 ASS / `bilingual_vtt` 双语 VTT / `json` segments JSON
- `GET /api/media/{id}` → 原始媒体文件（需支持 HTTP Range，用于 `<video>` 拖动进度）

### 其他

- `GET /api/health` → `{"ok": true, "version": "1.0.0", "ffmpeg": true, "ffmpeg_path": "...", "gpu": {"available": true, "name": "RTX 5070 Ti"}}`

## 翻译模块 Python 契约（backend/translate）

后端管线以下列签名调用，签名不可更改：

```python
# backend/translate/__init__.py 必须导出
from .translator import SubtitleTranslator, Proofreader, test_connection

class SubtitleTranslator:
    def __init__(self, cfg: dict): ...
    async def translate(
        self,
        segments: list[dict],        # [{"index": int, "start": float, "end": float, "text": str}, ...]
        source_lang: str | None,     # "zh"/"en"/"ja"/None(未知)
        target_lang: str,            # "zh"/"en"/"ja"
        progress_cb=None,            # progress_cb(done: int, total: int, message: str)
    ) -> list[str]: ...              # 与 segments 等长、按顺序对齐的译文列表

class Proofreader:
    def __init__(self, cfg: dict): ...
    async def proofread(
        self,
        segments: list[dict],        # 同上（含原文 text）
        translated: list[str],       # 初译结果
        target_lang: str,
        progress_cb=None,
    ) -> list[str]: ...              # 审校后译文，与输入等长对齐

async def test_connection(cfg: dict) -> dict:
    # cfg 含 base_url/api_key/model；返回 {"ok": bool, "message": str, "sample": str}
```

cfg 即配置中 `translate` 节（可能包含额外键，需容忍）。`mock=true` 时全流程离线确定性运行（伪翻译 `[译]{原文}`、审校原样返回、test_connection 返回 ok），且**必须懒加载 `openai`**（仅在非 mock 时 import），保证零依赖环境可跑测试。LLM 客户端用 `openai.AsyncOpenAI`（OpenAI 兼容协议）。解析协议、重试、并发语义见 `docs/` 下说明与 prompt.py 内实现。

## 产物目录

```
{程序所在目录}/
  .env               LLM_API_KEY 存这里（不进 config.json、不进 git）
  data/
    tasks/{id}/      任务工作目录：媒体、audio.wav、segments.json、task.json
    models/          魔搭下载的 ASR 模型
    config.json      配置（不含 api_key）
  output/
    {视频名}/        每个视频的独立产物文件夹
      orig.srt / trans.srt / bilingual.srt / bilingual.ass / bilingual.vtt / segments.json
```

任务对象额外字段：`dir`（任务工作目录绝对路径）、`output_dir`（产物目录绝对路径，写产物后回填）。`outputs` 映射的值为绝对路径。

## 密钥（.env）

- `LLM_API_KEY` 存放在程序目录的 `.env` 文件；`config.json` 永不保存 key。
- `POST /api/config` 传入非空、非 `***` 的 `translate.api_key` 时写入 `.env`；`***`/空表示保留。
- 首次启动检测到 config.json 残留旧 key 时自动迁移到 `.env` 并清除。
