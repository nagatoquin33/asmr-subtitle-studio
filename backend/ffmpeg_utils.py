"""ffmpeg/ffprobe 工具：定位二进制、探测媒体信息、抽取音频。"""
from __future__ import annotations

import json
import logging
import re
import shutil
import subprocess
from pathlib import Path

from . import config

log = logging.getLogger(__name__)


class FFmpegError(RuntimeError):
    pass


class Canceled(RuntimeError):
    pass


def _candidate_dirs():
    return [
        config.PROJECT_ROOT / "tools" / "ffmpeg" / "bin",
        config.PROJECT_ROOT / "tools" / "ffmpeg",
        config.RESOURCE_DIR / "tools" / "ffmpeg" / "bin",
        config.RESOURCE_DIR / "tools" / "ffmpeg",
    ]


def _find(name: str) -> str | None:
    for d in _candidate_dirs():
        for exe in (f"{name}.exe", name):
            p = d / exe
            if p.exists():
                return str(p)
    which = shutil.which(name)
    return which


_ffmpeg: str | None = None
_ffprobe: str | None = None


def ffmpeg_path() -> str | None:
    global _ffmpeg
    if _ffmpeg is None:
        _ffmpeg = _find("ffmpeg")
    return _ffmpeg


def ffprobe_path() -> str | None:
    global _ffprobe
    if _ffprobe is None:
        _ffprobe = _find("ffprobe")
    return _ffprobe


def ffmpeg_version() -> str | None:
    ff = ffmpeg_path()
    if not ff:
        return None
    try:
        out = subprocess.run([ff, "-version"], capture_output=True, text=True, timeout=10)
        m = re.search(r"ffmpeg version (\S+)", out.stdout)
        return m.group(1) if m else "unknown"
    except Exception:
        return None


def probe(path: str | Path) -> dict:
    """返回 {"duration": float, "has_video": bool, "size": int}。"""
    fp = ffprobe_path()
    if not fp:
        raise FFmpegError("未找到 ffprobe，请检查 tools/ffmpeg 目录")
    cmd = [
        fp, "-v", "error", "-print_format", "json",
        "-show_format", "-show_streams", str(path),
    ]
    proc = subprocess.run(cmd, capture_output=True, text=True, timeout=120)
    if proc.returncode != 0:
        raise FFmpegError(f"ffprobe 失败: {proc.stderr.strip()[:300]}")
    data = json.loads(proc.stdout)
    duration = float(data.get("format", {}).get("duration") or 0)
    has_video = any(
        s.get("codec_type") == "video" and s.get("disposition", {}).get("attached_pic", 0) != 1
        for s in data.get("streams", [])
    )
    size = int(data.get("format", {}).get("size") or 0)
    return {"duration": duration, "has_video": has_video, "size": size}


def extract_audio(
    src: str | Path,
    dst: str | Path,
    duration: float | None = None,
    progress_cb=None,
    cancel_cb=None,
) -> None:
    """抽取 16k 单声道 PCM wav。progress_cb(frac: float)；cancel_cb() 返回 True 时中止。"""
    ff = ffmpeg_path()
    if not ff:
        raise FFmpegError("未找到 ffmpeg，请检查 tools/ffmpeg 目录")
    dst = Path(dst)
    dst.parent.mkdir(parents=True, exist_ok=True)
    cmd = [
        ff, "-y", "-i", str(src),
        "-vn", "-ac", "1", "-ar", "16000",
        "-c:a", "pcm_s16le",
        "-progress", "pipe:1", "-nostats",
        str(dst),
    ]
    proc = subprocess.Popen(cmd, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, encoding="utf-8", errors="replace")
    err_tail = ""
    try:
        assert proc.stdout is not None
        for line in proc.stdout:
            line = line.strip()
            if line.startswith("out_time_ms=") or line.startswith("out_time_us="):
                try:
                    t_us = int(line.split("=", 1)[1])
                    if duration and progress_cb and t_us > 0:
                        progress_cb(min(1.0, (t_us / 1_000_000) / duration))
                except ValueError:
                    pass
            elif line.startswith("progress=") and line.endswith("end"):
                if progress_cb:
                    progress_cb(1.0)
        proc.wait()
        if proc.stderr:
            err_tail = (proc.stderr.read() or "")[-500:]
    finally:
        if proc.poll() is None:
            if cancel_cb and cancel_cb():
                proc.kill()
                proc.wait()
                raise Canceled("用户取消了音频提取")

    if cancel_cb and cancel_cb():
        raise Canceled("用户取消了音频提取")
    if proc.returncode != 0:
        raise FFmpegError(f"ffmpeg 抽取音频失败: {err_tail.strip()[:300]}")
    if not dst.exists() or dst.stat().st_size == 0:
        raise FFmpegError("ffmpeg 未生成音频文件")
