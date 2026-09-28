"""魔搭（ModelScope）模型下载管理 —— 纯 HTTP 直连实现（无第三方 SDK 依赖）。

- 文件列表：GET https://modelscope.cn/api/v1/models/{repo}/repo/files?Revision=master&Recursive=true
- 文件直链：GET https://modelscope.cn/models/{repo}/resolve/master/{path}
- 支持 Range 断点续传（.part 临时文件），下载进度实时上报（供 /api/models 展示百分比）。
- faster-whisper 各模型在魔搭上的镜像仓库（多个候选，主选失败自动回退）。
"""
from __future__ import annotations

import logging
import threading
import time
from pathlib import Path

import httpx

from .. import config

log = logging.getLogger(__name__)

_API_BASE = "https://modelscope.cn/api/v1/models"
_RESOLVE_BASE = "https://modelscope.cn/models"
_HEADERS = {"User-Agent": "asmr-subtitle-studio/1.0"}
_CHUNK = 8 * 1024 * 1024  # 8MB

REGISTRY: dict[str, dict] = {
    "small": {
        "repos": ["gpustack/faster-whisper-small"],
        "size_hint": "~480MB",
        "desc": "速度快，中英日准确率良好（默认）",
    },
    "medium": {
        "repos": ["gpustack/faster-whisper-medium"],
        "size_hint": "~1.5GB",
        "desc": "均衡之选，准确率更高",
    },
    "large-v3": {
        "repos": ["gpustack/faster-whisper-large-v3", "keepitsimple/faster-whisper-large-v3"],
        "size_hint": "~3GB",
        "desc": "最高准确率，推荐 GPU 使用",
    },
    "large-v3-turbo": {
        "repos": ["pengzhendong/faster-whisper-large-v3-turbo"],
        "size_hint": "~1.6GB",
        "desc": "large-v3 精简加速版，速度/精度折中最优",
    },
}

_states: dict[str, dict] = {}
_locks: dict[str, threading.Lock] = {}
_reg_lock = threading.Lock()


def _lock(name: str) -> threading.Lock:
    with _reg_lock:
        if name not in _locks:
            _locks[name] = threading.Lock()
        return _locks[name]


def _state(name: str) -> dict:
    with _reg_lock:
        if name not in _states:
            _states[name] = {
                "status": "idle", "progress": None,
                "detail": "", "error": "",
                "downloaded": 0, "total": 0,
            }
        return _states[name]


def model_dir(name: str) -> Path:
    return config.MODELS_DIR / name


def is_ready(name: str) -> bool:
    d = model_dir(name)
    return (d / "model.bin").exists() and ((d / "config.json").exists() or (d / "preprocessor_config.json").exists())


def get_status(name: str) -> dict:
    info = REGISTRY.get(name)
    st = _state(name)
    out = {
        "name": name,
        "repos": info["repos"] if info else [],
        "size_hint": info.get("size_hint", "") if info else "",
        "desc": info.get("desc", "") if info else "",
        "local_path": str(model_dir(name)) if is_ready(name) else None,
    }
    if is_ready(name):
        out.update({"status": "ready", "progress": 1.0, "detail": "", "error": ""})
    else:
        out.update({k: st.get(k) for k in ("status", "progress", "detail", "error")})
        if st.get("status") == "downloading" and st.get("total"):
            out["detail"] = f"{st['downloaded'] / 1048576:.0f}/{st['total'] / 1048576:.0f}MB"
    return out


def all_status() -> list[dict]:
    return [get_status(name) for name in REGISTRY]


def _list_files(client: httpx.Client, repo: str) -> list[dict]:
    r = client.get(f"{_API_BASE}/{repo}/repo/files", params={"Revision": "master", "Recursive": "true"})
    r.raise_for_status()
    data = r.json()
    if data.get("Code") != 200:
        raise RuntimeError(f"魔搭文件列表接口返回异常: {str(data)[:200]}")
    files = [
        {"path": f["Path"], "size": int(f.get("Size") or 0)}
        for f in data["Data"]["Files"]
        if f.get("Type") == "blob"
    ]
    if not files:
        raise RuntimeError(f"仓库 {repo} 文件列表为空")
    return files


def _download_file(client: httpx.Client, repo: str, path: str, dest: Path, done: dict, total: int, st: dict) -> None:
    """单文件下载，支持 Range 断点续传。done["bytes"] 为跨文件累计字节数。"""
    dest.parent.mkdir(parents=True, exist_ok=True)
    part = dest.with_name(dest.name + ".part")
    url = f"{_RESOLVE_BASE}/{repo}/resolve/master/{path}"

    for attempt in range(4):
        pos = part.stat().st_size if part.exists() else 0
        headers = dict(_HEADERS)
        if pos > 0:
            headers["Range"] = f"bytes={pos}-"
        try:
            with client.stream("GET", url, headers=headers, timeout=httpx.Timeout(30, read=60)) as r:
                if r.status_code == 200 and pos > 0:
                    part.unlink(missing_ok=True)  # 服务端不支持续传，重下
                    pos = 0
                if r.status_code not in (200, 206):
                    raise RuntimeError(f"HTTP {r.status_code}: {path}")
                mode = "ab" if r.status_code == 206 else "wb"
                if r.status_code == 200:
                    pos = 0
                with open(part, mode) as f:
                    for chunk in r.iter_bytes(chunk_size=_CHUNK):
                        f.write(chunk)
                        done["bytes"] += len(chunk)
                        st["downloaded"] = done["bytes"]
                        if total:
                            st["progress"] = min(1.0, done["bytes"] / total)
            part.replace(dest)
            return
        except httpx.HTTPError as e:
            if attempt == 3:
                raise RuntimeError(f"{path} 下载失败: {e}") from e
            time.sleep(2 * (attempt + 1))


def _download(name: str) -> Path:
    info = REGISTRY.get(name)
    if not info:
        raise ValueError(f"未知模型: {name}")
    st = _state(name)
    st.update({"status": "downloading", "progress": 0.0, "detail": "正在连接魔搭…", "error": ""})

    last_err: Exception | None = None
    for repo in info["repos"]:
        try:
            log.info("开始从魔搭下载模型 %s (repo=%s)", name, repo)
            st["detail"] = f"仓库 {repo}"
            with httpx.Client(headers=_HEADERS, follow_redirects=True) as client:
                files = _list_files(client, repo)
                total = sum(f["size"] for f in files)
                st["total"] = total
                st["detail"] = f"{total / 1048576:.0f}MB · {repo}"
                done = {"bytes": 0}
                for f in files:
                    _download_file(client, repo, f["path"], model_dir(name) / f["path"], done, total, st)
            if not is_ready(name):
                raise RuntimeError(f"仓库 {repo} 缺少 faster-whisper 模型文件 (model.bin)")
            st.update({"status": "ready", "progress": 1.0, "detail": "", "error": ""})
            log.info("模型 %s 下载完成: %s (%.0fMB)", name, model_dir(name), total / 1048576)
            return model_dir(name)
        except Exception as e:  # 尝试下一个候选仓库
            last_err = e
            log.warning("仓库 %s 下载失败: %s", repo, e)
    st.update({"status": "error", "progress": None, "error": str(last_err)[:300]})
    raise RuntimeError(f"模型 {name} 所有候选仓库下载失败: {last_err}")


def ensure_ready(name: str) -> Path:
    """阻塞直到模型就绪（线程安全；同一模型并发调用会串行等待同一次下载）。"""
    with _lock(name):
        if is_ready(name):
            _state(name).update({"status": "ready", "progress": 1.0})
            return model_dir(name)
        return _download(name)


def start_download(name: str) -> dict:
    """异步启动下载（供 API 调用），返回即时状态。"""
    if name not in REGISTRY:
        raise ValueError(f"未知模型: {name}")
    if is_ready(name):
        return get_status(name)

    def _run():
        try:
            ensure_ready(name)
        except Exception as e:
            log.error("模型 %s 下载失败: %s", name, e)

    threading.Thread(target=_run, name=f"dl-{name}", daemon=True).start()
    return get_status(name)
