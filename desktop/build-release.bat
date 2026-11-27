@echo off
setlocal
cd /d "%~dp0"

rem Release target: one x64 NSIS installer containing CPU and Vulkan transcription,
rem with ffmpeg/ffprobe/yt-dlp deployed directly as NSIS resources (LZMA solid compression).
rem Prerequisites: Rust MSVC, CMake, LLVM, Node.js, VS Build Tools and Vulkan SDK.
if not defined ASMR_BUILD_ROOT set "ASMR_BUILD_ROOT=%~d0\asmr-build"
if not defined VS_VCVARS (
  for /f "usebackq delims=" %%I in (`"%ProgramFiles(x86)%\Microsoft Visual Studio\Installer\vswhere.exe" -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath 2^>nul`) do (
    if exist "%%I\VC\Auxiliary\Build\vcvars64.bat" set "VS_VCVARS=%%I\VC\Auxiliary\Build\vcvars64.bat"
  )
)
if not defined VS_VCVARS set "VS_VCVARS=C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat"
if not defined VULKAN_SDK set "VULKAN_SDK=%USERPROFILE%\scoop\apps\vulkan\current"
if not defined LIBCLANG_PATH set "LIBCLANG_PATH=C:\Program Files\LLVM\bin"
set "PATH=%USERPROFILE%\.cargo\bin;C:\Program Files\CMake\bin;%LIBCLANG_PATH%;%VULKAN_SDK%\Bin;%PATH%"

if not exist "%VS_VCVARS%" (
  echo [ERROR] Set VS_VCVARS to the path of Visual Studio's vcvars64.bat.
  exit /b 1
)
if not exist "%VULKAN_SDK%\Bin\glslc.exe" (
  echo [ERROR] Set VULKAN_SDK to the Vulkan SDK installation directory.
  exit /b 1
)
call "%VS_VCVARS%" >nul
if errorlevel 1 exit /b 1
if defined VSINSTALLDIR set "PATH=%VSINSTALLDIR%Common7\IDE\CommonExtensions\Microsoft\CMake\CMake\bin;%VSINSTALLDIR%Common7\IDE\CommonExtensions\Microsoft\CMake\Ninja;%PATH%"
where ninja.exe >nul 2>nul
if errorlevel 1 (
  echo [ERROR] Ninja is required. Add it to PATH.
  exit /b 1
)
set "CMAKE_GENERATOR=Ninja"

rem The Vulkan shader build needs a short, writable build directory.
if not exist "%ASMR_BUILD_ROOT%" mkdir "%ASMR_BUILD_ROOT%"
if not exist "%ASMR_BUILD_ROOT%" (
  echo [ERROR] Set ASMR_BUILD_ROOT to a short, writable path, e.g. D:\asmr-build.
  exit /b 1
)
rustc -vV | findstr /c:"host: x86_64-pc-windows-msvc" >nul
if errorlevel 1 (
  echo [ERROR] The release build requires the x64 MSVC Rust toolchain.
  exit /b 1
)

echo [1/4] Building the Vulkan worker...
set "CARGO_TARGET_DIR=%ASMR_BUILD_ROOT%\v"
cargo build --locked --release -p asmr-core --features whisper-vulkan --bin asmr-asr-vulkan
if errorlevel 1 exit /b 1
if not exist "src-tauri\build" mkdir "src-tauri\build"
copy /y "%CARGO_TARGET_DIR%\release\asmr-asr-vulkan.exe" "src-tauri\build\asmr-asr-vulkan.exe" >nul
if errorlevel 1 exit /b 1

echo [2/4] Staging ffmpeg/ffprobe/yt-dlp resources...
set "CARGO_TARGET_DIR=%ASMR_BUILD_ROOT%\app"
cargo run --locked --release -p asmr-core --bin unpack-tools -- "embedded" "src-tauri\build\tools"
if errorlevel 1 exit /b 1

echo [3/4] Building the main application...
cargo build --locked --release --target x86_64-pc-windows-msvc -p asmr-studio
if errorlevel 1 exit /b 1

echo [4/4] Packaging the installer...
call npx --yes --package @tauri-apps/cli@2.12.6 tauri bundle --ci --target x86_64-pc-windows-msvc --bundles nsis --config "src-tauri\tauri.installer.conf.json"
if errorlevel 1 exit /b 1

for /f "delims=" %%V in ('node -p "require('./src-tauri/tauri.conf.json').version"') do set "APP_VERSION=%%V"
if not defined APP_VERSION exit /b 1
set "BUNDLE_DIR=%CARGO_TARGET_DIR%\x86_64-pc-windows-msvc\release\bundle\nsis"
if not exist "%BUNDLE_DIR%\*_%APP_VERSION%_x64-setup.exe" exit /b 1
if not exist "dist\installer" mkdir "dist\installer"
for %%F in ("%BUNDLE_DIR%\*_%APP_VERSION%_x64-setup.exe") do copy /y "%%~fF" "dist\installer\%%~nxF" >nul
if errorlevel 1 exit /b 1
echo Installer: %CD%\dist\installer
exit /b 0
