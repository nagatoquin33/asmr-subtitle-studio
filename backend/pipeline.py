"""管线编排：阶段化执行 下载 → 抽音频 → ASR → 翻译 → 审校 → 写字幕。

结构说明：
- StageContext 携带一次任务运行的全部上下文，各阶段函数独立、可单独测试；
- 纯决策逻辑（是否跳过翻译、产物目录、resume 判定）抽为模块级纯函数；
- 断点续跑基于磁盘产物检查（视频已下载/audio.wav 已存在/segments.json 已存在）。
"""
from __future__ import annotations

import asyncio
import copy
import json
import logging
import time
import traceback
from dataclasses import dataclass, field
from pathlib import Path

from . import config, ffmpeg_utils, subtitles
from .asr import whisper_asr
from .downloader import DownloadCanceled
from .ffmpeg_utils import Canceled
from .tasks import manager, sanitize_filename

log = logging.getLogger(__name__)


# ---------- 纯决策函数 ----------
def _looks_mock(text: str) -> bool:
    """Mock 模式的伪翻译前缀——不能算真实译文（续跑判断用）。"""
    return (text or "").strip().startswith("[译]")


def segments_all_translated(segments: list[dict] | None) -> bool:
    return bool(segments) and all(
        (s.get("translated") or "").strip() and not _looks_mock(s.get("translated"))
        for s in segments
    )


def should_skip_translation(source_lang: str, target_lang: str, detected: str | None,
                            prob: float) -> tuple[bool, str]:
    """是否跳过翻译：只有「显式指定」源=目标，或自动检测置信度 ≥0.9 且同语言。

    ASMR 音频（轻声+音乐）下语言检测不可靠，宁可多翻一次也不能静默漏翻。
    """
    if source_lang and source_lang != "auto" and source_lang == target_lang:
        return True, "源语言=目标语言（手动指定），跳过翻译"
    if detected and detected == target_lang and prob >= 0.9:
        return True, f"识别语言 {detected}（置信度 {prob:.0%}）与目标语言相同，跳过翻译"
    return False, ""


def plan_output_dir(task_id: str, name: str) -> Path:
    stem = Path(name).stem
    safe = sanitize_filename(stem)[:60] or task_id[-6:]
    out_dir = config.OUTPUT_DIR / safe
    if out_dir.exists() and any(out_dir.iterdir()):
        out_dir = config.OUTPUT_DIR / f"{safe}_{task_id[-6:]}"
    return out_dir


def load_existing_segments(task_dir: Path) -> list[dict] | None:
    seg_file = task_dir / "segments.json"
    if not seg_file.exists():
        return None
    try:
        loaded = json.loads(seg_file.read_text(encoding="utf-8"))
        if isinstance(loaded, list) and loaded and all(s.get("text") for s in loaded):
            return loaded
    except Exception:
        pass
    return None


def _require_llm(tcfg: dict) -> None:
    """翻译前快速校验，避免任务跑到一半才因 401 失败。"""
    if tcfg.get("mock"):
        return
    if not (tcfg.get("api_key") and tcfg.get("base_url")):
        raise RuntimeError(
            "未配置翻译 API：请到 设置 → 翻译 API 填写 Base URL 与 API Key 并保存，"
            "然后在任务上点「重试」即可继续（识别结果已保留，无需重跑）；"
            "也可以先开启 Mock 模式体验全流程"
        )


def _fmt_clock(sec: float) -> str:
    sec = max(0, int(sec))
    return f"{sec // 3600:02d}:{sec % 3600 // 60:02d}:{sec % 60:02d}"


def _mb(n: float | None) -> str:
    return f"{n / 1048576:.1f}MB" if n else "?"


# ---------- 阶段上下文 ----------
@dataclass
class StageContext:
    task_id: str
    cfg: dict
    opts: dict
    source_lang: str
    target_lang: str
    task_dir: Path
    media_path: str | None = None
    duration: float = 0.0
    wav_path: Path | None = None
    segments: list[dict] | None = None
    asr_meta: dict = field(default_factory=dict)
    need_translate: bool = True
    all_translated: bool = False
    outputs: dict = field(default_factory=dict)
    out_dir: Path | None = None


# ---------- 阶段实现 ----------
async def _stage_download(ctx: StageContext) -> None:
    task = manager.get_mutable(ctx.task_id)
    ctx.media_path = task["media"]["path"]
    if not task.get("source_url"):
        return
    manager.update(ctx.task_id, status="downloading", stage="downloading")
    if ctx.media_path and Path(ctx.media_path).exists():
        manager.update(ctx.task_id, progress=0.04, message="视频已下载，跳过下载")
        return

    dcfg = ctx.cfg.get("download", {})
    beat = {"t": 0.0}

    def dl_progress(done: float, total: float, speed: float | None):
        now = time.time()
        if now - beat["t"] < 0.5 and done < (total or 0):
            return
        beat["t"] = now
        msg = f"下载中 {_mb(done)}/{_mb(total)}" + (f"，{speed / 1048576:.1f}MB/s" if speed else "")
        manager.update(ctx.task_id,
                       progress=(done / total) * 0.05 if total else 0.02, message=msg)

    def _do_download():
        from .downloader import download_media

        return download_media(
            task["source_url"], ctx.task_dir,
            height=int(dcfg.get("height") or 720),
            proxy=dcfg.get("proxy") or "",
            cookies_from_browser=dcfg.get("cookies_from_browser") or "",
            progress_cb=dl_progress,
            cancel_cb=lambda: manager.is_cancelled(ctx.task_id),
        )

    info = await asyncio.to_thread(_do_download)
    ctx.media_path = info["path"]
    size = Path(ctx.media_path).stat().st_size
    manager.update(
        ctx.task_id,
        name=Path(ctx.media_path).name,
        media={"path": ctx.media_path, "duration": info.get("duration"),
               "size": size, "has_video": True},
        message=f"下载完成：{_mb(size)}",
    )


async def _stage_extract(ctx: StageContext) -> None:
    manager.update(ctx.task_id, status="extracting", stage="extracting",
                   progress=0.05, message="正在探测媒体信息…")
    probe = await asyncio.to_thread(ffmpeg_utils.probe, ctx.media_path)
    ctx.duration = probe["duration"] or 0.0
    task = manager.get_mutable(ctx.task_id)
    manager.update(ctx.task_id, media={**task["media"], **probe, "duration": ctx.duration})

    ctx.wav_path = ctx.task_dir / "audio.wav"
    if ctx.wav_path.exists():
        manager.update(ctx.task_id, message="复用已提取的音频，跳过抽取")
        return

    def extract_progress(frac: float):
        manager.update(ctx.task_id, progress=0.05 + frac * 0.03,
                       message=f"提取音频 {int(frac * 100)}%", event=frac >= 1.0)

    await asyncio.to_thread(
        ffmpeg_utils.extract_audio, ctx.media_path, ctx.wav_path, ctx.duration or None,
        extract_progress, lambda: manager.is_cancelled(ctx.task_id),
    )


async def _stage_asr(ctx: StageContext) -> None:
    ctx.segments = load_existing_segments(ctx.task_dir)
    ctx.asr_meta = manager.get_mutable(ctx.task_id).get("asr") or {}
    if ctx.segments is not None:
        manager.update(ctx.task_id, message=f"已存在识别结果（{len(ctx.segments)} 段），跳过识别")
        return

    cfg_asr = ctx.cfg["asr"]
    model_name = cfg_asr.get("model", "small")
    manager.update(ctx.task_id, status="asr", stage="asr", progress=0.08,
                   message=f"准备模型 {model_name}…")

    # GPU 运行库按需准备：单文件包不含 CUDA DLL，检测到 N 卡时自动下载一次
    if cfg_asr.get("device", "auto") != "cpu":
        from .asr.cuda_runtime import dlls_ready, ensure_cuda_runtime

        gpu_ok, gpu_name = whisper_asr.gpu_available()
        if gpu_ok and not dlls_ready():
            manager.update(ctx.task_id,
                           message=f"检测到 {gpu_name}，准备 GPU 运行库（仅首次，约 1-2GB）…")

            def cuda_progress(msg: str):
                manager.update(ctx.task_id, message=msg)

            try:
                if await asyncio.to_thread(
                    ensure_cuda_runtime, cuda_progress,
                    lambda: manager.is_cancelled(ctx.task_id),
                ):
                    manager.update(ctx.task_id, message=f"GPU 运行库就绪（{gpu_name}）")
            except Exception as e:
                log.warning("CUDA 运行库获取失败，将用 CPU: %s", e)
                manager.update(ctx.task_id, message=f"GPU 运行库获取失败（{str(e)[:80]}），使用 CPU")

    def asr_progress(frac: float):
        manager.update(ctx.task_id, progress=0.08 + frac * 0.52,
                       message=f"识别中 {_fmt_clock(frac * ctx.duration)} / {_fmt_clock(ctx.duration)}",
                       event=frac >= 1.0)

    def _do_asr():
        return whisper_asr.transcribe(
            str(ctx.wav_path),
            model_name=model_name,
            device=cfg_asr.get("device", "auto"),
            compute_type=cfg_asr.get("compute_type", "auto"),
            source_lang=ctx.source_lang if ctx.source_lang != "auto" else None,
            duration=ctx.duration or None,
            vad=cfg_asr.get("vad", "auto"),
            progress_cb=asr_progress,
            cancel_cb=lambda: manager.is_cancelled(ctx.task_id),
        )

    ctx.segments, ctx.asr_meta = await asyncio.to_thread(_do_asr)
    if ctx.asr_meta.get("warning"):
        manager.update(ctx.task_id, message=f"识别中…（{ctx.asr_meta['warning']}）")
    manager.update(ctx.task_id, asr=ctx.asr_meta)
    # 识别结果立即落盘：翻译阶段失败后重试无需重跑识别
    (ctx.task_dir / "segments.json").write_text(
        json.dumps(ctx.segments, ensure_ascii=False, indent=1), encoding="utf-8")


async def _stage_translate(ctx: StageContext) -> None:
    detected = ctx.asr_meta.get("language")
    prob = float(ctx.asr_meta.get("language_probability") or 0)
    skip, reason = should_skip_translation(ctx.source_lang, ctx.target_lang, detected, prob)
    ctx.need_translate = not skip
    ctx.all_translated = segments_all_translated(ctx.segments)

    if ctx.segments and ctx.need_translate and not ctx.all_translated:
        _require_llm(ctx.cfg["translate"])  # 未配置 API 时快速报错，指引配置后重试
        from .translate import SubtitleTranslator

        manager.update(ctx.task_id, status="translating", stage="translating",
                       progress=0.60, message="开始翻译…")
        translator = SubtitleTranslator(ctx.cfg["translate"])

        def tr_progress(done: int, total_n: int, msg: str = ""):
            manager.update(ctx.task_id, progress=0.60 + 0.25 * (done / max(total_n, 1)),
                           message=msg or f"翻译 {done}/{total_n}")

        translated = await translator.translate(
            ctx.segments, detected if detected in ("zh", "en", "ja") else None,
            ctx.target_lang, progress_cb=tr_progress,
        )
        for seg, tr in zip(ctx.segments, translated):
            seg["translated"] = tr
    elif not ctx.need_translate:
        for seg in ctx.segments or []:
            seg["translated"] = seg["text"]
        manager.update(ctx.task_id, message=reason)
    else:
        manager.update(ctx.task_id, message="已有译文，跳过翻译（续跑）")


async def _stage_proofread(ctx: StageContext) -> None:
    if not (ctx.need_translate and not ctx.all_translated and ctx.segments
            and ctx.cfg["translate"].get("proofread", True)):
        return
    from .translate import Proofreader

    manager.update(ctx.task_id, status="proofreading", stage="proofreading",
                   progress=0.85, message="开始审校…")
    proofreader = Proofreader(ctx.cfg["translate"])

    def pf_progress(done: int, total_n: int, msg: str = ""):
        manager.update(ctx.task_id, progress=0.85 + 0.10 * (done / max(total_n, 1)),
                       message=msg or f"审校 {done}/{total_n}")

    polished = await proofreader.proofread(
        ctx.segments, [s.get("translated", "") for s in ctx.segments], ctx.target_lang,
        progress_cb=pf_progress,
    )
    for seg, tr in zip(ctx.segments, polished):
        seg["translated"] = tr
    for seg in ctx.segments:
        seg.setdefault("translated", seg["text"])


async def _stage_write(ctx: StageContext) -> None:
    manager.update(ctx.task_id, status="writing", stage="writing", progress=0.95,
                   message="生成字幕文件…")
    task = manager.get_mutable(ctx.task_id)
    ctx.out_dir = plan_output_dir(ctx.task_id, task["name"])
    ctx.outputs = await asyncio.to_thread(
        subtitles.write_all, ctx.out_dir, ctx.segments or [], ctx.cfg.get("output", {}))
    (ctx.task_dir / "segments.json").write_text(
        json.dumps(ctx.segments or [], ensure_ascii=False, indent=1), encoding="utf-8")
    manager.update(ctx.task_id, output_dir=str(ctx.out_dir))

    if not ctx.cfg.get("pipeline", {}).get("keep_audio", False):
        try:
            (ctx.task_dir / "audio.wav").unlink(missing_ok=True)
        except OSError:
            pass

    done_msg = f"完成：{len(ctx.segments or [])} 段" \
        + (f"，ASR 加速比 {ctx.asr_meta.get('speed_ratio', '?')}x" if ctx.asr_meta.get("speed_ratio") else "") \
        + ("（音乐内容，已自动关闭 VAD 重识别）" if ctx.asr_meta.get("vad_retry") else "")
    manager.update(
        ctx.task_id,
        status="done", stage="done", progress=1.0,
        message=done_msg,
        counts={"segments": len(ctx.segments or []), "translated": len(ctx.segments or [])},
        has_result=True,
        outputs=ctx.outputs,
    )


STAGES = (_stage_download, _stage_extract, _stage_asr, _stage_translate, _stage_proofread, _stage_write)


async def run_task(task_id: str) -> None:
    task = manager.get_mutable(task_id)
    if not task or task["status"] != "queued":
        if task and task["status"] == "queued" and task.get("cancel"):
            manager.update(task_id, status="canceled", stage="canceled", message="已取消")
        return

    opts = task["options"]
    ctx = StageContext(
        task_id=task_id,
        cfg=copy.deepcopy(config.get_config()),
        opts=opts,
        source_lang=opts.get("source_lang") or "auto",
        target_lang=opts.get("target_lang") or "zh",
        task_dir=config.TASKS_DIR / task_id,
    )

    try:
        for stage in STAGES:
            if manager.is_cancelled(task_id):
                raise Canceled()
            await stage(ctx)
        manager.emit_final(task_id)

    except (Canceled, DownloadCanceled):
        manager.update(task_id, status="canceled", stage="canceled", message="已取消")
        manager.emit_final(task_id)
    except Exception as e:
        log.error("任务 %s 失败:\n%s", task_id, traceback.format_exc())
        manager.update(task_id, status="error", stage="error",
                       error=str(e)[:500], message=f"失败：{str(e)[:200]}")
        manager.emit_final(task_id)


# ---------- 单句编辑 / 重翻 ----------
def _rewrite_outputs(task: dict, segments: list[dict], out_cfg: dict) -> None:
    """单句编辑/重翻后重写字幕产物到该任务的 output 目录。"""
    out_dir = Path(task.get("output_dir") or (config.TASKS_DIR / task["id"] / "output"))
    subtitles.write_all(out_dir, segments, out_cfg)


async def retranslate_segment(task_id: str, index: int) -> dict:
    """用上下文重翻单句（走已配置的翻译 API），返回更新后的段。"""
    task_dir = config.TASKS_DIR / task_id
    seg_file = task_dir / "segments.json"
    segments = json.loads(seg_file.read_text(encoding="utf-8"))
    if index < 0 or index >= len(segments):
        raise IndexError("段索引越界")
    cfg = copy.deepcopy(config.get_config())
    from .translate import SubtitleTranslator

    lo = max(0, index - 3)
    ctx = segments[lo:index + 1]
    target_lang = manager.get_mutable(task_id)["options"].get("target_lang") or "zh"
    translator = SubtitleTranslator(cfg["translate"])
    results = await translator.translate(ctx, None, target_lang)
    segments[index]["translated"] = results[-1] or segments[index]["translated"]
    seg_file.write_text(json.dumps(segments, ensure_ascii=False, indent=1), encoding="utf-8")
    _rewrite_outputs(manager.get_mutable(task_id), segments, cfg.get("output", {}))
    task = manager.get_mutable(task_id)
    task["counts"]["translated"] = sum(1 for s in segments if s.get("translated"))
    manager.update(task_id)
    return segments[index]


def update_segment_text(task_id: str, index: int, translated: str) -> dict:
    task_dir = config.TASKS_DIR / task_id
    seg_file = task_dir / "segments.json"
    segments = json.loads(seg_file.read_text(encoding="utf-8"))
    if index < 0 or index >= len(segments):
        raise IndexError("段索引越界")
    segments[index]["translated"] = translated.strip()
    seg_file.write_text(json.dumps(segments, ensure_ascii=False, indent=1), encoding="utf-8")
    _rewrite_outputs(manager.get_mutable(task_id), segments, config.get_config().get("output", {}))
    task = manager.get_mutable(task_id)
    task["counts"]["translated"] = sum(1 for s in segments if s.get("translated"))
    manager.update(task_id)
    return segments[index]
