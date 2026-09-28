"""字幕文件输出：SRT / VTT / ASS / JSON。"""
from __future__ import annotations

import json
from pathlib import Path

OUTPUT_FILES = {
    "orig_srt": "orig.srt",
    "trans_srt": "trans.srt",
    "bilingual_srt": "bilingual.srt",
    "bilingual_ass": "bilingual.ass",
    "bilingual_vtt": "bilingual.vtt",
    "json": "segments.json",
}


def _clamp(t: float) -> float:
    return max(0.0, t)


def fmt_srt(t: float) -> str:
    t = _clamp(t)
    h, rem = divmod(t, 3600)
    m, s = divmod(rem, 60)
    return f"{int(h):02d}:{int(m):02d}:{int(s):02d},{int(round((s - int(s)) * 1000)):03d}"


def fmt_vtt(t: float) -> str:
    t = _clamp(t)
    h, rem = divmod(t, 3600)
    m, s = divmod(rem, 60)
    return f"{int(h):02d}:{int(m):02d}:{int(s):02d}.{int(round((s - int(s)) * 1000)):03d}"


def fmt_ass(t: float) -> str:
    t = _clamp(t)
    h, rem = divmod(t, 3600)
    m, s = divmod(rem, 60)
    return f"{int(h)}:{int(m):02d}:{s:05.2f}"


_PUNCT = "，。！？、；：…—,.!?;: "


def _wrap(text: str, max_chars: int) -> str:
    """超长行按标点/空格在中点附近折成两行。"""
    text = (text or "").strip()
    if max_chars <= 0 or len(text) <= max_chars:
        return text
    mid = len(text) // 2
    best = None
    for i in range(len(text) - 1, 0, -1):
        if text[i] in _PUNCT and abs(i - mid) <= len(text) // 4 + 4:
            best = i + 1
            break
    if best is None:
        best = mid
    return text[:best].rstrip() + "\n" + text[best:].lstrip()


def _pair(seg: dict, mode: str, order: str) -> str:
    orig = (seg.get("text") or "").strip()
    trans = (seg.get("translated") or "").strip() or orig
    if mode == "orig":
        return orig
    if mode == "trans":
        return trans
    lines = [trans, orig] if order == "trans_first" else [orig, trans]
    return "\n".join(lines)


def write_srt(path: Path, segments: list[dict], mode: str, order: str = "trans_first", max_chars: int = 42) -> Path:
    out = []
    for i, seg in enumerate(segments, 1):
        text = _wrap(_pair(seg, mode, order), max_chars)
        out.append(f"{i}\n{fmt_srt(seg['start'])} --> {fmt_srt(seg['end'])}\n{text}\n")
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text("\n".join(out), encoding="utf-8-sig")
    return path


def write_vtt(path: Path, segments: list[dict], order: str = "trans_first", max_chars: int = 42) -> Path:
    out = ["WEBVTT", ""]
    for seg in segments:
        text = _wrap(_pair(seg, "bilingual", order), max_chars)
        out.append(f"{fmt_vtt(seg['start'])} --> {fmt_vtt(seg['end'])}")
        out.append(text)
        out.append("")
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text("\n".join(out), encoding="utf-8")
    return path


_ASS_HEADER = """[Script Info]
ScriptType: v4.00+
PlayResX: 1920
PlayResY: 1080
WrapStyle: 0
ScaledBorderAndShadow: yes

[V4+ Styles]
Format: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding
Style: Trans,Microsoft YaHei,62,&H00FFFFFF,&H000000FF,&H00000000,&H96000000,0,0,0,0,100,100,0,0,1,2.2,1,2,80,80,72,1

[Events]
Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text
"""


def write_ass(path: Path, segments: list[dict], order: str = "trans_first", max_chars: int = 42) -> Path:
    """单条事件双语输出：译文大字 + 原文小字灰色（行内覆盖标签）。"""
    events = []
    for seg in segments:
        trans = _wrap(seg.get("translated") or seg.get("text") or "", max_chars)
        orig = _wrap(seg.get("text") or "", max_chars)
        trans = trans.replace("\n", "\\N")
        orig = orig.replace("\n", "\\N")
        if order == "trans_first":
            text = f"{trans}{{\\fs42\\c&HD8D8D8&}}\\N{orig}"
        else:
            text = f"{orig}{{\\fs42\\c&HD8D8D8&}}\\N{trans}"
        events.append(
            f"Dialogue: 0,{fmt_ass(seg['start'])},{fmt_ass(seg['end'])},Trans,,0,0,0,,{text}"
        )
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(_ASS_HEADER + "\n".join(events) + "\n", encoding="utf-8-sig")
    return path


def write_json(path: Path, segments: list[dict]) -> Path:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(segments, ensure_ascii=False, indent=1), encoding="utf-8")
    return path


def write_all(out_dir: Path, segments: list[dict], out_cfg: dict | None = None) -> dict:
    """在 out_dir（通常为 output/{视频名}/）写出全部产物，返回 type → 绝对路径。"""
    out_cfg = out_cfg or {}
    order = out_cfg.get("bilingual_order", "trans_first")
    max_chars = int(out_cfg.get("max_line_chars") or 42)
    out_dir = Path(out_dir)
    paths = {
        "orig_srt": write_srt(out_dir / "orig.srt", segments, "orig", order, max_chars),
        "trans_srt": write_srt(out_dir / "trans.srt", segments, "trans", order, max_chars),
        "bilingual_srt": write_srt(out_dir / "bilingual.srt", segments, "bilingual", order, max_chars),
        "bilingual_ass": write_ass(out_dir / "bilingual.ass", segments, order, max_chars),
        "bilingual_vtt": write_vtt(out_dir / "bilingual.vtt", segments, order, max_chars),
        "json": write_json(out_dir / "segments.json", segments),
    }
    return {k: str(p) for k, p in paths.items()}
