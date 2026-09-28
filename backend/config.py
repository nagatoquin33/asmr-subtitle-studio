"""全局配置：默认值、加载/保存、路径常量、.env 密钥管理。"""
from __future__ import annotations

import copy
import json
import sys
import threading
from pathlib import Path

# 打包成 exe 后：data/output/.env 跟随 exe 所在位置，只读资源（前端）在解包目录里
if getattr(sys, "frozen", False):
    PROJECT_ROOT = Path(sys.executable).resolve().parent
    RESOURCE_DIR = Path(getattr(sys, "_MEIPASS", PROJECT_ROOT))
else:
    PROJECT_ROOT = Path(__file__).resolve().parent.parent
    RESOURCE_DIR = PROJECT_ROOT

BACKEND_DIR = PROJECT_ROOT / "backend"
FRONTEND_DIR = RESOURCE_DIR / "frontend"
DATA_DIR = PROJECT_ROOT / "data"
TASKS_DIR = DATA_DIR / "tasks"
MODELS_DIR = DATA_DIR / "models"
OUTPUT_DIR = PROJECT_ROOT / "output"      # 字幕产物：output/{视频名}/
CONFIG_PATH = DATA_DIR / "config.json"
ENV_PATH = PROJECT_ROOT / ".env"          # LLM Key 存这里，不进 config.json、不进 git

APP_VERSION = "1.0.0"

DEFAULT_CONFIG: dict = {
    "asr": {
        "backend": "faster-whisper",
        "model": "small",
        "device": "auto",       # auto / cpu / cuda
        "compute_type": "auto",  # auto / int8 / float16
        "vad": "auto",          # auto / on / off：auto 下 VAD 过滤光整段音频时自动关闭重试
    },
    "translate": {
        "base_url": "https://api.deepseek.com/v1",
        "model": "deepseek-chat",
        "temperature": 0.2,
        "concurrency": 3,
        "batch_size": 40,
        "proofread": True,
        "mock": False,
        "max_retries": 3,
        "glossary": {
            "囁き": "轻声耳语",
            "耳かき": "掏耳朵",
            "タッピング": "敲击音",
            "トリガー": "触发音",
        },
    },
    "output": {
        "bilingual": True,
        "bilingual_order": "trans_first",  # trans_first / orig_first
        "max_line_chars": 42,
    },
    "download": {
        "height": 720,              # URL 下载画质上限
        "proxy": "",                # 代理，如 http://127.0.0.1:7890
        "cookies_from_browser": "",  # chrome / edge / firefox（可选）
    },
    "pipeline": {
        "workers": 1,
        "keep_audio": False,
    },
}

_ENV_HEADER = "# ASMR 字幕工作室 环境变量（此文件已被 .gitignore 忽略，不会提交）"

_lock = threading.Lock()
_cache: dict | None = None


def ensure_dirs() -> None:
    for p in (DATA_DIR, TASKS_DIR, MODELS_DIR, OUTPUT_DIR):
        p.mkdir(parents=True, exist_ok=True)


# ---------- .env 读写 ----------
def _read_env_file(path: Path) -> dict:
    env: dict = {}
    if not path.exists():
        return env
    for line in path.read_text(encoding="utf-8-sig").splitlines():
        line = line.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        k, _, v = line.partition("=")
        env[k.strip()] = v.strip().strip('"').strip("'")
    return env


def _write_env_file(path: Path, values: dict) -> None:
    lines = [_ENV_HEADER, ""]
    for k, v in values.items():
        lines.append(f"{k}={v}")
    path.write_text("\n".join(lines) + "\n", encoding="utf-8")


def write_env_key(key: str) -> None:
    """保存/覆盖 LLM_API_KEY 到 .env。传空串即清除。"""
    env = _read_env_file(ENV_PATH)
    env["LLM_API_KEY"] = (key or "").strip()
    _write_env_file(ENV_PATH, env)


def _migrate_and_load_key(cfg: dict) -> None:
    """旧版 key 存在 config.json 里——迁移到 .env 并从文件清除（无论 .env 是否已有值）。"""
    env = _read_env_file(ENV_PATH)
    if CONFIG_PATH.exists():
        try:
            stored = json.loads(CONFIG_PATH.read_text(encoding="utf-8"))
            file_key = (stored.get("translate") or {}).get("api_key") or ""
            if file_key:
                if not env.get("LLM_API_KEY"):
                    env["LLM_API_KEY"] = file_key
                    _write_env_file(ENV_PATH, env)
                stored.get("translate", {}).pop("api_key", None)
                tmp = CONFIG_PATH.with_suffix(".tmp")
                tmp.write_text(json.dumps(stored, ensure_ascii=False, indent=2), encoding="utf-8")
                tmp.replace(CONFIG_PATH)
        except Exception:
            pass
    cfg.setdefault("translate", {})["api_key"] = env.get("LLM_API_KEY", "")


# ---------- 配置加载/保存 ----------
def _deep_merge(base: dict, override: dict) -> dict:
    out = copy.deepcopy(base)
    for k, v in (override or {}).items():
        if isinstance(v, dict) and isinstance(out.get(k), dict):
            out[k] = _deep_merge(out[k], v)
        else:
            out[k] = copy.deepcopy(v)
    return out


def load_config(force: bool = False) -> dict:
    global _cache
    with _lock:
        if _cache is not None and not force:
            return copy.deepcopy(_cache)
        ensure_dirs()
        cfg = copy.deepcopy(DEFAULT_CONFIG)
        if CONFIG_PATH.exists():
            try:
                stored = json.loads(CONFIG_PATH.read_text(encoding="utf-8"))
                cfg = _deep_merge(cfg, stored)
            except Exception:
                pass  # 配置损坏时回退默认值
        _migrate_and_load_key(cfg)
        _cache = cfg
        return copy.deepcopy(cfg)


def save_config(cfg: dict) -> None:
    """持久化配置；api_key 只存 .env，不写进 config.json。"""
    global _cache
    with _lock:
        ensure_dirs()
        persist = copy.deepcopy(cfg)
        if isinstance(persist.get("translate"), dict):
            persist["translate"].pop("api_key", None)
        tmp = CONFIG_PATH.with_suffix(".tmp")
        tmp.write_text(json.dumps(persist, ensure_ascii=False, indent=2), encoding="utf-8")
        tmp.replace(CONFIG_PATH)
        _cache = copy.deepcopy(cfg)


def get_config() -> dict:
    return load_config()


def update_config(patch: dict) -> dict:
    cfg = load_config()
    merged = _deep_merge(cfg, patch or {})
    save_config(merged)
    return load_config()


def public_config(cfg: dict) -> dict:
    """脱敏副本：api_key 永远以 *** 返回，附带 has_api_key。"""
    pub = copy.deepcopy(cfg)
    tr = pub.setdefault("translate", {})
    key = tr.pop("api_key", "") or ""
    tr["api_key"] = "***" if key else ""
    tr["has_api_key"] = bool(key)
    return pub
