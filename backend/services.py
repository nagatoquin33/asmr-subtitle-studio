"""业务服务层：配置（含 .env 密钥）、任务创建与生命周期。

HTTP 路由（app.py）只做参数校验与委托；业务规则集中在这里，便于测试与复用。
"""
from __future__ import annotations

import copy
import json
import logging
import re
import subprocess
import sys
from pathlib import Path

from . import config, queueing
from .tasks import RUNNING_STAGES, manager, sanitize_filename

log = logging.getLogger(__name__)

_VALID_LANGS = ("auto", "zh", "en", "ja")


class ServiceError(Exception):
    """业务规则错误；code 映射 HTTP 状态码。"""

    def __init__(self, message: str, code: int = 400):
        super().__init__(message)
        self.code = code


# ---------- 配置 ----------
def public_config() -> dict:
    return config.public_config(config.get_config())


def apply_config_patch(patch: dict) -> dict:
    patch = copy.deepcopy(patch or {})
    tr = patch.get("translate")
    if isinstance(tr, dict) and "api_key" in tr:
        # key 只存 .env：新值写入，***/空 表示保留现有 key
        key = tr.pop("api_key")
        if key and key != "***":
            config.write_env_key(key)
    return config.public_config(config.update_config(patch))


async def test_translate(base_url: str, api_key: str, model: str) -> dict:
    cfg = copy.deepcopy(config.get_config().get("translate", {}))
    if base_url:
        cfg["base_url"] = base_url
    if model:
        cfg["model"] = model
    if api_key and api_key != "***":
        cfg["api_key"] = api_key
    try:
        from .translate import test_connection

        return await test_connection(cfg)
    except Exception as e:
        return {"ok": False, "message": str(e)[:300], "sample": ""}


# ---------- 任务创建 ----------
def _validated_langs(source_lang: str, target_lang: str) -> tuple[str, str]:
    if source_lang not in _VALID_LANGS:
        source_lang = "auto"
    if target_lang not in _VALID_LANGS:
        target_lang = "zh"
    return source_lang, target_lang


def _register_and_enqueue(task: dict) -> None:
    queueing.task_queue.put(task["id"])


async def create_tasks_from_uploads_async(files: list, source_lang: str, target_lang: str) -> list[dict]:
    """files 为 FastAPI UploadFile 列表；流式写入任务目录。"""
    source_lang, target_lang = _validated_langs(source_lang, target_lang)
    tasks = []
    for f in files:
        if not f.filename:
            continue
        t = manager.create(Path(f.filename), {"source_lang": source_lang, "target_lang": target_lang},
                           copy_into_task=False)
        dst = config.TASKS_DIR / t["id"] / sanitize_filename(f.filename)
        size = 0
        with open(dst, "wb") as out:
            while chunk := await f.read(1024 * 1024):
                out.write(chunk)
                size += len(chunk)
        if size == 0:
            manager.delete(t["id"])
            continue
        t = manager.get_mutable(t["id"])
        t["media"]["path"] = str(dst)
        t["name"] = dst.name
        manager.persist(t["id"])
        _register_and_enqueue(manager.get(t["id"]))
        tasks.append(manager.get(t["id"]))
    if not tasks:
        raise ServiceError("没有有效的上传文件")
    return tasks


def create_tasks_from_paths(paths: list[str], source_lang: str, target_lang: str) -> tuple[list[dict], list[str]]:
    source_lang, target_lang = _validated_langs(source_lang, target_lang)
    tasks, missing = [], []
    for p in paths:
        p = (p or "").strip().strip('"')
        if not p:
            continue
        src = Path(p)
        if not src.exists() or not src.is_file():
            missing.append(p)
            continue
        t = manager.create(src, {"source_lang": source_lang, "target_lang": target_lang},
                           copy_into_task=False)
        _register_and_enqueue(t)
        tasks.append(t)
    return tasks, missing


def create_tasks_from_urls(urls: list[str], source_lang: str, target_lang: str) -> tuple[list[dict], list[str]]:
    source_lang, target_lang = _validated_langs(source_lang, target_lang)
    tasks, invalid = [], []
    seen: set[str] = set()
    for u in urls:
        u = (u or "").strip()
        if not u or u in seen:
            continue
        seen.add(u)
        if not re.match(r"^https?://", u, re.IGNORECASE):
            invalid.append(u)
            continue
        t = manager.create_url_task(u, {"source_lang": source_lang, "target_lang": target_lang})
        _register_and_enqueue(t)
        tasks.append(t)
    if not tasks and invalid:
        raise ServiceError(f"{len(invalid)} 个链接无效（需要 http/https 开头）")
    return tasks, invalid


# ---------- 任务生命周期 ----------
def get_task_detail(task_id: str, with_segments: bool) -> dict:
    t = manager.get(task_id)
    if not t:
        raise ServiceError("任务不存在", 404)
    if with_segments:
        seg_file = config.TASKS_DIR / task_id / "segments.json"
        if seg_file.exists():
            t["segments"] = json.loads(seg_file.read_text(encoding="utf-8"))
    return t


def delete_task(task_id: str) -> None:
    if not manager.get(task_id):
        raise ServiceError("任务不存在", 404)
    try:
        manager.delete(task_id)
    except RuntimeError as e:
        raise ServiceError(str(e), 409)


def cancel_task(task_id: str) -> dict:
    t = manager.cancel(task_id)
    if t is None:
        raise ServiceError("任务不存在", 404)
    return t


def retry_task(task_id: str) -> dict:
    t = manager.get_mutable(task_id)
    if not t:
        raise ServiceError("任务不存在", 404)
    if t["status"] in RUNNING_STAGES:
        raise ServiceError("任务尚在运行", 409)
    manager.update(task_id, status="queued", stage="queued", progress=0.0,
                   message="排队中", error=None, cancel=False)
    _register_and_enqueue(manager.get(task_id))
    return manager.get(task_id)


def open_task_folder(task_id: str) -> str:
    t = manager.get(task_id)
    if not t:
        raise ServiceError("任务不存在", 404)
    d = config.TASKS_DIR / task_id
    if t.get("output_dir") and Path(t["output_dir"]).exists():
        d = Path(t["output_dir"])
    d.mkdir(parents=True, exist_ok=True)
    try:
        if sys.platform == "win32":
            import os

            os.startfile(str(d))  # type: ignore[attr-defined]
        elif sys.platform == "darwin":
            subprocess.Popen(["open", str(d)])
        else:
            subprocess.Popen(["xdg-open", str(d)])
    except Exception as e:
        raise ServiceError(f"无法打开文件夹：{e}", 500)
    return str(d)
