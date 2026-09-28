#!/usr/bin/env bash
# Git Bash / Linux 启动脚本
cd "$(dirname "$0")"
if [ ! -d .venv ]; then
    echo "[安装] 首次运行，创建虚拟环境..."
    python -m venv .venv
    ./.venv/Scripts/python.exe -m pip install -r requirements.txt -q 2>/dev/null \
        || ./.venv/bin/python -m pip install -r requirements.txt -q
fi
PY=./.venv/Scripts/python.exe; [ -x "$PY" ] || PY=./.venv/bin/python
echo "[启动] http://127.0.0.1:8765"
exec "$PY" -m uvicorn backend.app:app --host 127.0.0.1 --port 8765
