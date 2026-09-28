"""翻译 / 审校模块测试 —— 仅依赖标准库 + 本模块，零第三方依赖。

运行方式（项目根目录下）::

    python -m backend.tests.test_translate

覆盖范围：

1. mock 翻译：50 句（含空句、♪、超长句）→ 等长、顺序对齐、progress_cb 累计 done=50
2. mock 审校：等长原样返回
3. test_connection mock 直接 ok
4. parse_reply 纯函数级：正常 / 混入解释文本 / 缺行 / 重复行 / 格式变体 / 全错返回空
5. align_fallback 兜底对齐：缺行时长度不变、缺位为空串
6. 非 mock 真实批处理路径（离线驱动）：并发乱序完成仍按输入顺序输出、术语表注入
7. 纠错重试：第一次输出缺行 → 第二次请求带纠错说明 → 最终完整
8. 兜底对齐端到端：模型始终缺行 → 输出长度不变、缺位空串、打 warning
9. 上下文一致性：concurrency=1 时第二批请求携带前 3 句已定稿译文
10. 零依赖验证：全程未 import openai
"""

from __future__ import annotations

import asyncio
import logging
import sys
import traceback
from typing import Any

from backend.translate import Proofreader, SubtitleTranslator, test_connection
from backend.translate.translator import (
    _LINE_RE,
    align_fallback,
    batch_complete,
    mock_translate_text,
    parse_reply,
)

TESTS: list[Any] = []


def test(fn):
    """注册测试用例。"""
    TESTS.append(fn)
    return fn


def _seg(i: int, text: str) -> dict:
    return {"index": i, "start": float(i) * 2.0, "end": float(i) * 2.0 + 2.0, "text": text}


# ---------------------------------------------------------------------------
# 1. mock 翻译
# ---------------------------------------------------------------------------


@test
async def test_mock_translate_50_segments():
    texts: list[str] = []
    for i in range(50):
        if i % 7 == 0:
            texts.append("")  # 空句
        elif i % 7 == 1:
            texts.append("♪")  # 纯符号
        elif i % 7 == 2:
            texts.append("こんばんは、お兄ちゃん。今日も一日お疲れさま。")
        elif i % 7 == 3:
            texts.append("——")  # 纯符号
        elif i % 7 == 4:
            texts.append("Long whisper line: " + "softer and slower " * 40)  # 超长句
        else:
            texts.append(f"セグメント{i}のセリフです。")

    segs = [_seg(i, t) for i, t in enumerate(texts)]
    calls: list[tuple[int, int, str]] = []

    def cb(done: int, total: int, message: str) -> None:
        calls.append((done, total, message))

    cfg = {"mock": True, "batch_size": 7, "concurrency": 3}
    out = await SubtitleTranslator(cfg).translate(segs, "ja", "zh", cb)

    assert len(out) == 50, f"输出长度 {len(out)} != 50"
    for i, t in enumerate(texts):
        expected = mock_translate_text(t)
        assert out[i] == expected, f"第 {i} 句未对齐：{out[i]!r} != {expected!r}"
    # 空句与纯符号原样返回（i%7==0 → 空句；i%7==1 → ♪；i%7==3 → ——）
    assert out[0] == "" and out[7] == "" and out[8] == "♪" and out[10] == "——"
    # progress_cb：按批调用，done 单调累计到 50
    assert calls, "progress_cb 未被调用"
    assert len(calls) == 8, f"批次回调次数 {len(calls)} != 8（ceil(50/7)）"
    dones = [c[0] for c in calls]
    assert dones == sorted(dones), f"done 非单调递增：{dones}"
    assert dones[-1] == 50 and calls[-1][1] == 50, f"done 累计错误：{dones[-1]}"
    assert all("翻译批次" in c[2] for c in calls), f"消息格式异常：{calls[0][2]!r}"


# ---------------------------------------------------------------------------
# 2. mock 审校
# ---------------------------------------------------------------------------


@test
async def test_mock_proofread_alignment():
    segs = [_seg(i, f"原文{i}") for i in range(12)]
    translated = [f"初译{i}" for i in range(12)]
    calls: list[tuple[int, int, str]] = []

    out = await Proofreader({"mock": True, "batch_size": 5}).proofread(
        segs, translated, "zh", lambda d, t, m: calls.append((d, t, m))
    )
    assert out == translated, "mock 审校应原样返回初译"
    assert len(calls) == 3 and calls[-1][0] == 12, f"进度回调异常：{calls}"


# ---------------------------------------------------------------------------
# 3. test_connection（mock）
# ---------------------------------------------------------------------------


@test
async def test_mock_test_connection():
    result = await test_connection({"mock": True})
    assert result["ok"] is True, f"mock 连通性应 ok：{result}"
    assert isinstance(result["message"], str) and result["message"]
    assert isinstance(result["sample"], str) and result["sample"]


# ---------------------------------------------------------------------------
# 4. parse_reply 纯函数级
# ---------------------------------------------------------------------------


@test
async def test_parse_reply_variants():
    # 正常解析
    assert parse_reply("1|你好\n2|早上好") == {1: "你好", 2: "早上好"}
    # 格式变体：无空格 / 竖线后一个空格 / 竖线前空格 / 竖线后空白（译文为空）
    assert parse_reply("1|x\n2| y\n3 |z\n4| ") == {1: "x", 2: "y", 3: "z", 4: ""}
    # 多余解释文本混入 → 全部忽略
    noisy = "好的，以下是翻译：\n1|a\n（说明：仅供参考）\n2|b\n祝收听愉快♪"
    assert parse_reply(noisy, [1, 2]) == {1: "a", 2: "b"}
    # 缺行 → 返回解析到的部分
    assert parse_reply("1|a\n3|c", [1, 2, 3]) == {1: "a", 3: "c"}
    # 重复行 → 保留首次出现
    assert parse_reply("1|a\n1|b\n2|c") == {1: "a", 2: "c"}
    # 全错（无任何协议行）→ 空 dict
    assert parse_reply("完全不是协议格式的输出\n随便两行文字") == {}
    # expected_ids 白名单过滤：序号全不在范围内 → 空 dict
    assert parse_reply("37|x\n38|y", [1, 2]) == {}
    # 多位数字行号
    assert parse_reply("10|十\n11|十一") == {10: "十", 11: "十一"}
    # 译文文本本身含竖线 → 取第一个竖线之后的全部
    assert parse_reply("1|a|b") == {1: "a|b"}
    # batch_complete 校验
    assert batch_complete({1: "a"}, [1]) is True
    assert batch_complete({1: "a"}, [1, 2]) is False
    assert batch_complete({}, [1]) is False


# ---------------------------------------------------------------------------
# 5. align_fallback 兜底对齐
# ---------------------------------------------------------------------------


@test
async def test_align_fallback_keeps_length():
    # 缺 2 行：长度不变，缺位为空串
    out = align_fallback({1: "a", 2: "b", 4: "d"}, [1, 2, 3, 4, 5])
    assert out == ["a", "b", "", "d", ""], f"兜底对齐结果异常：{out!r}"
    # 序号全不可用 → 全空串但长度不变
    assert align_fallback({9: "x"}, [1, 2]) == ["", ""]
    # 空解析 / 空批次
    assert align_fallback({}, [1, 2, 3]) == ["", "", ""]
    assert align_fallback({1: "a"}, []) == []


# ---------------------------------------------------------------------------
# 6-9. 非 mock 真实批处理路径（通过覆写 _request_once 离线驱动，不 import openai）
# ---------------------------------------------------------------------------


def _fake_reply_for(user_content: str) -> str:
    """模拟一个遵守 N|译文 协议的模型：把请求里的 N|原文 行回译为 N|译:原文。"""
    lines = []
    for line in user_content.splitlines():
        m = _LINE_RE.match(line)
        if m:
            lines.append(f"{m.group(1)}|译:{m.group(2).strip()}")
    return "\n".join(lines)


@test
async def test_real_path_offline_concurrent_ordered():
    """并发批次乱序完成，输出仍严格按输入顺序对齐；术语表注入系统提示词。"""
    seen: list[list[dict[str, str]]] = []

    async def fake_request_once(messages, temperature=None):
        seen.append(messages)
        await asyncio.sleep(0.002 * (5 - len(seen)))  # 制造乱序完成：先发后到
        return _fake_reply_for(messages[-1]["content"])

    translator = SubtitleTranslator(
        {
            "mock": False,
            "base_url": "http://127.0.0.1:9/v1",
            "api_key": "test",
            "model": "fake-model",
            "batch_size": 10,
            "concurrency": 4,
            "glossary": {"囁き": "轻声耳语", "耳かき": "掏耳朵"},
        }
    )
    translator._request_once = fake_request_once  # 离线驱动：不构建真实客户端

    texts = [f"原文{i}" for i in range(35)]
    segs = [_seg(i, t) for i, t in enumerate(texts)]
    out = await translator.translate(segs, "ja", "zh")

    assert len(out) == 35, f"输出长度 {len(out)} != 35"
    for i, t in enumerate(texts):
        assert out[i] == f"译:{t}", f"第 {i} 句未对齐：{out[i]!r}"
    assert len(seen) == 4, f"批次请求数 {len(seen)} != 4"
    # 系统提示词包含术语表强制规则与领域要求
    sys_prompt = seen[0][0]["content"]
    assert "囁き → 轻声耳语" in sys_prompt, "术语表未注入系统提示词"
    assert "ASMR" in sys_prompt and "お兄ちゃん→哥哥" in sys_prompt
    # 用户消息为 N|原文 格式
    assert "1|原文0" in seen[0][-1]["content"]
    assert "10|原文9" in seen[0][-1]["content"]


@test
async def test_real_path_incomplete_retry_then_complete():
    """第一次输出缺行 → 附加纠错说明重试一次 → 第二次完整返回。"""
    calls: list[str] = []

    async def fake_request_once(messages, temperature=None):
        calls.append(messages[-1]["content"])
        if len(calls) == 1:
            return "1|甲\n2|乙"  # 缺第 3 句
        assert "不完整" in messages[-1]["content"], "第二次请求应附加纠错说明"
        assert "N|译文" in messages[-1]["content"], "纠错说明应包含输出格式要求"
        return "1|甲\n2|乙\n3|丙"

    translator = SubtitleTranslator({"mock": False, "batch_size": 3, "concurrency": 1})
    translator._request_once = fake_request_once

    out = await translator.translate([_seg(i, f"t{i}") for i in range(3)], "ja", "zh")
    assert out == ["甲", "乙", "丙"], f"重试后应完整：{out!r}"
    assert len(calls) == 2, f"应恰好请求 2 次：{len(calls)}"


@test
async def test_real_path_fallback_alignment():
    """模型始终缺行且带解释文本 → 兜底对齐：长度不变、缺位空串、打 warning。"""
    records: list[logging.LogRecord] = []

    class Capture(logging.Handler):
        def emit(self, record: logging.LogRecord) -> None:
            records.append(record)

    handler = Capture()
    mod_logger = logging.getLogger("backend.translate.translator")
    mod_logger.addHandler(handler)

    async def fake_request_once(messages, temperature=None):
        return "说明：\n1|甲\n3|丙"  # 永远缺第 2 句，且混入解释行

    translator = SubtitleTranslator(
        {"mock": False, "batch_size": 3, "concurrency": 1, "max_retries": 0}
    )
    translator._request_once = fake_request_once

    try:
        out = await translator.translate([_seg(i, f"t{i}") for i in range(3)], None, "zh")
    finally:
        mod_logger.removeHandler(handler)

    assert len(out) == 3, f"兜底对齐不得改变长度：{out!r}"
    assert out == ["甲", "", "丙"], f"兜底对齐结果异常：{out!r}"
    assert any(
        "兜底对齐" in r.getMessage() and "置空" in r.getMessage() for r in records
    ), "应打出兜底对齐 warning 日志"


@test
async def test_real_path_context_prefix():
    """concurrency=1 时，第二批请求的用户消息前缀携带前 3 句已定稿译文。"""
    seen: list[str] = []

    async def fake_request_once(messages, temperature=None):
        user = messages[-1]["content"]
        seen.append(user)
        return _fake_reply_for(user)

    translator = SubtitleTranslator({"mock": False, "batch_size": 5, "concurrency": 1})
    translator._request_once = fake_request_once

    segs = [_seg(i, f"原文{i}") for i in range(10)]
    out = await translator.translate(segs, "ja", "zh")

    assert len(out) == 10 and len(seen) == 2
    assert "前文译文（仅供参考，不要输出它们）" not in seen[0], "第一批不应有前文"
    assert "前文译文（仅供参考，不要输出它们）" in seen[1], "第二批应携带前文译文"
    # 前 3 句已定稿（绝对句号 2/3/4），且上下文行用 #N 译文： 格式，不污染 N| 解析
    assert "#2 译文：译:原文2" in seen[1]
    assert "#4 译文：译:原文4" in seen[1]
    assert out == [f"译:原文{i}" for i in range(10)]


# ---------------------------------------------------------------------------
# 其他边界
# ---------------------------------------------------------------------------


@test
async def test_empty_segments_and_cfg_tolerance():
    # 空输入 → 空输出
    assert await SubtitleTranslator({"mock": True}).translate([], "ja", "zh") == []
    assert await Proofreader({"mock": True}).proofread([], [], "zh") == []
    # cfg 缺键 / 未知键 / 非法值 → 全部给默认值，不抛异常
    translator = SubtitleTranslator({"unknown_key": 1, "batch_size": -5, "concurrency": 0})
    assert translator.batch_size >= 1 and translator.concurrency >= 1
    # 仅给 mock 开关、其余键全缺 → 正常离线运行
    out = await SubtitleTranslator({"mock": True}).translate([_seg(0, "hello")], None, "zh")
    assert out == ["[译]hello"]
    # 审校输入初译与原文不等长 → 容忍并等长返回
    pr = Proofreader({"mock": True})
    out2 = await pr.proofread([_seg(i, f"t{i}") for i in range(3)], ["a"], "zh")
    assert len(out2) == 3


# ---------------------------------------------------------------------------
# 运行器
# ---------------------------------------------------------------------------


def main() -> int:
    try:  # Windows 控制台编码兜底
        sys.stdout.reconfigure(encoding="utf-8", errors="replace")  # type: ignore[attr-defined]
    except Exception:  # noqa: BLE001
        pass

    print(f"backend.tests.test_translate：共 {len(TESTS)} 项测试（Python {sys.version.split()[0]}）")
    print("-" * 64)
    failed: list[str] = []
    passed: list[str] = []
    for fn in TESTS:
        name = fn.__name__
        try:
            asyncio.run(fn())
        except Exception:  # noqa: BLE001 —— 测试失败需要完整回溯
            failed.append(name)
            print(f"[FAIL] {name}")
            traceback.print_exc()
            print("-" * 64)
        else:
            passed.append(name)
            print(f"[PASS] {name}")
    print("-" * 64)
    print(f"结果：通过 {len(passed)} / {len(TESTS)}，失败 {len(failed)}")
    if "openai" not in sys.modules:
        print("零依赖验证：全程未 import openai")
    else:
        print("零依赖验证失败：openai 被意外导入！")
        failed.append("openai_must_not_be_imported")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
