"""backend.translate —— 字幕翻译 / 审校模块。

公开 API（契约见 docs/API.md「翻译模块 Python 契约」，签名不可更改）::

    from backend.translate import SubtitleTranslator, Proofreader, test_connection

协议解析器与兜底对齐工具（parse_reply / batch_complete / align_fallback /
mock_translate_text）可从 ``backend.translate.translator`` 导入。
"""

from .translator import SubtitleTranslator, Proofreader, test_connection

__all__ = ["SubtitleTranslator", "Proofreader", "test_connection"]
