//! 安装版在独立进程中运行 Vulkan：支持常驻复用以跳过重复加载模型，显卡驱动故障时自动回退 CPU。
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{atomic::Ordering, Arc, LazyLock};
use std::{process::Stdio, time::Duration};

use anyhow::{anyhow, bail, Context, Result};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

use crate::asr::{self, AsrOptions, AsrOutcome};
use crate::ffmpeg::{CancelFlag, StageError};
use crate::paths::Paths;

#[derive(Serialize, Deserialize)]
pub struct Request {
    pub options: AsrOptions,
    pub wav: PathBuf,
}

#[derive(Serialize, Deserialize)]
pub enum Event {
    Progress(f64),
    Complete(AsrOutcome),
    Error(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VulkanProbe {
    pub installed: bool,
    pub available: bool,
    pub device_name: Option<String>,
    pub reason: Option<String>,
}

static PROBE_CACHE: LazyLock<Mutex<HashMap<PathBuf, VulkanProbe>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

struct WorkerSession {
    exe: PathBuf,
    child: Child,
    stdin: ChildStdin,
    lines: Lines<BufReader<ChildStdout>>,
}

static WORKER: tokio::sync::Mutex<Option<WorkerSession>> = tokio::sync::Mutex::const_new(None);

pub fn worker_path(paths: &Paths) -> PathBuf {
    paths.root.join("tools").join("asmr-asr-vulkan.exe")
}

#[cfg(windows)]
fn has_vulkan_loader() -> bool {
    use std::ffi::c_void;

    #[link(name = "kernel32")]
    extern "system" {
        fn LoadLibraryExW(name: *const u16, file: *mut c_void, flags: u32) -> *mut c_void;
        fn FreeLibrary(module: *mut c_void) -> i32;
    }
    const LOAD_LIBRARY_SEARCH_SYSTEM32: u32 = 0x00000800;
    let name = "vulkan-1.dll\0".encode_utf16().collect::<Vec<_>>();
    // 仅检查 Windows 系统目录，避免从当前工作目录加载同名 DLL。
    unsafe {
        let module = LoadLibraryExW(
            name.as_ptr(),
            std::ptr::null_mut(),
            LOAD_LIBRARY_SEARCH_SYSTEM32,
        );
        if module.is_null() {
            return false;
        }
        FreeLibrary(module);
        true
    }
}

#[cfg(not(windows))]
fn has_vulkan_loader() -> bool {
    false
}

/// 探测本机是否具备可用的 Vulkan 转写显卡（按 worker 路径在当前进程内缓存一次）。
pub fn probe_vulkan(paths: &Paths) -> VulkanProbe {
    let exe = worker_path(paths);
    if !exe.is_file() {
        return VulkanProbe {
            installed: false,
            available: false,
            device_name: None,
            reason: Some("未安装 Vulkan 转写组件".to_owned()),
        };
    }
    if let Some(cached) = PROBE_CACHE.lock().get(&exe).cloned() {
        return cached;
    }
    let result = if !has_vulkan_loader() {
        VulkanProbe {
            installed: true,
            available: false,
            device_name: None,
            reason: Some("系统缺少可用的 Vulkan 运行库".to_owned()),
        }
    } else {
        run_probe_once(&exe)
    };
    PROBE_CACHE.lock().insert(exe, result.clone());
    result
}

fn run_probe_once(exe: &Path) -> VulkanProbe {
    use std::process::Command as StdCommand;
    let mut cmd = StdCommand::new(exe);
    cmd.arg("--probe")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    let output = match cmd.output() {
        Ok(out) => out,
        Err(err) => {
            return VulkanProbe {
                installed: true,
                available: false,
                device_name: None,
                reason: Some(format!("无法启动 Vulkan 探测：{err}")),
            };
        }
    };
    if !output.status.success() {
        return VulkanProbe {
            installed: true,
            available: false,
            device_name: None,
            reason: Some(format!("Vulkan 驱动初始化失败（{}）", output.status)),
        };
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value = text
        .lines()
        .rev()
        .find_map(|line| serde_json::from_str(line).ok())
        .unwrap_or_default();
    let available = parsed["available"].as_bool().unwrap_or(false);
    let device_name = parsed["device"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned);
    VulkanProbe {
        installed: true,
        available,
        device_name,
        reason: if available {
            None
        } else {
            Some("未检测到可用的 Vulkan 显卡设备".to_owned())
        },
    }
}

fn spawn_session(exe: &Path) -> Result<WorkerSession> {
    let mut command = Command::new(exe);
    command
        .kill_on_drop(true)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    command.creation_flags(0x0800_0000);
    let mut child = command.spawn().context("无法启动 Vulkan 转写进程")?;
    let stdin = child.stdin.take().context("无法连接 Vulkan 输入")?;
    let stdout = child.stdout.take().context("无法读取 Vulkan 输出")?;
    let lines = BufReader::new(stdout).lines();
    Ok(WorkerSession {
        exe: exe.to_path_buf(),
        child,
        stdin,
        lines,
    })
}

async fn run_persistent_worker(
    exe: &Path,
    request: &Request,
    progress: &(dyn Fn(f64) + Send + Sync),
    cancel: &CancelFlag,
) -> Result<AsrOutcome> {
    let mut guard = WORKER.lock().await;
    let payload = serde_json::to_vec(request)?;

    // 复用常驻进程；如果已空闲退出或路径变化，则重新拉起。
    let needs_spawn = match guard.as_mut() {
        Some(session) if session.exe == exe => session.child.try_wait()?.is_some(),
        _ => true,
    };
    if needs_spawn {
        *guard = Some(spawn_session(exe)?);
    }

    // 写请求失败（如旧进程刚好在空闲边界退出）时自动重连一次。
    let write_ok = if let Some(session) = guard.as_mut() {
        session.stdin.write_all(&payload).await.is_ok()
            && session.stdin.write_all(b"\n").await.is_ok()
            && session.stdin.flush().await.is_ok()
    } else {
        false
    };
    if !write_ok {
        *guard = Some(spawn_session(exe)?);
        let session = guard.as_mut().expect("worker spawned");
        session.stdin.write_all(&payload).await?;
        session.stdin.write_all(b"\n").await?;
        session.stdin.flush().await?;
    }

    let mut tick = tokio::time::interval(Duration::from_millis(100));
    loop {
        let session = guard.as_mut().expect("worker present");
        tokio::select! {
            _ = tick.tick() => {
                if cancel.load(Ordering::Relaxed) {
                    let _ = session.child.kill().await;
                    *guard = None;
                    bail!("已取消识别");
                }
            }
            line = session.lines.next_line() => {
                let line = match line {
                    Ok(Some(line)) => line,
                    Ok(None) => {
                        let status = session.child.wait().await.ok();
                        *guard = None;
                        if let Some(st) = status {
                            bail!("Vulkan 转写进程异常退出（{st}）");
                        }
                        bail!("Vulkan 转写进程意外断开");
                    }
                    Err(err) => {
                        let _ = session.child.kill().await;
                        *guard = None;
                        return Err(err).context("读取 Vulkan 输出失败");
                    }
                };
                match serde_json::from_str::<Event>(&line).context("Vulkan 转写进程返回了无效结果")? {
                    Event::Progress(frac) => progress(frac.clamp(0.0, 1.0)),
                    Event::Complete(result) => return Ok(result),
                    Event::Error(message) => {
                        // 逻辑错误（非崩溃），保留进程或按需退出；这里直接释放确保下次干净状态。
                        *guard = None;
                        bail!("{message}");
                    }
                }
            }
        }
    }
}

pub async fn transcribe(
    paths: &Paths,
    options: AsrOptions,
    wav: PathBuf,
    progress: Arc<dyn Fn(f64) + Send + Sync>,
    cancel: CancelFlag,
) -> Result<AsrOutcome, StageError> {
    let worker = worker_path(paths);
    let mut request = Request { options, wav };
    let mut fallback = None;
    if request.options.device != "cpu" && worker.is_file() {
        let probe_paths = paths.clone();
        let probe = tokio::task::spawn_blocking(move || probe_vulkan(&probe_paths))
            .await
            .unwrap_or(VulkanProbe {
                installed: true,
                available: false,
                device_name: None,
                reason: Some("Vulkan 探测异常".to_owned()),
            });
        if probe.available {
            match run_persistent_worker(&worker, &request, progress.as_ref(), &cancel).await {
                Ok(result) => return Ok(result),
                Err(err) => fallback = Some(err.to_string()),
            }
        } else {
            fallback = probe.reason.or_else(|| Some("系统缺少可用的 Vulkan 运行库".to_owned()));
        }
        request.options.device = "cpu".into();
    }

    if cancel.load(Ordering::Relaxed) {
        return Err(StageError::Canceled("已取消识别".into()));
    }
    let requested_gpu = request.options.device == "gpu";
    let cancel_for_cpu = cancel.clone();
    let outcome = tokio::task::spawn_blocking(move || {
        asr::transcribe(request.options, &request.wav, progress, cancel_for_cpu)
    })
    .await
    .map_err(|err| StageError::Other(anyhow!("识别线程异常：{err}")))?;
    if cancel.load(Ordering::Relaxed) {
        return Err(StageError::Canceled("已取消识别".into()));
    }
    let mut outcome = outcome.map_err(StageError::Other)?;
    if let Some(reason) = fallback {
        outcome.meta["warning"] = serde_json::json!(format!("GPU 转写失败，已改用 CPU：{reason}"));
    } else if requested_gpu && outcome.meta["device"] == "cpu" && !worker.is_file() {
        outcome.meta["warning"] = serde_json::json!("未安装 Vulkan 转写组件，已改用 CPU");
    }
    Ok(outcome)
}
