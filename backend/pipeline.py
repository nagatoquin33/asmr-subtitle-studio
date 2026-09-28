"""管线编排：抽音频 → ASR → 翻译 → 审校 → 写字幕。"""
from __future__ import annotations

import asyncio
import copy
import json
import logging
import time
import traceback
from pathlib import Path

from . import config, ffmpeg_utils, subtitles
from .asr import modelscope_dl, whisper_asr
from .downloader import DownloadCanceled
from .tasks import manager, sanitize_filename
from .ffmpeg_utils import Canceled

log = logging.getLogger(__name__)


def _looks_mock(text: str) -> bool:
    """Mock 模式的伪翻译前缀——不能算真实译文（续跑判断用）。"""
    return (text or "").strip().startswith("[译]")


def _fmt_clock(sec: float) -> str:
    sec = max(0, int(sec))
    return f"{sec // 3600:02d}:{sec % 3600 // 60:02d}:{sec % 60:02d}"


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


async def run_task(task_id: str) -> None:
    task = manager.get_mutable(task_id)
    if not task or task.get("cancel") or task["status"] not in ("queued",):
        if task and task["status"] == "queued" and task.get("cancel"):
            manager.update(task_id, status="canceled", stage="canceled", message="已取消")
        return

    cfg = copy.deepcopy(config.get_config())
    opts = task["options"]
    target_lang = opts.get("target_lang") or "zh"
    source_lang = opts.get("source_lang") or "auto"

    try:
        task_dir = config.TASKS_DIR / task_id

        # ---- 0. URL 下载（链接任务；媒体文件已存在则续跑跳过） ----
        media_path = task["media"]["path"]
        if task.get("source_url"):
            if media_path and Path(media_path).exists():
                manager.update(task_id, status="downloading", stage="downloading",
                               progress=0.04, message="视频已下载，跳过下载")
            else:
                manager.update(task_id, status="downloading", stage="downloading",
                               progress=0.0, message="开始下载…")
                dcfg = cfg.get("download", {})
                beat = {"t": 0.0}

                def _mb(n):
                    return f"{n / 1048576:.1f}MB" if n else "?"

                def dl_progress(done: float, total: float, speed: float | None):
                    now = time.time()
                    if now - beat["t"] < 0.5 and done < (total or 0):
                        return
                    beat["t"] = now
                    msg = f"下载中 {_mb(done)}/{_mb(total)}" + (f"，{speed / 1048576:.1f}MB/s" if speed else "")
                    manager.update(task_id,
                                   progress=(done / total) * 0.05 if total else 0.02,
                                   message=msg)

                def _do_download():
                    from .downloader import download_media

                    return download_media(
                        task["source_url"], task_dir,
                        height=int(dcfg.get("height") or 720),
                        proxy=dcfg.get("proxy") or "",
                        cookies_from_browser=dcfg.get("cookies_from_browser") or "",
                        progress_cb=dl_progress,
                        cancel_cb=lambda: manager.is_cancelled(task_id),
                    )

                info = await asyncio.to_thread(_do_download)
                media_path = info["path"]
                size = Path(media_path).stat().st_size
                manager.update(
                    task_id,
                    name=Path(media_path).name,
                    media={"path": media_path, "duration": info.get("duration"),
                           "size": size, "has_video": True},
                    message=f"下载完成：{_mb(size)}",
                )

        # ---- 1. 探测 + 抽取音频（audio.wav 已存在则续跑跳过） ----
        manager.update(task_id, status="extracting", stage="extracting",
                       progress=0.05, message="正在探测媒体信息…")
        probe = await asyncio.to_thread(ffmpeg_utils.probe, media_path)
        duration = probe["duration"] or 0.0
        manager.update(task_id, media={**task["media"], **probe, "duration": duration})

        wav_path = task_dir / "audio.wav"
        if wav_path.exists():
            manager.update(task_id, message="复用已提取的音频，跳过抽取")
        else:
            def extract_progress(frac: float):
                manager.update(task_id, progress=0.05 + frac * 0.03,
                               message=f"提取音频 {int(frac * 100)}%", event=frac >= 1.0)

            await asyncio.to_thread(
                ffmpeg_utils.extract_audio, media_path, wav_path, duration or None,
                extract_progress, lambda: manager.is_cancelled(task_id),
            )

        # ---- 2. ASR（已存在识别结果则续跑跳过） ----
        if manager.is_cancelled(task_id):
            raise Canceled()
        seg_file = task_dir / "segments.json"
        segments: list | None = None
        asr_meta: dict = task.get("asr") or {}
        if seg_file.exists():
            try:
                loaded = json.loads(seg_file.read_text(encoding="utf-8"))
                if isinstance(loaded, list) and loaded and all(s.get("text") for s in loaded):
                    segments = loaded
            except Exception:
                segments = None
        if segments is not None:
            manager.update(task_id, message=f"已存在识别结果（{len(segments)} 段），跳过识别")
        else:
            model_name = cfg["asr"].get("model", "small")
            manager.update(task_id, status="asr", stage="asr", progress=0.05,
                           message=f"准备模型 {model_name}…")

            def asr_progress(frac: float):
                manager.update(task_id, progress=0.05 + frac * 0.55,
                               message=f"识别中 {_fmt_clock(frac * duration)} / {_fmt_clock(duration)}",
                               event=frac >= 1.0)

            def _do_asr():
                return whisper_asr.transcribe(
                    str(wav_path),
                    model_name=model_name,
                    device=cfg["asr"].get("device", "auto"),
                    compute_type=cfg["asr"].get("compute_type", "auto"),
                    source_lang=source_lang if source_lang != "auto" else None,
                    duration=duration or None,
                    vad=cfg["asr"].get("vad", "auto"),
                    progress_cb=asr_progress,
                    cancel_cb=lambda: manager.is_cancelled(task_id),
                )

            segments, asr_meta = await asyncio.to_thread(_do_asr)
            if asr_meta.get("warning"):
                manager.update(task_id, message=f"识别中…（{asr_meta['warning']}）")
            manager.update(task_id, asr=asr_meta)
            # 识别结果立即落盘：翻译阶段失败后重试无需重跑识别
            seg_file.write_text(json.dumps(segments, ensure_ascii=False, indent=1), encoding="utf-8")
        detected = asr_meta.get("language")

        # ---- 3. 翻译 ----
        # 只有「显式指定」源语言=目标语言，或自动检测置信度 ≥0.9 且语言相同才跳过；
        # ASMR 音频（轻声+音乐）下语言检测不可靠，宁可多跑一次翻译也不能静默漏翻。
        if manager.is_cancelled(task_id):
            raise Canceled()
        explicit_same = source_lang != "auto" and source_lang == target_lang
        prob = float(asr_meta.get("language_probability") or 0)
        confident_same = detected == target_lang and prob >= 0.9
        need_translate = not (explicit_same or confident_same)
        all_translated = bool(segments) and all(
            (s.get("translated") or "").strip() and not _looks_mock(s.get("translated"))
            for s in segments
        )

        if segments and need_translate and not all_translated:
            _require_llm(cfg["translate"])  # 未配置 API 时快速报错，指引配置后重试
            from .translate import SubtitleTranslator  # 懒加载

            manager.update(task_id, status="translating", stage="translating",
                           progress=0.60, message="开始翻译…")
            translator = SubtitleTranslator(cfg["translate"])

            def tr_progress(done: int, total_n: int, msg: str = ""):
                manager.update(task_id, progress=0.60 + 0.25 * (done / max(total_n, 1)),
                               message=msg or f"翻译 {done}/{total_n}")

            translated = await translator.translate(
                segments, detected if detected in ("zh", "en", "ja") else None,
                target_lang, progress_cb=tr_progress,
            )
            for seg, tr in zip(segments, translated):
                seg["translated"] = tr
        elif not need_translate:
            for seg in segments:
                seg["translated"] = seg["text"]
            reason = ("源语言=目标语言（手动指定），跳过翻译" if explicit_same
                      else f"识别语言 {detected}（置信度 {prob:.0%}）与目标语言相同，跳过翻译")
            manager.update(task_id, message=reason)
        else:
            manager.update(task_id, message="已有译文，跳过翻译（续跑）")

        # ---- 4. 审校 ----
        if manager.is_cancelled(task_id):
            raise Canceled()
        if need_translate and not all_translated and segments and cfg["translate"].get("proofread", True):
            from .translate import Proofreader

            manager.update(task_id, status="proofreading", stage="proofreading",
                           progress=0.85, message="开始审校…")
            proofreader = Proofreader(cfg["translate"])

            def pf_progress(done: int, total_n: int, msg: str = ""):
                manager.update(task_id, progress=0.85 + 0.10 * (done / max(total_n, 1)),
                               message=msg or f"审校 {done}/{total_n}")

            polished = await proofreader.proofread(
                segments, [s.get("translated", "") for s in segments], target_lang,
                progress_cb=pf_progress,
            )
            for seg, tr in zip(segments, polished):
                seg["translated"] = tr
        for seg in segments:
            seg.setdefault("translated", seg["text"])

        # ---- 5. 写产物（output/{视频名}/，每个视频独立文件夹） ----
        manager.update(task_id, status="writing", stage="writing", progress=0.96,
                       message="生成字幕文件…")
        out_cfg = cfg.get("output", {})
        stem = Path(task["name"]).stem
        safe = sanitize_filename(stem)[:60] or task_id[-6:]
        out_dir = config.OUTPUT_DIR / safe
        if out_dir.exists() and any(out_dir.iterdir()):
            out_dir = config.OUTPUT_DIR / f"{safe}_{task_id[-6:]}"
        outputs = await asyncio.to_thread(subtitles.write_all, out_dir, segments, out_cfg)
        (task_dir / "segments.json").write_text(
            json.dumps(segments, ensure_ascii=False, indent=1), encoding="utf-8")
        manager.update(task_id, output_dir=str(out_dir))

        if not cfg.get("pipeline", {}).get("keep_audio", False):
            try:
                (task_dir / "audio.wav").unlink(missing_ok=True)
            except OSError:
                pass

        done_msg = f"完成：{len(segments)} 段" \
            + (f"，ASR 加速比 {asr_meta.get('speed_ratio', '?')}x" if asr_meta.get("speed_ratio") else "") \
            + ("（音乐内容，已自动关闭 VAD 重识别）" if asr_meta.get("vad_retry") else "")
        manager.update(
            task_id,
            status="done", stage="done", progress=1.0,
            message=done_msg,
            counts={"segments": len(segments), "translated": len(segments)},
            has_result=True,
            outputs=outputs,
        )
        manager.emit_final(task_id)

    except (Canceled, DownloadCanceled):
        manager.update(task_id, status="canceled", stage="canceled", message="已取消")
        manager.emit_final(task_id)
    except Exception as e:
        log.error("任务 %s 失败:\n%s", task_id, traceback.format_exc())
        manager.update(task_id, status="error", stage="error",
                       error=str(e)[:500], message=f"失败：{str(e)[:200]}")
        manager.emit_final(task_id)


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


def _rewrite_outputs(task: dict, segments: list[dict], out_cfg: dict) -> None:
    """单句编辑/重翻后重写字幕产物到该任务的 output 目录。"""
    out_dir = Path(task.get("output_dir") or (config.TASKS_DIR / task["id"] / "output"))
    subtitles.write_all(out_dir, segments, out_cfg)
