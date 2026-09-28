@echo off
setlocal
cd /d "%~dp0"
title ASMR 字幕工作室 - 打包

if not exist .venv (
    echo [错误] 未找到 .venv，请先运行 start.bat 完成首次安装
    pause & exit /b 1
)
call .venv\Scripts\activate.bat

pip show pyinstaller >nul 2>&1 || pip install pyinstaller -q -i https://pypi.tuna.tsinghua.edu.cn/simple

echo [1/3] PyInstaller 打包（首次约 5-10 分钟）...
pyinstaller --noconfirm --clean ^
  --name asmr-subtitle-studio ^
  --add-data "frontend;frontend" ^
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

echo [2/3] 复制 ffmpeg...
robocopy tools\ffmpeg "dist\asmr-subtitle-studio\tools\ffmpeg" /E /NFL /NDL /NJH /NJS >nul

echo [3/3] 完成！
echo   程序: dist\asmr-subtitle-studio\asmr-subtitle-studio.exe
echo   双击即可运行（整个 dist\asmr-subtitle-studio 文件夹可拷贝到其他 Windows 电脑使用）
echo   注意: 首次运行可能触发 Windows SmartScreen 提示，点"仍要运行"即可
pause
