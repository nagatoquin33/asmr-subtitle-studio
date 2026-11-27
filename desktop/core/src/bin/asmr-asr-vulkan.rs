//! 安装包中的 Vulkan 转写进程：支持 `--probe` 探测显卡，以及按行接收连续任务的常驻复用模式。
use std::ffi::{c_char, c_void, CStr};
use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc,
};
use std::time::Duration;

use anyhow::{bail, Result};
use asmr_core::asr_worker::{Event, Request};
use whisper_rs::whisper_rs_sys::ggml_log_level;

static USED_GPU: AtomicBool = AtomicBool::new(false);
const IDLE_TIMEOUT: Duration = Duration::from_secs(90);

unsafe extern "C" fn record_backend(_level: ggml_log_level, text: *const c_char, _: *mut c_void) {
    if text.is_null() {
        return;
    }
    let line = unsafe { CStr::from_ptr(text) }.to_bytes();
    if let Some(detail) = line.strip_prefix(b"whisper_backend_init_gpu: ") {
        if detail.starts_with(b"using ") {
            USED_GPU.store(true, Ordering::Relaxed);
        } else if detail.starts_with(b"failed to initialize ")
            || detail.starts_with(b"no GPU found")
        {
            USED_GPU.store(false, Ordering::Relaxed);
        }
    }
}

fn send(event: Event) -> Result<()> {
    let mut out = io::stdout().lock();
    serde_json::to_writer(&mut out, &event)?;
    writeln!(out)?;
    out.flush()?;
    Ok(())
}

fn run_probe() -> Result<()> {
    let devices = whisper_rs::vulkan::list_devices();
    let device = devices
        .first()
        .map(|d| d.name.trim().to_owned())
        .filter(|s| !s.is_empty());
    let payload = serde_json::json!({
        "available": !devices.is_empty(),
        "device": device,
        "count": devices.len(),
    });
    let mut out = io::stdout().lock();
    serde_json::to_writer(&mut out, &payload)?;
    writeln!(out)?;
    out.flush()?;
    Ok(())
}

fn run_server() -> Result<()> {
    if whisper_rs::vulkan::list_devices().is_empty() {
        bail!("没有可用的 Vulkan 设备");
    }
    unsafe { whisper_rs::set_log_callback(Some(record_backend), std::ptr::null_mut()) };

    // 独立线程监听 stdin：主程序退出或关闭管道时立即退出，避免残留进程阻挡升级。
    let (tx, rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        let stdin = io::stdin();
        let mut reader = stdin.lock();
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => std::process::exit(0),
                Ok(_) => {
                    if line.trim().is_empty() {
                        continue;
                    }
                    if tx.send(line).is_err() {
                        std::process::exit(0);
                    }
                }
            }
        }
    });

    let mut last_model: Option<PathBuf> = None;
    loop {
        let line = match rx.recv_timeout(IDLE_TIMEOUT) {
            Ok(line) => line,
            Err(mpsc::RecvTimeoutError::Timeout) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                // 空闲超时或管道关闭：主动退出释放显存。
                return Ok(());
            }
        };
        let mut request: Request = match serde_json::from_str(&line) {
            Ok(req) => req,
            Err(err) => {
                send(Event::Error(format!("无效转写请求：{err}")))?;
                continue;
            }
        };
        let model_path = request.options.model_path.clone();
        let model_changed = last_model.as_ref() != Some(&model_path);
        if model_changed {
            USED_GPU.store(false, Ordering::Relaxed);
        }
        request.options.device = "gpu".into();
        let outcome = asmr_core::asr::transcribe(
            request.options,
            &request.wav,
            Arc::new(|progress| {
                let _ = send(Event::Progress(progress));
            }),
            Arc::new(AtomicBool::new(false)),
        );
        match outcome {
            Ok(mut result) => {
                if USED_GPU.load(Ordering::Relaxed) {
                    last_model = Some(model_path);
                } else {
                    last_model = None;
                }
                if result.meta["device"] == "gpu" && !USED_GPU.load(Ordering::Relaxed) {
                    result.meta["device"] = serde_json::json!("cpu");
                    result.meta["warning"] =
                        serde_json::json!("Whisper 未能初始化 Vulkan 后端，已改用 CPU");
                }
                send(Event::Complete(result))?;
            }
            Err(err) => {
                last_model = None;
                send(Event::Error(format!("{err:#}")))?;
            }
        }
    }
}

fn main() {
    if std::env::args().any(|arg| arg == "--probe") {
        if let Err(err) = run_probe() {
            let _ = writeln!(
                io::stdout(),
                "{}",
                serde_json::json!({ "available": false, "error": format!("{err:#}") })
            );
            std::process::exit(1);
        }
        return;
    }
    if let Err(err) = run_server() {
        let _ = send(Event::Error(format!("{err:#}")));
        std::process::exit(1);
    }
}
