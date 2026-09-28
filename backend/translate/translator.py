"""字幕翻译与审校执行器（实现 docs/API.md「翻译模块 Python 契约」）。

对外 API（签名不可更改）::

    from backend.translate import SubtitleTranslator, Proofreader, test_connection

设计要点
--------

批处理协议
    每 ``batch_size`` 句一批；用户消息以 ``N|原文`` 逐行给出，要求模型每行输出
    ``N|译文``。解析正则严格为 ``^\\s*(\\d+)\\s*\\|\\s?(.*)$``，逐行解析，
    忽略一切不合规行（解释、寒暄、回显等），同一序号保留首次出现的行。

完整性校验与重试
    解析后校验"数量一致且序号完整覆盖"；不完整则附加纠错说明重试一次
    （第二次请求携带第一轮对话历史 + 纠错说明）；仍失败则兜底对齐：
    按序号对齐能对上的行，缺失句填空串并打 warning。
    当行号完全不可用（如模型改用绝对编号）且未过滤解析行数恰好相等时，
    按输出顺序直接对齐。任何情况下都绝不改变返回列表的长度。

并发与顺序
    ``asyncio.Semaphore(cfg.concurrency)`` 限制批间并发；批次独立调度，
    结果按批内序号写回预分配槽位，输出顺序与输入严格对齐。
    单批彻底失败只把该批置空并告警，不拖垮整篇。

上下文一致性
    每批请求携带前 3 句"已定稿"译文（连续、遇未完成批次即止），
    以 ``前文译文（仅供参考，不要输出它们）`` 为用户消息前缀。
    并发 > 1 时相邻批可能尚未定稿，此时省略上下文（best-effort）；
    concurrency=1 时每批必然携带完整前文。
"""

from __future__ import annotations

import asyncio
import logging
import re
from typing import Any, Awaitable, Callable, Iterable, Sequence

from ..llm_client import build_client, chat_with_retry, test_connection
from .prompt import (
    build_correction_note,
    build_proofread_system_prompt,
    build_proofread_user_message,
    build_translate_system_prompt,
    build_translate_user_message,
)

__all__ = [
    "SubtitleTranslator",
    "Proofreader",
    "test_connection",
    "parse_reply",
    "batch_complete",
    "align_fallback",
    "mock_translate_text",
]

logger = logging.getLogger(__name__)

# 严格的行协议正则：N|译文（允许 N 前后空白与竖线后至多一个空格）
_LINE_RE = re.compile(r"^\s*(\d+)\s*\|\s?(.*)$")

ProgressCB = Callable[[int, int, str], None]
ProcessBatchFn = Callable[[int, list[int], list[str | None]], Awaitable[list[str]]]

# ---------------------------------------------------------------------------
# 纯函数级工具：解析 / 校验 / 兜底对齐 / mock 伪翻译
# ---------------------------------------------------------------------------


def parse_reply(text: str, expected_ids: Iterable[int] | None = None) -> dict[int, str]:
    """把模型回复解析为 ``{序号: 译文}``。

    - 逐行匹配 ``^\\s*(\\d+)\\s*\\|\\s?(.*)$``，忽略一切不合规行；
    - 同一序号出现多次时保留首次出现的行；
    - 提供 ``expected_ids`` 时仅保留其中的序号（批内白名单过滤）；
    - 完全无法解析时返回空 dict。
    """
    result: dict[int, str] = {}
    for raw in (text or "").splitlines():
        match = _LINE_RE.match(raw)
        if match is None:
            continue
        idx = int(match.group(1))
        if idx in result:  # 重复行：保留首次出现
            continue
        result[idx] = match.group(2).strip()
    if expected_ids is not None:
        allowed = set(expected_ids)
        result = {k: v for k, v in result.items() if k in allowed}
    return result


def batch_complete(parsed: dict[int, str], expected_ids: Sequence[int]) -> bool:
    """校验解析结果：数量一致且序号完整覆盖。"""
    return len(parsed) == len(expected_ids) and set(parsed) == set(expected_ids)


def align_fallback(parsed: dict[int, str], expected_ids: Sequence[int]) -> list[str]:
    """兜底对齐：序号能对上的行按其序号落位（dict 保持模型输出顺序），
    缺失句填空串并打 warning；序号不在期望范围内的行丢弃。

    任何情况下返回列表长度 == ``len(expected_ids)``，绝不抛出对齐错误。
    """
    out = [str(parsed.get(sid, "") or "") for sid in expected_ids]
    missing = [sid for sid in expected_ids if sid not in parsed]
    if missing:
        preview = ", ".join(str(i) for i in missing[:20])
        logger.warning(
            "兜底对齐：%d 句未能对齐，将置空处理（缺失序号：%s%s）",
            len(missing),
            preview,
            "..." if len(missing) > 20 else "",
        )
    return out


def mock_translate_text(text: str) -> str:
    """mock 伪翻译：``[译]{原文}``；空句 / 纯符号（♪、——、… 等）原样返回。"""
    t = text or ""
    if not t.strip():
        return t
    if not any(ch.isalnum() for ch in t):  # 不含任何文字/数字 → 纯符号
        return t
    return f"[译]{t}"


def _collect_context(
    start: int, pending: Sequence[str | None], limit: int = 3
) -> list[tuple[int, str]]:
    """收集 ``start`` 之前最多 ``limit`` 句"已定稿"译文。

    连续向前取，遇到未完成的批次（None）即止，保证上下文真实连续。
    """
    ctx: list[tuple[int, str]] = []
    i = start - 1
    while i >= 0 and len(ctx) < limit:
        t = pending[i]
        if t is None:
            break
        ctx.append((i, t))
        i -= 1
    ctx.reverse()
    return ctx


def _safe_progress(cb: ProgressCB | None, done: int, total: int, message: str) -> None:
    """调用进度回调；回调异常只记日志，绝不影响翻译主流程。"""
    if cb is None:
        return
    try:
        cb(done, total, message)
    except Exception:  # noqa: BLE001 —— 回调属于调用方代码
        logger.exception("progress_cb 回调抛出异常（已忽略）")


def _text_of(seg: Any) -> str:
    """从段对象取原文文本；容忍缺键/None/非 dict（退化处理）。"""
    if isinstance(seg, dict):
        return str(seg.get("text") or "")
    return str(seg or "")


def _as_int(value: Any, default: int, minimum: int = 1) -> int:
    try:
        n = int(value)
    except (TypeError, ValueError):
        return default
    return n if n >= minimum else minimum


def _as_float(value: Any, default: float) -> float:
    try:
        return float(value)
    except (TypeError, ValueError):
        return default


# ---------------------------------------------------------------------------
# 公共基类：配置解析、请求通道、批处理调度、批次级协议（解析→重试→兜底）
# ---------------------------------------------------------------------------


class _LLMService:
    """SubtitleTranslator / Proofreader 的公共配置与批处理基础设施。"""

    def __init__(self, cfg: dict | None) -> None:
        cfg = dict(cfg or {})
        self.cfg = cfg
        self.mock = bool(cfg.get("mock", False))
        self.temperature = _as_float(cfg.get("temperature"), 0.2)
        self.concurrency = _as_int(cfg.get("concurrency"), 3, minimum=1)
        self.batch_size = _as_int(cfg.get("batch_size"), 40, minimum=1)
        self.max_retries = _as_int(cfg.get("max_retries"), 3, minimum=0)
        glossary = cfg.get("glossary")
        self.glossary: dict[str, str] = (
            {str(k): str(v) for k, v in glossary.items()}
            if isinstance(glossary, dict)
            else {}
        )
        self._client: Any = None

    # -- 请求通道 -----------------------------------------------------------

    def _get_client(self) -> Any:
        """懒构建 AsyncOpenAI 客户端（首次真实请求时才 import openai）。"""
        if self._client is None:
            self._client = build_client(self.cfg)
        return self._client

    async def _request_once(
        self, messages: list[dict[str, str]], temperature: float | None = None
    ) -> str:
        """单次 LLM 请求（含网络层重试）。

        测试可在实例上覆写本方法，从而在零依赖（不 import openai）的
        环境下离线驱动真实的批处理 / 解析 / 兜底路径。
        """
        return await chat_with_retry(
            self._get_client(),
            model=str(self.cfg.get("model") or ""),
            messages=messages,
            temperature=self.temperature if temperature is None else temperature,
            max_retries=self.max_retries,
        )

    # -- mock 路径 -----------------------------------------------------------

    async def _mock_run(
        self,
        texts: Sequence[str],
        label: str,
        progress_cb: ProgressCB | None,
        transform: Callable[[str], str],
    ) -> list[str]:
        """mock 模式：离线确定性逐批变换，同样按批回报进度。"""
        n = len(texts)
        bs = self.batch_size
        total_batches = (n + bs - 1) // bs
        out: list[str] = []
        done = 0
        for bi, start in enumerate(range(0, n, bs)):
            chunk = texts[start : start + bs]
            out.extend(transform(t) for t in chunk)
            done += len(chunk)
            _safe_progress(progress_cb, done, n, f"{label} {bi + 1}/{total_batches}")
            await asyncio.sleep(0)  # 让出事件循环
        return out

    # -- 批处理调度 -----------------------------------------------------------

    async def _run_all(
        self,
        n: int,
        progress_label: str,
        progress_cb: ProgressCB | None,
        process_batch: ProcessBatchFn,
    ) -> list[str]:
        """按批并发调度，输出严格按输入顺序对齐。

        :param n: 总句数
        :param process_batch: ``async (批序号, 批内绝对索引列表, pending槽位) -> 该批译文列表``
        """
        bs = self.batch_size
        batches = [list(range(s, min(s + bs, n))) for s in range(0, n, bs)]
        pending: list[str | None] = [None] * n
        sem = asyncio.Semaphore(self.concurrency)
        counter = {"done": 0}
        lock = asyncio.Lock()

        async def run_one(bi: int, indices: list[int]) -> None:
            async with sem:
                try:
                    out = await process_batch(bi, indices, pending)
                except Exception:  # noqa: BLE001 —— 单批失败不拖垮整篇
                    logger.exception(
                        "%s：第 %d/%d 批处理失败，该批 %d 句将置空",
                        progress_label,
                        bi + 1,
                        len(batches),
                        len(indices),
                    )
                    out = [""] * len(indices)
                # 长度保护：任何情况下不允许改变最终列表长度
                if len(out) != len(indices):
                    logger.warning(
                        "第 %d 批返回长度 %d != 期望 %d，自动补空/截断",
                        bi + 1,
                        len(out),
                        len(indices),
                    )
                    out = (list(out) + [""] * len(indices))[: len(indices)]
                for j, text in enumerate(out):
                    pending[indices[j]] = text
                async with lock:
                    counter["done"] += len(indices)
                    _safe_progress(
                        progress_cb,
                        counter["done"],
                        n,
                        f"{progress_label} {bi + 1}/{len(batches)}",
                    )

        if batches:
            await asyncio.gather(*(run_one(bi, idxs) for bi, idxs in enumerate(batches)))
        return [t if t is not None else "" for t in pending]

    # -- 单批协议：请求 → 解析 → 纠错重试一次 → 兜底对齐 ----------------------

    async def _fetch_batch_aligned(
        self,
        items: Sequence[tuple[int, Any]],
        context: Sequence[tuple[int, str]],
        system_prompt: str,
        user_builder: Callable[[Sequence[tuple[int, Any]], Sequence[tuple[int, str]]], str],
    ) -> list[str]:
        """对一批条目执行完整的 N|译文 协议，返回与 items 等长的译文列表。"""
        ids = [no for no, _ in items]
        user_msg = user_builder(items, context)
        messages: list[dict[str, str]] = [
            {"role": "system", "content": system_prompt},
            {"role": "user", "content": user_msg},
        ]
        raw1 = await self._request_once(messages)
        parsed1 = parse_reply(raw1, ids)
        if batch_complete(parsed1, ids):
            return [parsed1[sid] for sid in ids]

        # 第一次解析不完整 → 附加纠错说明重试一次（携带第一轮对话历史）
        missing = [sid for sid in ids if sid not in parsed1]
        logger.warning(
            "批次解析不完整（%d/%d 行有效，缺失行号 %s），附加纠错说明重试一次",
            len(parsed1),
            len(ids),
            missing[:20],
        )
        messages2 = messages + [
            {"role": "assistant", "content": raw1},
            {"role": "user", "content": build_correction_note(missing)},
        ]
        raw2 = await self._request_once(messages2)
        parsed2 = parse_reply(raw2, ids)
        if batch_complete(parsed2, ids):
            return [parsed2[sid] for sid in ids]

        # 兜底对齐：取两次解析中覆盖率最高的一次
        best = parsed2 if len(parsed2) >= len(parsed1) else parsed1
        aligned = align_fallback(best, ids)
        if not any(aligned):
            # 行号完全不可用（如模型使用绝对编号）且行数恰好相等 → 按输出顺序直接对齐
            unfiltered = parse_reply(raw2) or parse_reply(raw1)
            if len(unfiltered) == len(ids):
                logger.warning(
                    "行号均无法对齐，改按模型输出顺序直接对齐 %d 行", len(ids)
                )
                aligned = list(unfiltered.values())
        return aligned


# ---------------------------------------------------------------------------
# 翻译器
# ---------------------------------------------------------------------------


class SubtitleTranslator(_LLMService):
    """字幕翻译器：整篇 segments → 与输入等长、按顺序对齐的译文列表。"""

    def __init__(self, cfg: dict) -> None:
        super().__init__(cfg)

    async def translate(
        self,
        segments: list[dict],
        source_lang: str | None,
        target_lang: str,
        progress_cb: ProgressCB | None = None,
    ) -> list[str]:
        """翻译整篇字幕。

        :param segments: ``[{"index": int, "start": float, "end": float, "text": str}, ...]``
        :param source_lang: ``"zh"/"en"/"ja"/None``（None = 未知，自动检测）
        :param target_lang: ``"zh"/"en"/"ja"``
        :param progress_cb: 同步回调 ``progress_cb(done, total, message)``，按批完成后调用
        :return: 与 segments 等长、按顺序对齐的译文列表
        """
        texts = [_text_of(seg) for seg in (segments or [])]
        n = len(texts)
        if n == 0:
            return []
        if self.mock:
            return await self._mock_run(texts, "翻译批次", progress_cb, mock_translate_text)

        target = str(target_lang or "zh")
        system_prompt = build_translate_system_prompt(source_lang, target, self.glossary)

        async def process_batch(
            bi: int, indices: list[int], pending: list[str | None]
        ) -> list[str]:
            items = [(k + 1, texts[i]) for k, i in enumerate(indices)]
            context = _collect_context(indices[0], pending)
            return await self._fetch_batch_aligned(
                items,
                context,
                system_prompt,
                lambda its, ctx: build_translate_user_message(its, ctx, target),
            )

        return await self._run_all(n, "翻译批次", progress_cb, process_batch)


# ---------------------------------------------------------------------------
# 审校器
# ---------------------------------------------------------------------------


class Proofreader(_LLMService):
    """字幕审校器：原文 + 初译 → 与输入等长、按顺序对齐的审校后译文。"""

    def __init__(self, cfg: dict) -> None:
        super().__init__(cfg)

    async def proofread(
        self,
        segments: list[dict],
        translated: list[str],
        target_lang: str,
        progress_cb: ProgressCB | None = None,
    ) -> list[str]:
        """审校整篇初译。

        :param segments: 原文段（结构同 translate 的 segments）
        :param translated: 初译结果（应与 segments 等长；不等长时容忍并对齐告警）
        :param target_lang: ``"zh"/"en"/"ja"``
        :param progress_cb: 同步回调 ``progress_cb(done, total, message)``，按批完成后调用
        :return: 审校后译文，长度恒等于 len(segments)
        """
        segs = list(segments or [])
        n = len(segs)
        if n == 0:
            return []
        drafts = [str(t or "") for t in (translated or [])]
        if len(drafts) < n:
            logger.warning(
                "审校输入初译 %d 句少于原文 %d 句，缺失句按空串处理", len(drafts), n
            )
            drafts = drafts + [""] * (n - len(drafts))
        elif len(drafts) > n:
            logger.warning(
                "审校输入初译 %d 句多于原文 %d 句，多余部分将被忽略", len(drafts), n
            )
            drafts = drafts[:n]

        if self.mock:
            # mock 审校：原样返回初译（逐批回报进度）
            return await self._mock_run(drafts, "审校批次", progress_cb, lambda t: t)

        target = str(target_lang or "zh")
        sources = [_text_of(seg) for seg in segs]
        system_prompt = build_proofread_system_prompt(target, self.glossary)

        async def process_batch(
            bi: int, indices: list[int], pending: list[str | None]
        ) -> list[str]:
            items = [(k + 1, (sources[i], drafts[i])) for k, i in enumerate(indices)]
            context = _collect_context(indices[0], pending)
            return await self._fetch_batch_aligned(
                items,
                context,
                system_prompt,
                lambda its, ctx: build_proofread_user_message(its, ctx, target),
            )

        return await self._run_all(n, "审校批次", progress_cb, process_batch)
