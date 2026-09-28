"""翻译 / 审校模块的提示词与批处理用户消息构建。

包含：

- 默认系统提示词（翻译 / 审校两个角色，中文书写，只允许输出 ``N|译文``）
- 语言代码 → 语言名的映射（zh/en/ja/None → 中文/英文/日文/自动检测）
- 术语表（cfg["glossary"]）注入为强制替换规则
- 批处理用户消息构建：``N|原文`` 逐行 + 前文译文参考上下文
- 解析失败时的纠错说明文本

协议约定：批内行号 N 从 1 开始；模型每行输出 ``N|译文``。
参考上下文行使用 ``#绝对句号 译文：...`` 格式——刻意避免 ``数字|`` 开头，
防止模型回显上下文行时污染对 ``N|译文`` 的严格解析。
"""

from __future__ import annotations

from typing import Any, Sequence

__all__ = [
    "LANG_NAMES",
    "lang_name",
    "build_translate_system_prompt",
    "build_proofread_system_prompt",
    "build_translate_user_message",
    "build_proofread_user_message",
    "INCOMPLETE_REPLY_CORRECTION",
    "build_correction_note",
]

# 语言代码 → 提示词中的语言名（None 表示未知，交给模型自动检测）
LANG_NAMES: dict[str | None, str] = {
    "zh": "中文",
    "en": "英文",
    "ja": "日文",
    "auto": "自动检测",
    None: "自动检测",
}


def lang_name(lang: str | None) -> str:
    """语言代码 → 中文名；未知代码原样返回。"""
    if lang is None:
        return LANG_NAMES[None]
    return LANG_NAMES.get(str(lang).lower(), str(lang))


def _one_line(text: Any) -> str:
    """把任意文本压成单行（换行会破坏逐行协议）。"""
    return str(text or "").replace("\r", " ").replace("\n", " ").strip()


def _glossary_block(glossary: dict[str, str] | None) -> str:
    """把术语表渲染为强制替换规则；空术语表返回空串。"""
    if not glossary:
        return ""
    lines = ["", "", "术语表（强制：原文出现下列词语时，必须按指定译法翻译，并保持全篇一致）："]
    lines.extend(f"- {key} → {value}" for key, value in glossary.items())
    return "\n".join(lines)


_TRANSLATE_SYSTEM_TEMPLATE = """\
你是一名专业的影视字幕译者，精通中文、英文、日文，尤其擅长 ASMR、助眠、耳音触发、陪伴哄睡类娱乐内容的翻译，深谙此类作品轻声细语、温柔亲密的氛围。

任务：把用户消息中逐行给出的编号台词翻译成{target_name}。原文语言：{source_name}。

翻译要求：
1. 忠实原意，语气保持轻声细语、温柔放松，符合 ASMR／助眠氛围；译文要自然口语化，避免书面腔和翻译腔。
2. 日文称呼语按中文习惯意译，例如：お兄ちゃん→哥哥、お姉ちゃん→姐姐，其余称呼按亲昵程度处理。
3. 拟声词、拟态词（如 こつこつ、さらさら、ふわふわ）要转换成自然贴切的中文拟声词，不要保留假名。
4. 原文中的空行、♪、纯音乐／间奏标记等，原样保留输出，不要翻译也不要删除。
5. 严格逐行对应：不合并行、不拆分行、不遗漏行、不增加行，每句独立翻译。
6. 不输出任何解释、注释、罗马音或原文。

输出格式（必须严格遵守）：
- 每行一条译文，格式为：N|译文
- 其中 N 是该句在用户消息中的行号（从 1 开始的整数），紧跟半角竖线“|”，之后是该句译文。
- 除上述输出行以外，不要输出任何其他内容。{glossary_block}"""

_PROOFREAD_SYSTEM_TEMPLATE = """\
你是一名资深的影视字幕审校，精通中文、英文、日文，熟悉 ASMR、助眠、娱乐向内容，负责审校字幕初译。

任务：用户消息中将逐行给出编号台词的原文与初译，请逐句校对，输出审校后的{target_name}译文。

审校要求：
1. 修正误译、漏译、语法错误与明显不通顺之处；初译已经正确时保持原样，不要过度改写。
2. 统一全篇术语与称呼的一致性；如提供术语表则必须遵守。
3. 保持轻声细语、温柔放松的氛围与自然口语感，符合 ASMR／助眠场景。
4. 原文为空行、♪ 或纯音乐标记时，对应译文原样保留。
5. 行号与行数必须与输入完全一致：不合并、不拆分、不遗漏、不增加。
6. 不输出任何解释或原文。

输出格式（必须严格遵守）：
- 每行一条译文，格式为：N|译文
- 其中 N 是该句的行号（从 1 开始的整数），紧跟半角竖线“|”，之后是审校后的译文。
- 除上述输出行以外，不要输出任何其他内容。{glossary_block}"""


def build_translate_system_prompt(
    source_lang: str | None,
    target_lang: str,
    glossary: dict[str, str] | None = None,
) -> str:
    """构建翻译系统提示词。"""
    return _TRANSLATE_SYSTEM_TEMPLATE.format(
        source_name=lang_name(source_lang),
        target_name=lang_name(target_lang),
        glossary_block=_glossary_block(glossary),
    )


def build_proofread_system_prompt(
    target_lang: str,
    glossary: dict[str, str] | None = None,
) -> str:
    """构建审校系统提示词。"""
    return _PROOFREAD_SYSTEM_TEMPLATE.format(
        target_name=lang_name(target_lang),
        glossary_block=_glossary_block(glossary),
    )


def build_translate_user_message(
    items: Sequence[tuple[int, str]],
    context: Sequence[tuple[int, str]] | None = None,
    target_lang: str = "zh",
) -> str:
    """构建翻译批次用户消息。

    :param items: ``[(批内行号, 原文)]``，行号从 1 开始
    :param context: ``[(绝对句号, 已定稿译文)]``，最多前 3 句，供参考
    """
    lines: list[str] = []
    if context:
        lines.append("前文译文（仅供参考，不要输出它们）：")
        lines.extend(f"#{idx} 译文：{_one_line(text)}" for idx, text in context)
        lines.append("")
    lines.append(
        f"请把下面 {len(items)} 句台词翻译成{lang_name(target_lang)}，"
        f"输出 {len(items)} 行，每行格式严格为 N|译文（N 为行号，从 1 开始）："
    )
    lines.extend(f"{no}|{_one_line(text)}" for no, text in items)
    return "\n".join(lines)


def build_proofread_user_message(
    items: Sequence[tuple[int, tuple[str, str]]],
    context: Sequence[tuple[int, str]] | None = None,
    target_lang: str = "zh",
) -> str:
    """构建审校批次用户消息。

    :param items: ``[(批内行号, (原文, 初译))]``，行号从 1 开始
    :param context: ``[(绝对句号, 已定稿译文)]``，最多前 3 句，供参考
    """
    lines: list[str] = []
    if context:
        lines.append("前文译文（仅供参考，不要输出它们）：")
        lines.extend(f"#{idx} 译文：{_one_line(text)}" for idx, text in context)
        lines.append("")
    lines.append(
        f"下面是 {len(items)} 句字幕的原文与初译，请审校成{lang_name(target_lang)}，"
        f"输出 {len(items)} 行，每行格式严格为 N|审校后的译文（N 为行号，从 1 开始）："
    )
    for no, (source, draft) in items:
        lines.append(f"{no}|{_one_line(source)}")
        lines.append(f"初译：{_one_line(draft)}")
    return "\n".join(lines)


# 解析不完整时第二次请求附加的纠错说明（措辞来自实现约定，勿随意改动开头）
INCOMPLETE_REPLY_CORRECTION = (
    "你上次的输出行数/编号不完整，请严格按 N|译文 每行一条输出全部 N 行，"
    "不要输出任何解释或额外内容。"
)


def build_correction_note(missing_ids: Sequence[int]) -> str:
    """生成纠错说明，附带缺失行号便于模型自查。"""
    if missing_ids:
        missing = ", ".join(str(i) for i in missing_ids[:20])
        return f"{INCOMPLETE_REPLY_CORRECTION}（缺失行号：{missing}）"
    return INCOMPLETE_REPLY_CORRECTION
