"""任务管理器：注册表、持久化、SSE 订阅、工作队列。"""
from __future__ import annotations

import asyncio
import copy
import json
import logging
import re
import shutil
import threading
import time
import uuid
from pathlib import Path

from . import config

log = logging.getLogger(__name__)

RUNNING_STAGES = {"queued", "downloading", "extracting", "asr", "translating", "proofreading", "writing"}
FINAL_STAGES = {"done", "error", "canceled"}


def sanitize_filename(name: str) -> str:
    name = Path(name).name
    name = re.sub(r'[\\/:*?"<>|\r\n]', "_", name).strip()
    return name or "media"


def new_task_id() -> str:
    return f"t_{time.strftime('%Y%m%d_%H%M%S')}_{uuid.uuid4().hex[:6]}"


class TaskManager:
    def __init__(self) -> None:
        self.tasks: dict[str, dict] = {}
        self.subs: dict[str, set] = {}
        self.loop: asyncio.AbstractEventLoop | None = None
        self._lock = threading.Lock()
        self._load_existing()

    # ---------- 持久化 ----------
    def _load_existing(self) -> None:
        config.ensure_dirs()
        for d in sorted(config.TASKS_DIR.glob("t_*")):
            meta = d / "task.json"
            if not meta.exists():
                continue
            try:
                task = json.loads(meta.read_text(encoding="utf-8"))
                if task.get("status") in RUNNING_STAGES:
                    task["status"] = "error"
                    task["message"] = "服务重启导致中断，可点击重试"
                task.setdefault("dir", str(config.TASKS_DIR / task["id"]))
                self.tasks[task["id"]] = task
            except Exception as e:
                log.warning("读取任务 %s 失败: %s", d.name, e)

    def persist(self, task_id: str) -> None:
        task = self.tasks.get(task_id)
        if not task:
            return
        d = config.TASKS_DIR / task_id
        d.mkdir(parents=True, exist_ok=True)
        tmp = d / "task.json.tmp"
        tmp.write_text(json.dumps(task, ensure_ascii=False, indent=1), encoding="utf-8")
        tmp.replace(d / "task.json")

    # ---------- 创建/查询 ----------
    def create(self, src: Path, options: dict, copy_into_task: bool) -> dict:
        tid = new_task_id()
        d = config.TASKS_DIR / tid
        d.mkdir(parents=True, exist_ok=True)
        if copy_into_task:
            dst = d / sanitize_filename(src.name)
            shutil.copy2(src, dst)
            media_path = dst
        else:
            media_path = src.resolve()
        now = time.strftime("%Y-%m-%dT%H:%M:%S")
        task = {
            "id": tid,
            "name": media_path.name,
            "status": "queued",
            "stage": "queued",
            "progress": 0.0,
            "message": "排队中",
            "error": None,
            "cancel": False,
            "created_at": now,
            "updated_at": now,
            "dir": str(d),
            "media": {"path": str(media_path), "duration": None, "size": None, "has_video": None},
            "asr": {},
            "options": options,
            "counts": {"segments": 0, "translated": 0},
            "has_result": False,
            "outputs": {},
        }
        with self._lock:
            self.tasks[tid] = task
        self.persist(tid)
        self._publish(tid, task)
        return copy.deepcopy(task)

    def create_url_task(self, url: str, options: dict) -> dict:
        """URL 下载任务：媒体路径在下载完成后回填。"""
        tid = new_task_id()
        (config.TASKS_DIR / tid).mkdir(parents=True, exist_ok=True)
        now = time.strftime("%Y-%m-%dT%H:%M:%S")
        task = {
            "id": tid,
            "name": url if len(url) <= 72 else url[:69] + "…",
            "status": "queued",
            "stage": "queued",
            "progress": 0.0,
            "message": "排队中",
            "error": None,
            "cancel": False,
            "created_at": now,
            "updated_at": now,
            "dir": str(config.TASKS_DIR / tid),
            "media": {"path": None, "duration": None, "size": None, "has_video": None},
            "source_url": url,
            "asr": {},
            "options": options,
            "counts": {"segments": 0, "translated": 0},
            "has_result": False,
            "outputs": {},
        }
        with self._lock:
            self.tasks[tid] = task
        self.persist(tid)
        self._publish(tid, task)
        return copy.deepcopy(task)

    def get(self, task_id: str) -> dict | None:
        t = self.tasks.get(task_id)
        return copy.deepcopy(t) if t else None

    def get_mutable(self, task_id: str) -> dict | None:
        return self.tasks.get(task_id)

    def list(self, limit: int = 100) -> list[dict]:
        out = sorted(self.tasks.values(), key=lambda t: t["created_at"], reverse=True)[:limit]
        return [copy.deepcopy(t) for t in out]

    # ---------- 状态更新（可从任意线程调用） ----------
    def update(self, task_id: str, event: bool = True, **fields) -> dict | None:
        task = self.tasks.get(task_id)
        if not task:
            return None
        task.update(fields)
        task["updated_at"] = time.strftime("%Y-%m-%dT%H:%M:%S")
        self.persist(task_id)
        if event:
            self._publish(task_id, task)
        return copy.deepcopy(task)

    def is_cancelled(self, task_id: str) -> bool:
        t = self.tasks.get(task_id)
        return bool(t and t.get("cancel"))

    def cancel(self, task_id: str) -> dict | None:
        t = self.tasks.get(task_id)
        if not t:
            return None
        if t["status"] == "queued":
            return self.update(task_id, status="canceled", stage="canceled", message="已取消")
        t["cancel"] = True
        self.persist(task_id)
        return copy.deepcopy(t)

    def delete(self, task_id: str) -> bool:
        t = self.tasks.get(task_id)
        if not t:
            return False
        if t["status"] in RUNNING_STAGES and t["status"] != "queued":
            raise RuntimeError("任务运行中，请先取消")
        if t["status"] == "queued":
            t["status"] = "canceled"  # 阻止 worker 领取
            t["cancel"] = True
        with self._lock:
            self.tasks.pop(task_id, None)
            self.subs.pop(task_id, None)
        shutil.rmtree(config.TASKS_DIR / task_id, ignore_errors=True)
        # 该任务的产物目录（output/{视频名}/）一并删除
        if t.get("output_dir"):
            out = Path(t["output_dir"])
            if out.resolve().parent == config.OUTPUT_DIR.resolve():
                shutil.rmtree(out, ignore_errors=True)
        return True

    # ---------- SSE ----------
    def set_loop(self, loop: asyncio.AbstractEventLoop) -> None:
        self.loop = loop

    def _publish(self, task_id: str, task: dict) -> None:
        subs = self.subs.get(task_id)
        if not subs:
            return
        event = {"type": "progress", "task": copy.deepcopy(task)}
        try:
            running = asyncio.get_running_loop()
        except RuntimeError:
            running = None
        if running and running is self.loop:
            for q in list(subs):
                q.put_nowait(event)
        elif self.loop:
            for q in list(subs):
                self.loop.call_soon_threadsafe(q.put_nowait, event)

    async def subscribe(self, task_id: str):
        q: asyncio.Queue = asyncio.Queue(maxsize=64)
        self.subs.setdefault(task_id, set()).add(q)
        try:
            yield q
        finally:
            self.subs.get(task_id, set()).discard(q)
            if not self.subs.get(task_id):
                self.subs.pop(task_id, None)

    def emit_final(self, task_id: str) -> None:
        task = self.tasks.get(task_id)
        if task:
            etype = {"done": "done", "error": "error", "canceled": "canceled"}.get(task["status"], "progress")
            event = {"type": etype, "task": copy.deepcopy(task)}
            subs = self.subs.get(task_id) or set()
            if self.loop:
                for q in list(subs):
                    self.loop.call_soon_threadsafe(q.put_nowait, event)


manager = TaskManager()
