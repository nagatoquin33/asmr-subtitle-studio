# ASMR 字幕工作室 · 桌面版（Rust + Tauri v2）

Python/FastAPI 版的重写：**Windows 安装包、无本地 HTTP 服务、无端口监听**（彻底规避 Windows
保留端口段导致的 `[Errno 13] bind` 失败——Hyper-V/WinNAT 会随机保留 8740-8839 等端口段，
connect 探测不到但 bind 会被拒）。

## 架构

```
desktop/
  core/                 纯 Rust 核心（不依赖 Tauri，可独立测试/复用）
    paths.rs            目录布局 + 首启引导（解包内嵌工具、建 data/output）
    config.rs           配置 + .env 密钥（与 Python 版 config.json 同构）
    tasks.rs            任务管理器 + 事件广播 + task.json 节流持久化
    subtitles.rs        SRT/VTT/ASS/JSON 输出（与 Python 版逐字节兼容）
    ffmpeg.rs           ffmpeg/ffprobe 子进程封装（进度/取消）
    downloader.rs       yt-dlp 子进程封装（模板标记行解析进度）
    models.rs           ggml 模型下载（hf-mirror → huggingface，断点续传）+ VAD 模型
    asr.rs              whisper.cpp 转写、VAD 重试与幻觉过滤
    asr_worker.rs       按需启动 Vulkan 转写组件，失败时使用主程序内的 CPU 后端
    llm.rs / translate.rs  OpenAI 兼容客户端 + N|译文 批处理协议（完整移植）
    pipeline.rs         阶段编排（断点续跑）+ 单句编辑/重翻
    services.rs         服务层（任务创建/生命周期/导出）
    examples/e2e.rs     无 UI 端到端测试（样例 → ASR → mock 翻译 → 产物校验）
  src-tauri/            Tauri v2 壳（IPC 命令层 + asmrmedia:// 流媒体协议 + 事件桥）
  embedded/             zstd 压缩的 ffmpeg/ffprobe/yt-dlp（bundled-tools 特性内嵌）
```

## 作品库

- `data/library/<作品编号>/work.json` 保存作品名、RJ 编号以及相对曲目路径。原文件的副本及子目录保存在 `media/`。导入完整后，作品才出现在库里。
- `subtitles/<曲目编号>/` 保存工作室生成的独立字幕。现有同名或带语言后缀（`.ja` / `.zh` / `.bilingual` 等）的 SRT / VTT / ASS / LRC 字幕可直接随曲目播放（自动识别 UTF-8 / UTF-16 / Shift-JIS / GB18030），生成后优先加载新 VTT。视频的软字幕和硬字幕仍从字幕任务导出。
- 作品入库时只复制或下载，不建立字幕任务。需要时从作品页单条添加，或批量处理缺失字幕。播放和字幕工作区共用作品媒体。
- `data/webview/` 保存桌面界面偏好和 WebView2 本地数据。清理该目录会重置界面偏好，作品、字幕和模型各有自己的目录。

## 关键设计

- **零端口**：前端(WebView2) ↔ 后端(Rust) 全部走 Tauri IPC（invoke/listen），媒体流走
  自定义协议 `http://asmrmedia.localhost/{task_id}/{kind}`（支持 Range，视频可拖动）。
- **工具直装部署**：NSIS 安装包将 `ffmpeg`/`ffprobe`/`yt-dlp` 与 Vulkan 转写组件直接作为安装包资源以 LZMA 压缩部署到 `tools/`，主程序体积小、冷启动快，升级时自动更新工具（便携单文件构建仍保留 `--features bundled-tools`）。数据目录 `data/`、`output/`、`.env` 同级生成。
- **ASR 换引擎**：faster-whisper(CTranslate2) → whisper.cpp(ggml)。模型从
  hf-mirror.com 直连下载（ggml-small/medium/large-v3/large-v3-turbo + silero VAD）。
  自动和优先 GPU 会在可复用的独立 Vulkan 进程中运行（连续队列复用显存模型、空闲 90 秒自动释放，支持 N/A/I 跨厂商 Vulkan 探测），不可用或失败时主程序继续用 CPU。仅 CPU 会直接在主程序中识别。
  compute_type 兼容保留（ggml 量化由模型文件决定）。
- **视频与字幕同目录**：URL 下载的视频在任务完成后自动移入 `output/{视频名}/`
  与字幕同文件夹（`pipeline.move_video_to_output`，默认开启）；
  本地文件任务保持零拷贝引用原位置，不搬动用户文件。
- **成品视频导出**：任务详情 → 导出区 →
  「成品视频（硬字幕）」把双语 ASS 烧录进画面（N 卡走 NVENC 硬件编码，
  8 分钟 720p 约 31s；无 N 卡自动回退 libx264）；
  「软字幕 MKV」流复制封装（秒级，播放器可开关字幕）。
- **兼容性**：任务/segments/字幕产物与 Python 版格式完全一致（同一 data/tasks 结构，
  断点续跑互通；旧 CTranslate2 模型不兼容 ggml，需重新下载模型）。

## 段编辑与导出命令（v2.1 易用性）

字幕编辑从「只能改译文」扩展到「原文 / 时间码 / 合并拆分 / 整轨平移 / 批量重翻」，
全部命令共用同一落盘出口 `pipeline::persist_edited`：重排 index → 写 `segments.json`
→ `subtitles::write_all` 重写全部产物 → 更新 `counts` / `outputs` / `has_output`。

| IPC 命令 | 作用 |
|---|---|
| `patch_segment_fields(task_id, index, text?, translated?, start?, end?)` | 改原文/译文/起止时间（仅传传入字段；保证 end>start） |
| `shift_segments(task_id, offset)` | 全部时间码整体平移（start 下限 0，保留 ≥50ms 时长） |
| `merge_segments(task_id, index)` | 合并 index 与 index+1（文本空格拼接、flagged 取或） |
| `split_segment(task_id, index, at)` | 在绝对秒 at 处拆分（文本按时间比例切分） |
| `retranslate_batch(task_id, indices)` | 批量重翻（并发 clamp(concurrency,1,6)，整批只写盘一次） |
| `export_all_outputs(task_id)` | 选文件夹一次导出 6 种产物 |
| `delete_model(name)` | 删除本地模型（白名单校验，拒路径穿越） |
| `cancel_burn(task_id)` | 取消成品视频渲染（共享取消注册表） |
| `app_paths()` | 返回 root/data/output/models/tasks/tools 绝对路径 |
| `list_tasks(limit, offset)` | 分页 + `total`（前端「加载更多」用） |

纯逻辑（`round_ms` / `reindex_segments` / `patch_segment_fields_in` /
`shift_segments_in` / `merge_segments_in` / `split_segment_in` / `clamp_concurrency`）
不依赖 IO，全部在 `core/tests/logic.rs` 里有单元测试（`cargo test -p asmr-core`，24 项）。

## 开发

```bash
# 依赖：rustup(MSVC) + CMake + LLVM(libclang, bindgen 用) + VS Build Tools
export PATH="$HOME/.cargo/bin:/c/Program Files/CMake/bin:/c/Program Files/LLVM/bin:$PATH"
export LIBCLANG_PATH="C:\Program Files\LLVM\bin"

cargo build                       # 开发构建（不内嵌工具，需要仓库根的 tools/ 可见）
cargo run -p asmr-studio          # 打开桌面窗口（ASMR_HOME=临时目录 可隔离测试）
cargo run -p asmr-core --example e2e   # 无 UI 端到端（mock 翻译）
ASMR_E2E_REAL=1 cargo run -p asmr-core --example e2e   # 真实 API 翻译
cargo run -p asmr-core --example asr_min -- <wav>      # whisper 隔离测试
```

前端即仓库根 `frontend/`（零构建零依赖），编译期内嵌进 exe；改前端后重新
`cargo build` 即可（app crate 重链很快，嵌入式工具大块在 core crate，很少重编）。
`src-tauri/build.rs` 已声明 `rerun-if-changed=../../frontend`，改 `index.html` /
`app.js` / `api.js` / `style.css` / `ux.css` 都会自动触发重编，不会出现「改了前端但
exe 里还是旧界面」。

## Windows 安装包

在 Windows x64 上安装 Rust MSVC、Visual Studio Build Tools、CMake、Ninja、LLVM、Node.js 和 Vulkan SDK，然后运行：

```bat
desktop\build-release.bat
```

脚本依次构建 Vulkan 转写组件和主程序，用 Tauri 的 NSIS 向导打包，产物在 `desktop\dist\installer\`。可通过 `VULKAN_SDK`、`VS_VCVARS`、`LIBCLANG_PATH` 和 `ASMR_BUILD_ROOT` 指定本机路径。构建目录应使用短路径，例如 `D:\asmr-build`，避免 Windows 工具的路径长度限制。用户端使用显卡驱动提供的 Vulkan 运行库，构建用的 SDK 只装在构建机。

发布新版时一起递增 Cargo 工作区和 Tauri 配置的版本号，沿用应用标识 `local.asmr.studio`。安装器记住原安装位置，覆盖更新程序和 Vulkan 组件。应用把作品、配置和模型保存在安装目录，正常升级沿用这份数据。

## 已修复的关键问题（迭代记录）

- **点击「下载模型」闪退（0xc0000409）**：同步 Tauri 命令线程没有 tokio reactor，
  `tokio::spawn` 直接 panic→abort。修复：ModelStore 注入 `runtime::Handle` +
  命令改 async。同类问题已全量排查（其余命令不涉及 spawn）。
- **模型下载卡死**：hf-mirror 大文件常中途断流，旧实现立即换 huggingface.co
  （国内不通）且无读超时 → 无限挂起。修复：45s 分块读超时 + **同源断点续传**
  （最多 8 次退避重试），仅 HTTP 4xx 才换源。
- **panic 可诊断**：release 下 panic=abort 闪退无痕迹；现 panic 钩子把
  崩溃写入 `data/panic.log`（时间/线程/位置）。
- 性能：翻译并发默认 3→6、下载分片 4→8、GPU 开 flash attention。

## 实测（RTX 5070 Ti Laptop · large-v3-turbo · B 站 80 分钟日文 ASMR 全片）

- 8 分钟切片（flash attention 开启后）：ASR **17.4s ≈ 27.6 倍实时**（此前 43.3s/13 倍）
- 全片端到端（下载 368MB → ASR → 翻译+审校 485 段 → 双语字幕）：约 10 分钟内
- 翻译模型默认 `deepseek-flash`（deepseek-chat 已弃用，配置自动迁移旧名）
- 幻觉过滤：丢弃连续重复游程 + 高重复字符段（80 分钟片滤除 1731 段，0 残留）
- 断点续跑重试 0.3-0.6s；单元测试 12 项（含旧模型名迁移）
