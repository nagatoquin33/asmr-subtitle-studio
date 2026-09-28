"""OpenAI 兼容 LLM 客户端封装（懒加载 openai，供翻译/审校/单句重翻等模块复用）。

设计要点：

- ``openai`` 是可选依赖：只有在真正发起网络请求的路径上才会 import，
  因此 mock 模式与纯函数级测试可以在零第三方依赖的环境下运行。
- ``chat_with_retry`` 对 429 / 5xx / 超时 / 连接错误按指数退避重试（基础 2s）；
  其余 4xx 属于请求本身的问题，直接抛出、不重试。
- ``test_connection`` 发送一条极小的聊天请求做连通性测试，
  返回 ``{"ok": bool, "message": str, "sample": str}``。

本模块放在 ``backend/`` 顶层而非 translate 包内：它是与业务无关的传输层，
后续「单句重翻」等接口（POST /api/tasks/{id}/segments/{index}/retranslate）
也要复用同一客户端与重试语义。
"""

from __future__ import annotations

import asyncio
import logging
from typing import Any

logger = logging.getLogger(__name__)

__all__ = ["build_client", "chat_with_retry", "test_connection"]

DEFAULT_TIMEOUT: float = 120.0    # 单次请求超时（秒）
DEFAULT_BASE_DELAY: float = 2.0   # 重试退避基数（秒）
MAX_DELAY: float = 60.0           # 单次退避等待上限（秒）
_RATE_LIMIT_STATUS = 429          # 限流：可重试
_RETRIABLE_5XX_FROM = 500         # >= 500 的服务端错误：可重试


def _import_openai() -> Any:
    """懒加载 openai —— 保证 mock / 零依赖环境下永远不会触发 import。"""
    import openai  # 特意延迟到真正发请求时才加载

    return openai


def build_client(cfg: dict) -> Any:
    """构建 ``openai.AsyncOpenAI`` 兼容客户端（OpenAI 兼容协议）。

    仅在非 mock 的真实请求路径上调用。``timeout`` 默认 120 秒，
    可通过 cfg["timeout"]（秒）覆盖。
    """
    openai = _import_openai()
    return openai.AsyncOpenAI(
        base_url=str(cfg.get("base_url") or "") or None,
        api_key=str(cfg.get("api_key") or ""),
        timeout=float(cfg.get("timeout") or DEFAULT_TIMEOUT),
    )


def _is_retriable(exc: BaseException, openai: Any) -> bool:
    """判断异常是否可重试：429 / 5xx / 超时 / 连接错误 → True；其余 4xx → False。"""
    if isinstance(exc, (asyncio.TimeoutError, TimeoutError, ConnectionError)):
        return True
    # 优先用 openai 的异常类型精确判断
    api_timeout = getattr(openai, "APITimeoutError", None)
    if api_timeout is not None and isinstance(exc, api_timeout):
        return True
    api_conn = getattr(openai, "APIConnectionError", None)
    if api_conn is not None and isinstance(exc, api_conn):
        return True
    api_status = getattr(openai, "APIStatusError", None)
    if api_status is not None and isinstance(exc, api_status):
        status = getattr(exc, "status_code", None)
        return isinstance(status, int) and (
            status == _RATE_LIMIT_STATUS or status >= _RETRIABLE_5XX_FROM
        )
    # 兜底：不依赖 openai 类型，看异常对象是否携带 status_code
    status = getattr(exc, "status_code", None)
    if isinstance(status, int):
        return status == _RATE_LIMIT_STATUS or status >= _RETRIABLE_5XX_FROM
    return False


def _retry_delay(exc: BaseException, attempt: int, base_delay: float) -> float:
    """指数退避：2s、4s、8s...；若服务端给了 Retry-After 则取较大值。"""
    delay = base_delay * (2**attempt)
    resp = getattr(exc, "response", None)
    headers = getattr(resp, "headers", None)
    if headers is not None:
        try:
            retry_after = headers.get("retry-after")
            if retry_after:
                delay = max(delay, float(retry_after))
        except (TypeError, ValueError):
            pass
    return min(delay, MAX_DELAY)


async def chat_with_retry(
    client: Any,
    *,
    model: str,
    messages: list[dict[str, str]],
    temperature: float = 0.2,
    max_retries: int = 3,
    base_delay: float = DEFAULT_BASE_DELAY,
) -> str:
    """非流式 ``chat.completions.create``，带指数退避重试；返回首条回复文本。

    - 可重试：429 / 5xx / 超时 / 连接错误，共重试 ``max_retries`` 次
      （即最多 ``max_retries + 1`` 次请求），退避 2s、4s、8s...
    - 不可重试（多为 4xx 参数/鉴权问题）：立即抛出。
    - 返回空 ``choices`` 或空 content 时按空字符串处理，
      由上层批处理协议的"解析不完整 → 重试 → 兜底对齐"逻辑兜底。
    """
    openai = _import_openai()
    for attempt in range(max_retries + 1):
        try:
            resp = await client.chat.completions.create(
                model=model,
                messages=messages,
                temperature=temperature,
                stream=False,
            )
        except Exception as exc:
            if not _is_retriable(exc, openai):
                logger.warning(
                    "LLM 请求失败（不可重试，直接抛出）：%s: %s", type(exc).__name__, exc
                )
                raise
            if attempt >= max_retries:
                logger.error(
                    "LLM 请求失败，已达最大重试次数（%d）：%s: %s",
                    max_retries,
                    type(exc).__name__,
                    exc,
                )
                raise
            delay = _retry_delay(exc, attempt, base_delay)
            logger.warning(
                "LLM 请求失败（%s: %s），%.1f 秒后进行第 %d/%d 次重试",
                type(exc).__name__,
                exc,
                delay,
                attempt + 1,
                max_retries,
            )
            await asyncio.sleep(delay)
            continue
        choices = getattr(resp, "choices", None) or []
        if not choices:
            logger.warning("LLM 返回空 choices（model=%s），按空回复处理", model)
            return ""
        content = getattr(choices[0].message, "content", None)
        return content or ""
    raise RuntimeError("chat_with_retry: unreachable")  # pragma: no cover


def _cap_retries(cfg: dict) -> int:
    """连通性测试的重试上限：取 cfg.max_retries 与 2 的较小值，保证测试足够快。"""
    try:
        return max(0, min(int(cfg.get("max_retries", 3)), 2))
    except (TypeError, ValueError):
        return 2


async def test_connection(cfg: dict) -> dict:
    """翻译 API 连通性测试。

    - ``cfg["mock"]=True``：不发网络请求，直接返回 ok。
    - 真实模式：发送一条极小的聊天请求（"请只回复：OK"），返回模型原文作为 sample。
    - 返回 ``{"ok": bool, "message": str, "sample": str}``，绝不抛异常。
    """
    cfg = cfg or {}
    if cfg.get("mock"):
        return {
            "ok": True,
            "message": "mock 模式：未发起网络请求，连通性测试直接通过",
            "sample": "[mock] OK",
        }
    model = str(cfg.get("model") or "")
    try:
        client = build_client(cfg)
        text = await chat_with_retry(
            client,
            model=model,
            messages=[{"role": "user", "content": "请只回复两个字符：OK"}],
            temperature=0.0,
            max_retries=_cap_retries(cfg),
        )
    except Exception as exc:  # noqa: BLE001 —— 对外只报告，不抛出
        logger.warning("翻译 API 连通性测试失败：%s: %s", type(exc).__name__, exc)
        message = f"{type(exc).__name__}: {exc}".strip()
        return {"ok": False, "message": message[:300], "sample": ""}
    sample = (text or "").strip()
    if not sample:
        return {"ok": False, "message": "连接成功，但模型返回内容为空", "sample": ""}
    return {"ok": True, "message": f"连接成功（model={model or '未知'}）", "sample": sample[:500]}
