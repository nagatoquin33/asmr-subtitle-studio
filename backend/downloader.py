"""yt-dlp 封装：解析/下载 bilibili、YouTube 等平台的音视频。"""
from __future__ import annotations

import logging
import time
from pathlib import Path

from . import ffmpeg_utils

log = logging.getLogger(__name__)


class DownloadCanceled(RuntimeError):
    pass


def humanize_mb(n: float | None) -> str:
    if not n:
        return "?"
    return f"{n / 1048576:.1f}MB"


def format_choice(height: int) -> str:
    # 视频画质上限 + 最优音频；无独立视频流时退回单文件
    return (
        f"bv*[height<={height}]+ba/b[height<={height}]/bv*+ba/b"
    )


def _human_error(msg: str) -> str:
    low = msg.lower()
    hint = ""
    if "unsupported url" in low:
        hint = "不支持该链接（yt-dlp 无法识别）"
    elif "login" in low or "sign in" in low or "cookies" in low or "登录" in low:
        hint = "该视频需要登录：可在设置里配置浏览器 cookies（chrome/edge/firefox）后重试"
    elif "private" in low or "会员" in low or "charge" in low:
        hint = "该视频为私密/付费内容，无法下载"
    elif "timed out" in low or "connection" in low or "unreachable" in low or "unable to download" in low:
        hint = "网络连接失败：若是 YouTube 等境外平台，请在设置里配置代理"
    if hint:
        return f"{hint}（{msg[:160]}）"
    return msg


def download_media(
    url: str,
    dest_dir: Path,
    *,
    height: int = 720,
    proxy: str = "",
    cookies_from_browser: str = "",
    progress_cb=None,   # progress_cb(done, total, speed)  字节
    cancel_cb=None,     # cancel_cb() -> bool
) -> dict:
    """下载音视频到 dest_dir（合并为 mp4），返回 {"path","title","duration","webpage_url"}。"""
    import yt_dlp

    dest_dir.mkdir(parents=True, exist_ok=True)
    last_update = {"t": 0.0}

    def hook(d):
        if cancel_cb and cancel_cb():
            raise DownloadCanceled("用户取消了下载")
        if d.get("status") == "downloading" and progress_cb:
            now = time.time()
            if now - last_update["t"] > 0.4 or d.get("status") == "finished":
                last_update["t"] = now
                progress_cb(
                    d.get("downloaded_bytes") or 0,
                    d.get("total_bytes") or d.get("total_bytes_estimate") or 0,
                    d.get("speed"),
                )

    ff_dir = str(Path(ffmpeg_utils.ffmpeg_path() or "ffmpeg").parent)
    opts: dict = {
        "format": format_choice(height),
        "outtmpl": str(dest_dir / "%(title).60B-%(id).20s.%(ext)s"),
        "windowsfilenames": True,
        "noplaylist": True,
        "merge_output_format": "mp4",
        "concurrent_fragment_downloads": 4,
        "retries": 5,
        "fragment_retries": 5,
        "quiet": True,
        "no_warnings": True,
        "noprogress": True,
        "ffmpeg_location": ff_dir,
        "progress_hooks": [hook],
    }
    if proxy:
        opts["proxy"] = proxy.strip()
    if cookies_from_browser:
        opts["cookiesfrombrowser"] = (cookies_from_browser.strip(),)

    try:
        with yt_dlp.YoutubeDL(opts) as ydl:
            info = ydl.extract_info(url, download=True)
            if info and info.get("_type") == "playlist":
                entries = [e for e in (info.get("entries") or []) if e]
                if not entries:
                    raise RuntimeError("链接没有可下载的视频")
                info = entries[0]
            requested = info.get("requested_downloads") or []
            final = requested[0].get("filepath") if requested else None
            if not final or not Path(final).exists():
                # 兜底：取目录里最新的媒体文件
                cands = sorted(dest_dir.glob("*"), key=lambda p: p.stat().st_mtime, reverse=True)
                final = next((str(p) for p in cands if p.suffix.lower() in (".mp4", ".mkv", ".webm", ".m4a", ".mp3", ".flac", ".wav")), None)
            if not final:
                raise RuntimeError("下载完成但未找到产物文件")
    except DownloadCanceled:
        raise
    except yt_dlp.utils.DownloadError as e:
        log.warning("下载失败 %s：%s", url, e)
        raise RuntimeError(_human_error(str(e))) from e

    duration = info.get("duration")
    result = {
        "path": str(Path(final)),
        "title": (info.get("title") or Path(final).stem)[:120],
        "duration": float(duration) if duration else None,
        "webpage_url": info.get("webpage_url") or url,
    }
    log.info("下载完成: %s (%s)", result["title"], humanize_mb(Path(final).stat().st_size))
    return result
