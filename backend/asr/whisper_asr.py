"""faster-whisper ASR 封装：模型缓存、设备解析、CUDA 回退、转写、幻觉标记。"""
from __future__ import annotations

import logging
import os
import re
import sys
import time
from collections import OrderedDict
from pathlib import Path

from .. import ffmpeg_utils
from . import modelscope_dl

log = logging.getLogger(__name__)

MAX_LOADED_MODELS = 2

_cache: OrderedDict[tuple, object] = OrderedDict()
_cache_lock = None  # 延迟创建


def _lock():
    global _cache_lock
    import threading

    if _cache_lock is None:
        _cache_lock = threading.Lock()
    return _cache_lock


class ASRCanceled(RuntimeError):
    pass


_CUDA_ERR_RE = re.compile(r"cublas|cudnn|cuda|CUDA", re.IGNORECASE)


def _setup_cuda_dlls() -> None:
    """注册 pip 安装的 NVIDIA 运行库（nvidia-cublas-cu12 / nvidia-cudnn-cu12）DLL 目录。"""
    dirs = []
    for p in Path(sys.executable).parent.parent.glob("Lib/site-packages/nvidia/*/bin"):
        dirs.append(p)
    if not dirs:
        for p in Path(sys.prefix).glob("**/nvidia/*/bin"):
            dirs.append(p)
    for d in dirs:
        try:
            os.add_dll_directory(str(d))
            os.environ["PATH"] = f"{d};{os.environ.get('PATH', '')}"
        except OSError:
            pass
    if dirs:
        log.info("已注册 %d 个 NVIDIA DLL 目录", len(dirs))


def gpu_available() -> tuple[bool, str]:
    try:
        import ctranslate2

        if ctranslate2.get_cuda_device_count() > 0:
            try:
                name = ctranslate2.get_cuda_device_name(0)
            except Exception:
                name = "CUDA GPU"
            return True, str(name)
    except Exception:
        pass
    try:
        import subprocess

        out = subprocess.run(
            ["nvidia-smi", "--query-gpu=name", "--format=csv,noheader"],
            capture_output=True, text=True, timeout=5,
        )
        if out.returncode == 0 and out.stdout.strip():
            return False, out.stdout.strip().splitlines()[0]
    except Exception:
        pass
    return False, ""


def _resolve(device: str, compute_type: str) -> tuple[str, str]:
    if device == "auto":
        ok, _ = gpu_available()
        device = "cuda" if ok else "cpu"
    if compute_type == "auto":
        compute_type = "float16" if device == "cuda" else "int8"
    return device, compute_type


def _build_model(path: Path, device: str, compute_type: str):
    from faster_whisper import WhisperModel

    if device == "cuda":
        _setup_cuda_dlls()
    return WhisperModel(str(path), device=device, compute_type=compute_type)


def load_model(name: str, device: str = "auto", compute_type: str = "auto"):
    """加载（或命中缓存）faster-whisper 模型，返回 (model, device, compute, warning)。"""
    device, compute_type = _resolve(device, compute_type)
    key = (name, device, compute_type)
    with _lock():
        if key in _cache:
            _cache.move_to_end(key)
            return _cache[key], device, compute_type, ""

    path = modelscope_dl.ensure_ready(name)
    warning = ""
    try:
        model = _build_model(path, device, compute_type)
    except Exception as e:
        if device == "cuda":
            log.warning("CUDA 加载失败（%s），回退 CPU int8", e)
            warning = f"CUDA 不可用（{str(e)[:120]}），已回退 CPU"
            device, compute_type = "cpu", "int8"
            key = (name, device, compute_type)
            with _lock():
                if key in _cache:
                    return _cache[key], device, compute_type, warning
            model = _build_model(path, device, compute_type)
        else:
            raise
    with _lock():
        _cache[key] = model
        _cache.move_to_end(key)
        while len(_cache) > MAX_LOADED_MODELS:
            _cache.popitem(last=False)
    log.info("模型 %s 已加载: device=%s compute=%s", name, device, compute_type)
    return model, device, compute_type, warning


# Whisper 在静音/音乐段常见的幻觉文本（小写匹配）
_HALLUCINATION_SUBSTRINGS = [
    "amara.org", "subtitles by", "subtitle by", "字幕by", "字幕由",
    "请不吝点赞", "请订阅", "感谢观看", "谢谢观看", "ご視聴ありがとう",
    "チャンネル登録", "字幕组", "请点赞", "字幕制作",
]
_HALLUCINATION_EXACT = {
    "字幕", "字幕by", "谢谢你", "谢谢", "大家好", "明石市", "子",
}


def flag_hallucination(text: str) -> bool:
    t = (text or "").strip().lower()
    if not t or len(t) > 60:
        return False
    if t in _HALLUCINATION_EXACT:
        return True
    return any(p in t for p in _HALLUCINATION_SUBSTRINGS)


def _run_transcribe(model, wav_path, language, duration, progress_cb, cancel_cb, meta_base, vad: bool):
    t0 = time.time()
    kwargs = dict(
        language=language,
        task="transcribe",
        beam_size=5,
        condition_on_previous_text=False,
    )
    if vad:
        kwargs.update(vad_filter=True, vad_parameters={"min_silence_duration_ms": 500})
    raw_iter, info = model.transcribe(wav_path, **kwargs)
    detected = getattr(info, "language", None) or language or "unknown"

    segments = []
    last_beat = 0.0
    for seg in raw_iter:
        if cancel_cb and cancel_cb():
            raise ASRCanceled("用户取消了识别")
        text = (seg.text or "").strip()
        if not text:
            continue
        segments.append(
            {
                "index": len(segments),
                "start": round(float(seg.start), 3),
                "end": round(float(seg.end), 3),
                "text": text,
                "flagged": flag_hallucination(text),
            }
        )
        if duration and progress_cb:
            frac = min(1.0, float(seg.end) / duration)
            now = time.time()
            if now - last_beat > 0.5 or frac >= 1.0:
                last_beat = now
                progress_cb(frac)

    asr_seconds = time.time() - t0
    meta = {**meta_base, "language": detected,
            "language_probability": round(float(getattr(info, "language_probability", 0) or 0), 3),
            "asr_seconds": round(asr_seconds, 1)}
    if duration:
        meta["speed_ratio"] = round(duration / max(asr_seconds, 0.1), 2)
    return segments, meta


def transcribe(
    wav_path: str,
    *,
    model_name: str,
    device: str = "auto",
    compute_type: str = "auto",
    source_lang: str | None = None,
    duration: float | None = None,
    vad: str = "auto",  # auto / on / off
    progress_cb=None,   # progress_cb(frac: float)
    cancel_cb=None,     # cancel_cb() -> bool
):
    """执行转写，返回 (segments, meta)。

    vad=auto：先开 VAD（过滤静音/音乐）识别；若整段音频被 VAD 过滤光（0 段，
    典型于歌曲/音乐 MV），自动关闭 VAD 重跑一次。
    """
    model, device, compute_type, warning = load_model(model_name, device, compute_type)
    language = source_lang if source_lang in ("zh", "en", "ja") else None
    meta_base = {
        "backend": "faster-whisper",
        "model": model_name,
        "device": device,
        "compute_type": compute_type,
        "warning": warning,
    }

    def _attempt(use_vad: bool, meta_extra: dict):
        try:
            return _run_transcribe(
                model, wav_path, language, duration, progress_cb, cancel_cb,
                {**meta_base, **meta_extra}, use_vad,
            )
        except ASRCanceled:
            raise
        except Exception as e:
            if device == "cuda" and _CUDA_ERR_RE.search(str(e)):
                # 构建期探测通过但运行库缺失/内核不兼容：清缓存后回退 CPU
                log.warning("CUDA 运行时故障（%s），整段回退 CPU 重跑", e)
                with _lock():
                    _cache.pop((model_name, device, compute_type), None)
                cpu_model, _, _, _ = load_model(model_name, "cpu", "int8")
                return _run_transcribe(
                    cpu_model, wav_path, language, duration, progress_cb, cancel_cb,
                    {**meta_base, **meta_extra, "device": "cpu", "compute_type": "int8",
                     "warning": f"CUDA 运行失败（{str(e)[:120]}），已用 CPU 完成"},
                    use_vad,
                )
            raise

    vad_mode = vad if vad in ("auto", "on", "off") else "auto"
    meta_extra: dict = {}
    if vad_mode != "off":
        segments, meta = _attempt(True, meta_extra)
        if not segments and vad_mode == "auto":
            # VAD 把整段音频过滤光了（歌曲/MV 常见）：关闭 VAD 重试
            log.info("VAD 过滤后 0 段，自动关闭 VAD 重试")
            meta_extra["vad_retry"] = True
            segments, meta = _attempt(False, meta_extra)
    else:
        segments, meta = _attempt(False, meta_extra)
    log.info(
        "ASR 完成: %d 段, 语言=%s, device=%s, 耗时 %.1fs",
        len(segments), meta.get("language"), meta.get("device"), meta.get("asr_seconds", 0),
    )
    return segments, meta
