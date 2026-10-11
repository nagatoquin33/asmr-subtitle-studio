"""任务工作队列：worker 池与排队恢复，与 HTTP 层解耦。"""
from __future__ import annotations

import asyncio
import logging

from . import config
from .tasks import manager

log = logging.getLogger(__name__)


class TaskQueue:
    def __init__(self) -> None:
        self._queue: asyncio.Queue | None = None
        self._workers: list[asyncio.Task] = []

    @property
    def started(self) -> bool:
        return self._queue is not None

    def start(self) -> None:
        """在运行中的事件循环里启动 worker 池并恢复上次中断的排队任务。"""
        if self._queue is not None:
            return
        self._queue = asyncio.Queue()
        workers = max(1, int(config.get_config().get("pipeline", {}).get("workers", 1)))
        for i in range(workers):
            t = asyncio.create_task(self._worker(i), name=f"task-worker-{i}")
            self._workers.append(t)
        for t in manager.list(limit=1000):
            if t["status"] == "queued":
                self.put(t["id"])

    def put(self, task_id: str) -> None:
        if self._queue is None:
            log.error("TaskQueue 未启动，任务 %s 无法入队", task_id)
            return
        self._queue.put_nowait(task_id)

    async def _worker(self, idx: int) -> None:
        from .pipeline import run_task

        while True:
            task_id = await self._queue.get()
            try:
                await run_task(task_id)
            except Exception:
                log.exception("worker-%d 处理任务 %s 异常", idx, task_id)
            finally:
                self._queue.task_done()


task_queue = TaskQueue()
