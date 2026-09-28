# ASMR 字幕工作室

本地全流程 ASMR 音视频字幕工具：**上传/粘贴链接 → 自动下载（bilibili/YouTube 等）→ ffmpeg 抽音频 → 本地 ASR（faster-whisper，模型自动从魔搭下载）→ API 翻译 → API 审校 → 输出双语字幕**。

支持中文 / 英文 / 日文互译，带网页前端：拖拽上传、链接解析下载、实时进度（SSE）、视频预览挂双语字幕轨、逐句编辑 / 单句重翻、多种格式导出。

## 快速开始

**方式一（推荐给日常使用）：打包成 exe**

```bat
build_exe.bat
```

跑一次后得到 `dist\asmr-subtitle-studio\`——双击 `asmr-subtitle-studio.exe` 即可，会以**原生桌面窗口**打开（内嵌 WebView2，不再弹浏览器；窗口关闭即退出，启动出错时控制台会停留显示原因）。整个文件夹可以拷到其他 Windows 电脑直接用（无需 Python）。首次运行如遇 SmartScreen 提示点「仍要运行」。可选参数：`--browser` 强制浏览器、`--no-browser` 纯服务、`--port 8766` 换端口。

**方式二（开发/源码运行）：**

```bat
:: Windows：双击 start.bat 即可（首次自动建 venv 装依赖）
start.bat
```

启动后访问 **http://127.0.0.1:8765**（会自动打开浏览器）。

Git Bash / Linux 用 `./start.sh`。

## 使用流程

1. **设置**（右上角）→「翻译 API」里填你的 OpenAI 兼容接口（默认 DeepSeek：`https://api.deepseek.com/v1` + `deepseek-chat`，也支持 Moonshot/OpenAI/ollama/one-api 等任何兼容端点），保存后点「测试翻译连接」确认。
2. **三种添加方式**：拖入/选择 音视频文件（可多文件）；「本地路径添加」直接引用磁盘上的大文件（不上传复制）；**「链接下载」粘贴 bilibili / YouTube 等平台视频页链接**（yt-dlp 解析，自动下载后接完整流程）。
3. 选好 源语言（默认自动检测）/ 目标语言，任务自动排队执行：下载（URL 任务）→ 抽音频 → 识别 → 翻译 → 审校 → 生成字幕。
4. 点开任务卡片：在线预览（视频可直接挂双语字幕轨播放）、逐句编辑译文、单句「重翻」、导出 **原文/译文/双语 SRT、双语 ASS、双语 VTT、JSON**；详情里有「文件位置」（产物目录 + 一键打开文件夹，下载的视频就在同一目录），失败/取消的任务卡片上直接有「重试」。

## 失败重试与断点续跑

任务失败后点「重试」（卡片或详情里都有），管线会**自动跳过已完成的阶段**：视频已下载→跳过下载、audio.wav 已存在→跳过抽取、识别结果已落盘→跳过识别，直接从失败的阶段继续。典型场景：没配 API Key 时任务会快速失败并明确提示 → 配好 Key 后点重试，几十秒内完成翻译，不用重跑一小时的视频识别。

跳过翻译的策略：只有**手动指定**源语言=目标语言，或自动检测置信度 ≥0.9 且语言相同时才跳过（ASMR 轻声+音乐下语言检测不可靠，宁可多翻一次也不会静默漏翻）；Mock 模式的 `[译]` 前缀伪翻译不会被当作真实译文。

## 链接下载（bilibili / YouTube）

- 基于 yt-dlp，默认下载 **≤720p** 视频并合并为 mp4（画质可在设置里调），仅取音频的平台类型同样支持。
- 设置里有「下载」分组：**代理**（YouTube 等境外平台连不上时填，如 `http://127.0.0.1:7890`）、**浏览器 cookies**（`chrome`/`edge`/`firefox`，用于需要登录/会员的视频）。
- 音乐/MV 类视频若 VAD 把整段音频过滤光导致 0 段，会**自动关闭 VAD 重识别**（设置里 `asr.vad` 可强制 on/off）。
- 歌曲视频曾实测：B 站 MV 下载 → 63/41 段歌词识别全通；下架视频会报 404 等明确错误。

## ASR 模型（从魔搭下载）

首次识别会自动从魔搭（ModelScope）直连下载所选模型到 `data/models/`（纯 HTTP 实现：真实百分比进度、断点续传，无需任何第三方 SDK）。也可在「设置 → ASR 模型下载」里预下载、看进度。

| 模型 | 魔搭仓库 | 体积 | 说明 |
|---|---|---|---|
| small（默认） | gpustack/faster-whisper-small | ~480MB | 快，中英日良好 |
| medium | gpustack/faster-whisper-medium | ~1.5GB | 更准 |
| large-v3 | gpustack/faster-whisper-large-v3（备用 keepitsimple 镜像） | ~3GB | 最准，推荐 GPU |
| large-v3-turbo | pengzhendong/faster-whisper-large-v3-turbo | ~1.6GB | large-v3 加速版，性价比最高 |

GPU：有 NVIDIA 显卡时自动用 CUDA（float16），需要 `nvidia-cublas-cu12` + `nvidia-cudnn-cu12`（requirements 已含，RTX 50 系需 cuBLAS ≥ 12.8）；任何 CUDA 问题自动回退 CPU int8，不会中断任务。

## 配置

所有配置存 `data/config.json`，也可在网页设置里改：ASR 模型/设备、翻译参数（温度/并发/批大小/重试）、术语表（每行 `原文=译文`，强制注入提示词）、审校开关、双语顺序、每行最大字符数等。

- **翻译协议**：逐批（默认 40 句/批）按 `行号|译文` 严格解析，解析不完整自动纠错重试，仍失败按兜底对齐保证句数不变；批间并发可调，带上下文（前 3 句定稿）保持全文一致。
- **审校**：二次 LLM 校对（误译/不通顺/术语不一致/语气），可在设置里关。
- **Mock 模式**：设置里打开后翻译走本地伪翻译，**无 API Key 也能跑通全流程**（测试/演示用）。
- **幻觉标记**：识别结果里疑似 Whisper 幻觉（字幕组致谢、广告词等）的句子会标 ⚠，方便人工复核。

## 目录与密钥

程序运行后会在**程序所在目录**构建工作目录（exe 同理）：

```
程序目录/
  .env               ← LLM API Key 存这里（LLM_API_KEY=sk-xxx），不进 git、不进 config.json
  data/
    tasks/{id}/      任务工作目录（下载的视频、audio.wav、segments.json）
    models/          魔搭下载的 ASR 模型
  output/
    {视频名}/        每个视频一个独立产物文件夹（orig/trans/bilingual SRT、ASS、VTT、JSON）
```

- 详情页「文件位置」可一键打开产物文件夹；删除任务会连带删除它的产物文件夹。
- API Key 在网页设置里保存后会写入 `.env`；也可手动编辑。仓库里有 `.env.example` 模板。

## 目录结构（源码）

```
run.py              启动入口（exe 与源码共用；自动开窗口、端口占用检测、--port/--browser）
backend/            FastAPI 后端（app.py 路由 / pipeline.py 管线 / tasks.py 任务与 SSE
                    / asr/ 识别与魔搭下载 / translate/ 翻译审校 / downloader.py 链接下载
                    / subtitles.py 字幕输出）
frontend/           单页前端（零依赖、零构建、完全离线；打包时内嵌进 exe）
build_exe.bat       PyInstaller 打包脚本 → dist/asmr-subtitle-studio/
tools/ffmpeg/       内置 ffmpeg（打包时会复制到 dist）
scripts/e2e_test.py 端到端测试（对运行中的服务跑：英文样例→全流程→下载→编辑）
docs/API.md         REST API 契约
```

## 测试

```bash
./.venv/Scripts/python -m backend.tests.test_translate   # 翻译模块单元测试（无需依赖/联网）
./.venv/Scripts/python -m uvicorn backend.app:app --port 8765   # 先起服务
./.venv/Scripts/python scripts/e2e_test.py               # 端到端（自动开 mock 翻译）
```

实测（RTX 5070 Ti Laptop, CUDA float16, small 模型）：25s 英文音频 ASR 1.3s（≈19x 实时），全流程含 mock 翻译约 8s。

## 常见问题

- **双击 exe 后窗口一闪而过**：一般是端口被占用（已有实例在运行）或启动错误，现在的版本控制台会停留显示原因并等回车；也可用 `--port 8766` 换端口。旧版本请重新打包。
- **端口占用**：`start.bat`/exe 支持 `--port`；或改 run.py 里 `PORT`。
- **ffmpeg 找不到**：放入 `tools/ffmpeg/bin/`，或装到系统 PATH。
- **模型下载慢/失败**：直连魔搭支持断点续传，重试即可；或手动下载仓库内容解压到 `data/models/{模型名}/`。
- **CUDA 报错**：确认驱动 ≥ CUDA 12.8（RTX 50 系）；不行就设置里把设备改为 `cpu`。
