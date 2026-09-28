@echo off
setlocal
cd /d "%~dp0"
title ASMR 字幕工作室

if not exist .venv (
    echo [安装] 首次运行，创建虚拟环境...
    python -m venv .venv || py -3 -m venv .venv
    call .venv\Scripts\activate.bat
    echo [安装] 安装依赖（首次较慢）...
    python -m pip install -r requirements.txt -q
) else (
    call .venv\Scripts\activate.bat
)

python run.py
