"""管线纯决策函数测试：跳翻策略 / 伪翻译识别 / 产物目录规划 / resume 判定。"""
from __future__ import annotations

import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent.parent))

from backend import config  # noqa: E402
from backend.pipeline import (  # noqa: E402
    _looks_mock,
    load_existing_segments,
    plan_output_dir,
    segments_all_translated,
    should_skip_translation,
)

PASS = 0


def check(name: str, cond: bool, detail: str = ""):
    global PASS
    assert cond, f"{name} 失败 {detail}"
    PASS += 1
    print(f"[PASS] {name}")


def main() -> int:
    # 跳翻策略
    skip, why = should_skip_translation("zh", "zh", "ja", 0.44)
    check("手动指定同语言→跳过", skip and "手动" in why)
    skip, _ = should_skip_translation("auto", "zh", "zh", 0.44)
    check("自动+低置信度同语言→不跳过（防静默漏翻）", not skip)
    skip, why = should_skip_translation("auto", "zh", "zh", 0.99)
    check("自动+高置信度同语言→跳过", skip and "置信度" in why)
    skip, _ = should_skip_translation("auto", "zh", "ja", 1.0)
    check("语言不同→不跳过", not skip)
    skip, _ = should_skip_translation("auto", "zh", None, 0.0)
    check("未知语言→不跳过", not skip)

    # 伪翻译
    check("[译] 前缀视为伪翻译", _looks_mock("[译]こんにちは"))
    check("正常译文不算伪翻译", not _looks_mock("晚上好"))
    check("空串不算", not _looks_mock(""))
    check("混入正文不算", not _looks_mock("他说[译]这个词"))

    segs = [
        {"text": "a", "translated": "译a"},
        {"text": "b", "translated": "[译]b"},
    ]
    check("含伪翻译→视为未全译", not segments_all_translated(segs))
    check("全真译文→已全译", segments_all_translated([{"text": "a", "translated": "译"}]))
    check("空段→未全译", not segments_all_translated([]))
    check("None→未全译", not segments_all_translated(None))

    # 产物目录规划
    config.ensure_dirs()
    d1 = plan_output_dir("t_test_000001", "我的视频.mp4")
    check("产物目录在 output/ 下", d1.parent == config.OUTPUT_DIR)
    check("目录名净化", "/" not in d1.name and "\\" not in d1.name)
    d1.mkdir(parents=True, exist_ok=True)
    (d1 / "x.srt").write_text("x", encoding="utf-8")
    d2 = plan_output_dir("t_test_000002", "我的视频.mp4")
    check("同名冲突→追加 id 后缀", d2.name.endswith("_000002"), d2.name)

    # resume 判定
    with tempfile.TemporaryDirectory() as td:
        check("无 segments.json→None", load_existing_segments(Path(td)) is None)
        p = Path(td) / "segments.json"
        p.write_text('[{"index":0,"start":0,"end":1,"text":"你好"}]', encoding="utf-8")
        segs = load_existing_segments(Path(td))
        check("合法 segments 加载", segs and segs[0]["text"] == "你好")
        p.write_text("not-json", encoding="utf-8")
        check("坏 JSON→None", load_existing_segments(Path(td)) is None)
        p.write_text("[]", encoding="utf-8")
        check("空列表→None", load_existing_segments(Path(td)) is None)

    print(f"结果：通过 {PASS} 项")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
