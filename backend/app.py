"""FastAPI 主应用：REST API + SSE + 静态托管。"""
from __future__ import annotations

import asyncio
import copy
import json
import logging
import mimetypes
import re
import subprocess
from pathlib import Path

from fastapi import FastAPI, File, Form, HTTPException, Request, UploadFile
from fastapi.responses import FileResponse, JSONResponse, StreamingResponse
from fastapi.staticfiles import StaticFiles
from pydantic import BaseModel

from . import config, ffmpeg_utils, pipeline, subtitles
from .asr import modelscope_dl, whisper_asr
from .tasks import RUNNING_STAGES, manager, sanitize_filename

logging.basicConfig(
    level=logging.INFO,
    format="%(asctime)s %(levelname)s %(name)s: %(message)s",
)
log = logging.getLogger(__name__)

app = FastAPI(title="ASMR 字幕工作室", version=config.APP_VERSION)

_queue: asyncio.Queue | None = None


@app.on_event("startup")
async def startup() -> None:
    global _queue
    config.ensure_dirs()
    manager.set_loop(asyncio.get_running_loop())
    _queue = asyncio.Queue()

    async def worker():
        while True:
            tid = await _queue.get()
            try:
                await pipeline.run_task(tid)
            except Exception:
                log.exception("worker 处理任务 %s 异常", tid)
            finally:
                _queue.task_done()

    workers = max(1, int(config.get_config().get("pipeline", {}).get("workers", 1)))
    for _ in range(workers):
        asyncio.create_task(worker())

    # 重启后恢复排队的任务
    for t in manager.list(limit=1000):
        if t["status"] == "queued":
            _queue.put_nowait(t["id"])


def _get_or_404(task_id: str) -> dict:
    t = manager.get(task_id)
    if not t:
        raise HTTPException(404, "任务不存在")
    return t


def _enqueue(task_id: str) -> None:
    if _queue is not None:
        _queue.put_nowait(task_id)


# ---------- 健康 ----------
@app.get("/api/health")
def health():
    ok, gpu_name = whisper_asr.gpu_available()
    return {
        "ok": True,
        "app": "asmr-subtitle-studio",  # 供启动器识别端口上是否为本程序实例
        "version": config.APP_VERSION,
        "ffmpeg": bool(ffmpeg_utils.ffmpeg_path()),
        "ffmpeg_version": ffmpeg_utils.ffmpeg_version(),
        "ffmpeg_path": ffmpeg_utils.ffmpeg_path(),
        "gpu": {"available": ok, "name": gpu_name},
    }


# ---------- 配置 ----------
@app.get("/api/config")
def get_cfg():
    return config.public_config(config.get_config())


@app.post("/api/config")
async def set_cfg(request: Request):
    patch = await request.json()
    patch = copy.deepcopy(patch or {})
    tr = patch.get("translate")
    if isinstance(tr, dict) and "api_key" in tr:
        # key 只存 .env：新值写入，***/空 表示保留现有 key
        key = tr.pop("api_key")
        if key and key != "***":
            config.write_env_key(key)
    cfg = config.update_config(patch)
    return config.public_config(cfg)


# ---------- 翻译连通性测试 ----------
class TranslateTestBody(BaseModel):
    base_url: str = ""
    api_key: str = ""
    model: str = ""


@app.post("/api/translate/test")
async def translate_test(body: TranslateTestBody):
    cfg = copy.deepcopy(config.get_config().get("translate", {}))
    if body.base_url:
        cfg["base_url"] = body.base_url
    if body.model:
        cfg["model"] = body.model
    if body.api_key and body.api_key != "***":
        cfg["api_key"] = body.api_key
    try:
        from .translate import test_connection

        return await test_connection(cfg)
    except Exception as e:
        return {"ok": False, "message": str(e)[:300], "sample": ""}


# ---------- 模型管理 ----------
@app.get("/api/models")
def models():
    return {"models": modelscope_dl.all_status()}


class ModelDownloadBody(BaseModel):
    name: str


@app.post("/api/models/download")
def download_model(body: ModelDownloadBody):
    try:
        st = modelscope_dl.start_download(body.name)
    except ValueError as e:
        raise HTTPException(400, str(e))
    return {"ok": True, "model": st}


# ---------- 任务创建 ----------
def _validate_langs(source_lang: str, target_lang: str) -> tuple[str, str]:
    if source_lang not in ("auto", "zh", "en", "ja"):
        source_lang = "auto"
    if target_lang not in ("zh", "en", "ja"):
        target_lang = "zh"
    return source_lang, target_lang


@app.post("/api/tasks")
async def create_tasks(
    files: list[UploadFile] = File(...),
    source_lang: str = Form("auto"),
    target_lang: str = Form("zh"),
):
    source_lang, target_lang = _validate_langs(source_lang, target_lang)
    tasks = []
    for f in files:
        if not f.filename:
            continue
        t = manager.create(Path(f.filename), {"source_lang": source_lang, "target_lang": target_lang},
                           copy_into_task=False)
        # 先建任务再流式写入文件（文件较大时避免占内存）
        d = config.TASKS_DIR / t["id"]
        dst = d / sanitize_filename(f.filename)
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
        _enqueue(t["id"])
        tasks.append(manager.get(t["id"]))
    if not tasks:
        raise HTTPException(400, "没有有效的上传文件")
    return {"tasks": tasks}


class LocalPathsBody(BaseModel):
    paths: list[str]
    source_lang: str = "auto"
    target_lang: str = "zh"


@app.post("/api/tasks/local")
def create_local_tasks(body: LocalPathsBody):
    source_lang, target_lang = _validate_langs(body.source_lang, body.target_lang)
    tasks, missing = [], []
    for p in body.paths:
        p = p.strip().strip('"')
        if not p:
            continue
        src = Path(p)
        if not src.exists() or not src.is_file():
            missing.append(p)
            continue
        t = manager.create(src, {"source_lang": source_lang, "target_lang": target_lang},
                           copy_into_task=False)
        _enqueue(t["id"])
        tasks.append(t)
    return {"tasks": tasks, "missing": missing}


class UrlTasksBody(BaseModel):
    urls: list[str]
    source_lang: str = "auto"
    target_lang: str = "zh"


@app.post("/api/tasks/url")
def create_url_tasks(body: UrlTasksBody):
    source_lang, target_lang = _validate_langs(body.source_lang, body.target_lang)
    tasks, invalid = [], []
    seen = set()
    for u in body.urls:
        u = (u or "").strip()
        if not u or u in seen:
            continue
        seen.add(u)
        if not re.match(r"^https?://", u, re.IGNORECASE):
            invalid.append(u)
            continue
        t = manager.create_url_task(u, {"source_lang": source_lang, "target_lang": target_lang})
        _enqueue(t["id"])
        tasks.append(t)
    if not tasks and invalid:
        raise HTTPException(400, f"{len(invalid)} 个链接无效（需要 http/https 开头）")
    return {"tasks": tasks, "invalid": invalid}


# ---------- 任务查询/操作 ----------
@app.get("/api/tasks")
def list_tasks(limit: int = 100):
    return {"tasks": manager.list(limit=min(limit, 500))}


@app.get("/api/tasks/{task_id}")
def get_task(task_id: str, segments: int = 0):
    t = _get_or_404(task_id)
    if segments:
        seg_file = config.TASKS_DIR / task_id / "segments.json"
        if seg_file.exists():
            t["segments"] = json.loads(seg_file.read_text(encoding="utf-8"))
    return t


@app.delete("/api/tasks/{task_id}")
def delete_task(task_id: str):
    _get_or_404(task_id)
    try:
        manager.delete(task_id)
    except RuntimeError as e:
        raise HTTPException(409, str(e))
    return {"ok": True}


@app.post("/api/tasks/{task_id}/cancel")
def cancel_task(task_id: str):
    t = manager.cancel(task_id)
    if t is None:
        raise HTTPException(404, "任务不存在")
    return {"ok": True, "task": t}


@app.post("/api/tasks/{task_id}/retry")
def retry_task(task_id: str):
    _get_or_404(task_id)
    t = manager.get_mutable(task_id)
    if t["status"] in RUNNING_STAGES:
        raise HTTPException(409, "任务尚在运行")
    manager.update(task_id, status="queued", stage="queued", progress=0.0,
                   message="排队中", error=None, cancel=False)
    _enqueue(task_id)
    return {"ok": True, "task": manager.get(task_id)}


@app.post("/api/tasks/{task_id}/open_folder")
def open_folder(task_id: str):
    """在系统文件管理器中打开任务目录（优先产物目录 output/{视频名}/）。"""
    _get_or_404(task_id)
    t = manager.get(task_id)
    d = config.TASKS_DIR / task_id
    if t.get("output_dir") and Path(t["output_dir"]).exists():
        d = Path(t["output_dir"])
    d.mkdir(parents=True, exist_ok=True)
    import os
    import sys

    try:
        if sys.platform == "win32":
            os.startfile(str(d))  # type: ignore[attr-defined]
        elif sys.platform == "darwin":
            subprocess.Popen(["open", str(d)])
        else:
            subprocess.Popen(["xdg-open", str(d)])
    except Exception as e:
        raise HTTPException(500, f"无法打开文件夹：{e}")
    return {"ok": True, "dir": str(d)}


class SegmentPatchBody(BaseModel):
    translated: str


@app.patch("/api/tasks/{task_id}/segments/{index}")
def patch_segment(task_id: str, index: int, body: SegmentPatchBody):
    _get_or_404(task_id)
    try:
        seg = pipeline.update_segment_text(task_id, index, body.translated)
    except IndexError:
        raise HTTPException(404, "段索引越界")
    return {"ok": True, "segment": seg}


@app.post("/api/tasks/{task_id}/segments/{index}/retranslate")
async def retranslate_segment(task_id: str, index: int):
    _get_or_404(task_id)
    try:
        seg = await pipeline.retranslate_segment(task_id, index)
    except IndexError:
        raise HTTPException(404, "段索引越界")
    except Exception as e:
        raise HTTPException(502, f"重翻失败：{e}")
    return {"ok": True, "segment": seg}


# ---------- SSE ----------
async def _sse_stream(task_id: str):
    t = _get_or_404(task_id)
    yield f"data: {json.dumps({'type': 'progress', 'task': t}, ensure_ascii=False)}\n\n"
    try:
        async for q in manager.subscribe(task_id):
            while True:
                try:
                    evt = await asyncio.wait_for(q.get(), timeout=15.0)
                    yield f"data: {json.dumps(evt, ensure_ascii=False)}\n\n"
                    if evt.get("type") in ("done", "error", "canceled"):
                        break
                except asyncio.TimeoutError:
                    yield 'data: {"type": "ping"}\n\n'
    except asyncio.CancelledError:
        raise


@app.get("/api/tasks/{task_id}/events")
async def task_events(task_id: str):
    return StreamingResponse(
        _sse_stream(task_id),
        media_type="text/event-stream",
        headers={"Cache-Control": "no-cache", "X-Accel-Buffering": "no", "Connection": "keep-alive"},
    )


# ---------- 下载 ----------
@app.get("/api/tasks/{task_id}/download")
def download(task_id: str, type: str = "bilingual_srt"):
    _get_or_404(task_id)
    fname = subtitles.OUTPUT_FILES.get(type)
    if not fname:
        raise HTTPException(400, f"未知类型: {type}")
    # 产物在 output/{视频名}/（绝对路径）；兼容旧任务的 data/tasks/{id}/output/
    outputs = manager.get(task_id).get("outputs") or {}
    path = Path(outputs[type]) if outputs.get(type) else config.TASKS_DIR / task_id / "output" / fname
    if not path.exists():
        path = config.TASKS_DIR / task_id / "output" / fname
    if not path.exists():
        raise HTTPException(404, "产物尚未生成")
    mt = "application/json" if type == "json" else "text/plain; charset=utf-8"
    return FileResponse(path, media_type=mt, filename=f"{manager.get(task_id)['name']}.{fname}")


# ---------- 媒体流（支持 Range，供 <video> 拖动） ----------
@app.get("/api/media/{task_id}")
def media(task_id: str, request: Request):
    t = _get_or_404(task_id)
    path = Path(t["media"]["path"])
    if not path.exists():
        raise HTTPException(404, "媒体文件不存在")
    size = path.stat().st_size
    ct = mimetypes.guess_type(path.name)[0] or "application/octet-stream"
    range_header = request.headers.get("range")
    if range_header:
        m = re.search(r"bytes=(\d*)-(\d*)", range_header)
        start = int(m.group(1)) if m and m.group(1) else 0
        end = int(m.group(2)) if m and m.group(2) else size - 1
        end = min(end, size - 1)
        if start > end or start >= size:
            return JSONResponse(status_code=416, content={"detail": "Range Not Satisfiable"})

        def chunk_iter():
            with open(path, "rb") as f:
                f.seek(start)
                remaining = end - start + 1
                while remaining > 0:
                    buf = f.read(min(1024 * 1024, remaining))
                    if not buf:
                        break
                    remaining -= len(buf)
                    yield buf

        return StreamingResponse(
            chunk_iter(), status_code=206, media_type=ct,
            headers={
                "Content-Range": f"bytes {start}-{end}/{size}",
                "Accept-Ranges": "bytes",
                "Content-Length": str(end - start + 1),
            },
        )
    return FileResponse(path, media_type=ct)


# ---------- 静态托管（放在 API 路由之后） ----------
if config.FRONTEND_DIR.exists():
    app.mount("/", StaticFiles(directory=str(config.FRONTEND_DIR), html=True), name="static")
else:
    app.mount("/", StaticFiles(directory=str(config.PROJECT_ROOT), html=True), name="static")
