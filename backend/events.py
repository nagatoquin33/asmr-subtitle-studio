"""进程内事件总线：任务状态变化的唯一发布通道，SSE 等传输层从这里订阅。

与 TaskManager 解耦：管理器只管状态与持久化，广播统一走总线；
同一事件的订阅者各自持有 asyncio.Queue，跨线程发布安全。
"""
from __future__ import annotations

import asyncio
import copy
import threading


class EventBus:
    def __init__(self) -> None:
        self._subs: dict[str, set] = {}
        self._reg_lock = threading.Lock()
        self.loop: asyncio.AbstractEventLoop | None = None

    def set_loop(self, loop: asyncio.AbstractEventLoop) -> None:
        self.loop = loop

    def subscribe(self, topic: str) -> asyncio.Queue:
        q: asyncio.Queue = asyncio.Queue(maxsize=64)
        with self._reg_lock:
            self._subs.setdefault(topic, set()).add(q)
        return q

    def unsubscribe(self, topic: str, q: asyncio.Queue) -> None:
        with self._reg_lock:
            subs = self._subs.get(topic)
            if subs:
                subs.discard(q)
                if not subs:
                    self._subs.pop(topic, None)

    def subscriber_count(self, topic: str) -> int:
        with self._reg_lock:
            return len(self._subs.get(topic, ()))

    def publish(self, topic: str, event: dict) -> None:
        """向 topic 的所有订阅者投递事件（深拷贝隔离，线程安全）。"""
        with self._reg_lock:
            subs = list(self._subs.get(topic, ()))
        if not subs:
            return
        payload = copy.deepcopy(event)
        try:
            running = asyncio.get_running_loop()
        except RuntimeError:
            running = None
        if running and running is self.loop:
            for q in subs:
                q.put_nowait(payload)
        elif self.loop:
            for q in subs:
                self.loop.call_soon_threadsafe(q.put_nowait, payload)
