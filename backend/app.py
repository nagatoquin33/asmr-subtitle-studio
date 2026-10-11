"""FastAPI 主应用：HTTP 路由层（参数校验 + 委托 services），含 SSE 与媒体流传输。

业务规则在 services.py / pipeline.py / tasks.py；本文件不包含业务判断。
"""
from __future__ import annotations

import asyncio
import json
import logging
import mimetypes
import re
from pathlib import Path

from fastapi import FastAPI, File, Form, HTTPException, Request, UploadFile
from fastapi.responses import FileResponse, JSONResponse, StreamingResponse
from fastapi.staticfiles import StaticFiles
from pydantic import BaseModel

from . import config, ffmpeg_utils, queueing, services, subtitles
from .asr import modelscope_dl, whisper_asr
from .pipeline import retranslate_segment, update_segment_text
from .tasks import manager

logging.basicConfig(
    level=logging.INFO,
    format="%(asctime)s %(levelname)s %(name)s: %(message)s",
)
log = logging.getLogger(__name__)

app = FastAPI(title="ASMR 字幕工作室", version=config.APP_VERSION)


def _svc_error(e: services.ServiceError) -> HTTPException:
    return HTTPException(e.code, str(e))


@app.on_event("startup")
async def startup() -> None:
    config.ensure_dirs()
    manager.set_loop(asyncio.get_running_loop())
    queueing.task_queue.start()


def _task_or_404(task_id: str) -> dict:
    t = manager.get(task_id)
    if not t:
        raise HTTPException(404, "任务不存在")
    return t


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
    return services.public_config()


@app.post("/api/config")
async def set_cfg(request: Request):
    return services.apply_config_patch(await request.json())


# ---------- 翻译连通性测试 ----------
class TranslateTestBody(BaseModel):
    base_url: str = ""
    api_key: str = ""
    model: str = ""


@app.post("/api/translate/test")
async def translate_test(body: TranslateTestBody):
    return await services.test_translate(body.base_url, body.api_key, body.model)


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
@app.post("/api/tasks")
async def create_tasks(
    files: list[UploadFile] = File(...),
    source_lang: str = Form("auto"),
    target_lang: str = Form("zh"),
):
    try:
        tasks = await services.create_tasks_from_uploads_async(files, source_lang, target_lang)
    except services.ServiceError as e:
        raise _svc_error(e)
    return {"tasks": tasks}


class LocalPathsBody(BaseModel):
    paths: list[str]
    source_lang: str = "auto"
    target_lang: str = "zh"


@app.post("/api/tasks/local")
def create_local_tasks(body: LocalPathsBody):
    tasks, missing = services.create_tasks_from_paths(body.paths, body.source_lang, body.target_lang)
    return {"tasks": tasks, "missing": missing}


class UrlTasksBody(BaseModel):
    urls: list[str]
    source_lang: str = "auto"
    target_lang: str = "zh"


@app.post("/api/tasks/url")
def create_url_tasks(body: UrlTasksBody):
    try:
        tasks, invalid = services.create_tasks_from_urls(body.urls, body.source_lang, body.target_lang)
    except services.ServiceError as e:
        raise _svc_error(e)
    return {"tasks": tasks, "invalid": invalid}


# ---------- 任务查询/操作 ----------
@app.get("/api/tasks")
def list_tasks(limit: int = 100):
    return {"tasks": manager.list(limit=min(limit, 500))}


@app.get("/api/tasks/{task_id}")
def get_task(task_id: str, segments: int = 0):
    try:
        return services.get_task_detail(task_id, with_segments=bool(segments))
    except services.ServiceError as e:
        raise _svc_error(e)


@app.delete("/api/tasks/{task_id}")
def delete_task(task_id: str):
    try:
        services.delete_task(task_id)
    except services.ServiceError as e:
        raise _svc_error(e)
    return {"ok": True}


@app.post("/api/tasks/{task_id}/cancel")
def cancel_task(task_id: str):
    try:
        return {"ok": True, "task": services.cancel_task(task_id)}
    except services.ServiceError as e:
        raise _svc_error(e)


@app.post("/api/tasks/{task_id}/retry")
def retry_task(task_id: str):
    try:
        return {"ok": True, "task": services.retry_task(task_id)}
    except services.ServiceError as e:
        raise _svc_error(e)


class SegmentPatchBody(BaseModel):
    translated: str


@app.patch("/api/tasks/{task_id}/segments/{index}")
def patch_segment(task_id: str, index: int, body: SegmentPatchBody):
    _task_or_404(task_id)
    try:
        return {"ok": True, "segment": update_segment_text(task_id, index, body.translated)}
    except IndexError:
        raise HTTPException(404, "段索引越界")


@app.post("/api/tasks/{task_id}/segments/{index}/retranslate")
async def retranslate_segment_api(task_id: str, index: int):
    _task_or_404(task_id)
    try:
        return {"ok": True, "segment": await retranslate_segment(task_id, index)}
    except IndexError:
        raise HTTPException(404, "段索引越界")
    except Exception as e:
        raise HTTPException(502, f"重翻失败：{e}")


@app.post("/api/tasks/{task_id}/open_folder")
def open_folder(task_id: str):
    try:
        return {"ok": True, "dir": services.open_task_folder(task_id)}
    except services.ServiceError as e:
        raise _svc_error(e)


# ---------- SSE ----------
async def _sse_stream(task_id: str):
    t = _task_or_404(task_id)
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
    _task_or_404(task_id)
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
    _task_or_404(task_id)
    path = Path(manager.get(task_id)["media"]["path"] or "")
    if not str(path) or not path.exists():
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
