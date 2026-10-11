"""NVIDIA CUDA 运行库按需下载（供单文件 exe 使用 GPU 推理）。

单文件 exe 里不含 cuBLAS/cuDNN（体积太大），首次在 NVIDIA 机器上做识别时：
  1. 从 PyPI 镜像下载 nvidia-cublas-cu12 / nvidia-cudnn-cu12 的 win_amd64 wheel
  2. 解出其中的 DLL 到 data/cuda/bin（一次性，之后永久复用）
  3. whisper_asr._setup_cuda_dlls 会把该目录注册进 DLL 搜索路径
"""
from __future__ import annotations

import logging
import zipfile
from pathlib import Path

import httpx

from .. import config

log = logging.getLogger(__name__)

_INDEX = "https://pypi.tuna.tsinghua.edu.cn/simple"
_INDEX_FALLBACK = "https://pypi.org/simple"
_PACKAGES = ("nvidia-cublas-cu12", "nvidia-cudnn-cu12")
_CHUNK = 8 * 1024 * 1024


def cuda_bin_dir() -> Path:
    return config.DATA_DIR / "cuda" / "bin"


def dlls_ready() -> bool:
    d = cuda_bin_dir()
    return (d / "cublas64_12.dll").exists() and any(d.glob("cudnn*.dll"))


def _pick_wheel(client: httpx.Client, package: str) -> tuple[str, int]:
    """从镜像索引解析该包最新的 win_amd64 wheel 直链。"""
    for base in (_INDEX, _INDEX_FALLBACK):
        try:
            r = client.get(f"{base}/{package}/", timeout=30)
            r.raise_for_status()
            import re

            links = re.findall(r'href="([^"]+)"', r.text)
            cands = [
                link for link in links
                if link.lower().endswith(".whl#") or ".whl" in link.lower()
            ]
            win = [link for link in cands if "win_amd64" in link.lower()]
            if not win:
                continue
            url = win[-1].split("#", 1)[0]
            if url.startswith("./") or not url.startswith("http"):
                from urllib.parse import urljoin

                url = urljoin(f"{base}/{package}/", url)
            size = 0
            try:
                head = client.head(url, timeout=30)
                size = int(head.headers.get("content-length") or 0)
            except Exception:
                pass
            return url, size
        except Exception as e:
            log.warning("索引 %s 获取 %s 失败: %s", base, package, e)
    raise RuntimeError(f"无法从镜像索引解析 {package} 的 wheel 下载地址")


def ensure_cuda_runtime(progress_cb=None, cancel_cb=None) -> bool:
    """确保 CUDA 运行库就绪；返回 True 表示可用（原本就有或下载完成）。"""
    if dlls_ready():
        return True
    d = cuda_bin_dir()
    d.mkdir(parents=True, exist_ok=True)
    done = {"bytes": 0}

    def report(msg: str):
        if progress_cb:
            try:
                progress_cb(msg)
            except Exception:
                pass

    with httpx.Client(follow_redirects=True) as client:
        for pkg in _PACKAGES:
            part = d / f"{pkg}.whl.part"
            if not part.exists():
                url, size = _pick_wheel(client, pkg)
                log.info("下载 %s (%.0fMB)", pkg, size / 1048576)
                report(f"下载 GPU 运行库 {pkg} {size / 1048576:.0f}MB")
                headers = {}
                if part.exists():
                    headers["Range"] = f"bytes={part.stat().st_size}-"
                with client.stream("GET", url, headers=headers, timeout=httpx.Timeout(30, read=60)) as r:
                    if r.status_code not in (200, 206):
                        raise RuntimeError(f"{pkg} 下载失败: HTTP {r.status_code}")
                    mode = "ab" if r.status_code == 206 else "wb"
                    with open(part, mode) as f:
                        for chunk in r.iter_bytes(chunk_size=_CHUNK):
                            if cancel_cb and cancel_cb():
                                raise RuntimeError("已取消")
                            f.write(chunk)
                            done["bytes"] += len(chunk)
                            if size and done["bytes"] > size:
                                size = 0  # HEAD 估计与实际不符时切换为不显示总量
                            if size:
                                report(f"下载 GPU 运行库 {pkg} {done['bytes'] / 1048576:.0f}/{size / 1048576:.0f}MB")
                            else:
                                report(f"下载 GPU 运行库 {pkg} {done['bytes'] / 1048576:.0f}MB")
            report(f"解压 GPU 运行库 {pkg}…")
            with zipfile.ZipFile(part) as z:
                for name in z.namelist():
                    low = name.lower()
                    if low.startswith("nvidia/") and low.endswith(".dll"):
                        target = d / Path(name).name
                        with z.open(name) as src, open(target, "wb") as f:
                            f.write(src.read())
            part.unlink(missing_ok=True)

    ok = dlls_ready()
    if ok:
        report("GPU 运行库就绪")
        log.info("CUDA 运行库就绪: %s", d)
    else:
        report("GPU 运行库下载不完整，将使用 CPU")
    return ok
