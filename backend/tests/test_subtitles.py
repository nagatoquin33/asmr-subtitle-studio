"""字幕格式化与产物输出测试（纯本地，无依赖）。"""
from __future__ import annotations

import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent.parent))

from backend.subtitles import (  # noqa: E402
    _wrap,
    fmt_ass,
    fmt_srt,
    fmt_vtt,
    write_all,
)

PASS = 0


def check(name: str, cond: bool, detail: str = ""):
    global PASS
    assert cond, f"{name} 失败 {detail}"
    PASS += 1
    print(f"[PASS] {name}")


SEGS = [
    {"index": 0, "start": 0.0, "end": 2.73, "text": "Hello, and welcome.", "translated": "你好，欢迎。"},
    {"index": 1, "start": 2.73, "end": 8.25, "text": "Long line " * 8, "translated": "很" * 60},
]


def main() -> int:
    check("fmt_srt", fmt_srt(3661.5) == "01:01:01,500", fmt_srt(3661.5))
    check("fmt_srt 零", fmt_srt(0) == "00:00:00,000")
    check("fmt_vtt", fmt_vtt(3661.5) == "01:01:01.500", fmt_vtt(3661.5))
    check("fmt_ass", fmt_ass(3661.5) == "1:01:01.50", fmt_ass(3661.5))
    check("fmt_ass 亚秒", fmt_ass(2.73) == "0:00:02.73", fmt_ass(2.73))

    wrapped = _wrap("很" * 60, 42)
    check("超长行折两行", "\n" in wrapped and all(len(p) <= 42 for p in wrapped.split("\n")))
    check("短行不折", "\n" not in _wrap("短句", 42))

    with tempfile.TemporaryDirectory() as td:
        out = Path(td)
        paths = write_all(out, SEGS, {"bilingual_order": "trans_first", "max_line_chars": 42})
        check("write_all 六产物", set(paths) == {"orig_srt", "trans_srt", "bilingual_srt",
                                                "bilingual_ass", "bilingual_vtt", "json"})
        check("write_all 绝对路径", all(Path(p).is_absolute() for p in paths.values()), str(paths))
        srt = (out / "bilingual.srt").read_text(encoding="utf-8-sig")
        check("双语 SRT 译文在上", srt.index("你好，欢迎。") < srt.index("Hello, and welcome."))
        check("SRT 时间码", "00:00:00,000 --> 00:00:02,730" in srt)
        ass = (out / "bilingual.ass").read_text(encoding="utf-8-sig")
        check("ASS 样式与覆盖", "Style: Trans," in ass and "\\fs42" in ass)
        vtt = (out / "bilingual.vtt").read_text(encoding="utf-8")
        check("VTT 头", vtt.startswith("WEBVTT"))
        js = (out / "segments.json").read_text(encoding="utf-8")
        check("segments.json", '"translated": "你好，欢迎。"' in js)

    print(f"结果：通过 {PASS} 项")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
