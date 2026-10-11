@echo off
setlocal
cd /d "%~dp0"
title ASMR 字幕工作室 - 打包

if not exist .venv (
    echo [错误] 未找到 .venv，请先运行 start.bat 完成首次安装
    pause & exit /b 1
)
call .venv\Scripts\activate.bat

if not exist assets\icon.ico (
    echo [图标] 生成应用图标...
    python scripts\make_icon.py
)

pip show pyinstaller >nul 2>&1 || pip install pyinstaller -q -i https://pypi.tuna.tsinghua.edu.cn/simple
pip show pillow >nul 2>&1 || pip install pillow -q -i https://pypi.tuna.tsinghua.edu.cn/simple

echo [1/2] PyInstaller 单文件打包（约 5-15 分钟，产物约 150-250MB）...
pyinstaller --noconfirm --clean --onefile ^
  --name "ASMR字幕工作室" ^
  --icon "assets\icon.ico" ^
  --add-data "frontend;frontend" ^
  --add-binary "tools\ffmpeg\bin\ffmpeg.exe;ffmpeg_bin" ^
  --add-binary "tools\ffmpeg\bin\ffprobe.exe;ffmpeg_bin" ^
  --collect-all faster_whisper ^
  --collect-all ctranslate2 ^
  --collect-all onnxruntime ^
  --collect-all av ^
  --collect-all tokenizers ^
  --collect-all yt_dlp ^
  --collect-all webview ^
  --collect-all pythonnet ^
  --collect-all clr_loader ^
  --exclude-module torch ^
  --exclude-module modelscope ^
  --exclude-module tkinter ^
  run.py
if errorlevel 1 (
    echo [失败] 打包出错，见上方日志
    pause & exit /b 1
)

echo [2/2] 完成！
echo   产物: dist\ASMR字幕工作室.exe  （单文件，拷到任何位置/任何电脑双击即用）
echo   说明: 首次启动需解压依赖（数秒到十几秒）；首次识别会自动从魔搭下载模型
echo         可能触发 SmartScreen/杀软提示，属 PyInstaller 常见误报
pause
