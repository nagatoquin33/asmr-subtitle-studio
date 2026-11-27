@echo off
rem ============================================================
rem ASMR Studio (desktop) release build  [ASCII only: cmd parses bat as ANSI]
rem   build-release.bat            -> CPU universal (Win10/11 x64)
rem   build-release.bat gpu        -> Vulkan GPU build (faster on dGPU)
rem Outputs:
rem   dist\asmr-studio-cpu.exe / dist\asmr-studio-gpu.exe
rem   (rename to Chinese display names outside this script)
rem One-time deps: rustup(MSVC) / VS BuildTools 2022 / CMake / LLVM(libclang)
rem GPU also needs Vulkan SDK (scoop install vulkan), see VULKAN_SDK below
rem ============================================================
setlocal
cd /d "%~dp0"

set "VS_VCVARS=C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat"
set "VS_NINJA=C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\Common7\IDE\CommonExtensions\Microsoft\CMake\Ninja"
set "VULKAN_SDK=C:\Users\nagatoquin\scoop\apps\vulkan\current"

if /i "%~1"=="gpu" goto gpu

:cpu
set "FEATURES=bundled-tools"
set "OUT=dist\asmr-studio-cpu.exe"
set "SRC=target\release\asmr-studio.exe"
goto build

:gpu
rem must run sequentially: inside an if-block, %PATH% expands before
rem vcvars injects MSVC dirs, which later breaks link.exe resolution
set "FEATURES=bundled-tools,whisper-vulkan"
set "OUT=dist\asmr-studio-gpu.exe"
set "SRC=target\gpu\release\asmr-studio.exe"
rem separate target dir: whisper.cpp cmake cache uses different generators
set "CARGO_TARGET_DIR=%~dp0target\gpu"
rem ggml vulkan-shaders-gen nested cmake needs vcvars + Ninja generator
call "%VS_VCVARS%" >nul
set "CMAKE_GENERATOR=Ninja"
set "PATH=%VULKAN_SDK%\Bin;%VS_NINJA%;%PATH%"

:build
set "PATH=%USERPROFILE%\.cargo\bin;C:\Program Files\CMake\bin;C:\Program Files\LLVM\bin;%PATH%"
set "LIBCLANG_PATH=C:\Program Files\LLVM\bin"

echo [1/2] cargo build --release -p asmr-studio --features %FEATURES%
cargo build --release -p asmr-studio --features %FEATURES%
if errorlevel 1 (
  echo [ERROR] build failed
  exit /b 1
)

echo [2/2] copy artifact...
if not exist dist mkdir dist
copy /y "%SRC%" "%OUT%" >nul
if not exist "%OUT%" (
  echo [ERROR] copy failed
  exit /b 1
)

echo DONE: %CD%\%OUT%
endlocal
