//! ffmpeg/ffprobe 封装：定位二进制、探测媒体信息、抽取 16k 单声道 wav。

use std::path::{Path, PathBuf};
use std::process::{Stdio};
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use regex::Regex;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

use crate::paths::{find_in_path, Paths};

/// 取消信号：多个子进程（ffmpeg/yt-dlp/whisper）共享同一任务的信号。
pub type CancelFlag = Arc<std::sync::atomic::AtomicBool>;

#[derive(Debug, Clone)]
pub struct ProbeInfo {
    pub duration: f64,
    pub has_video: bool,
    pub size: u64,
}

#[derive(Debug)]
pub enum StageError {
    Canceled(String),
    Other(anyhow::Error),
}

impl From<anyhow::Error> for StageError {
    fn from(e: anyhow::Error) -> Self {
        StageError::Other(e)
    }
}

pub fn is_canceled(err: &StageError) -> bool {
    matches!(err, StageError::Canceled(_))
}

pub struct Ffmpeg {
    ffmpeg: PathBuf,
    ffprobe: PathBuf,
}

impl Ffmpeg {
    pub fn resolve(paths: &Paths) -> Result<Self> {
        let bin = paths.tools_ffmpeg_bin();
        let ffmpeg = [bin.join("ffmpeg.exe"), bin.join("ffmpeg")]
            .into_iter()
            .find(|p| p.is_file())
            .or_else(|| find_in_path("ffmpeg"))
            .ok_or_else(|| anyhow!("未找到 ffmpeg，请检查 tools/ffmpeg 目录"))?;
        let ffprobe = [bin.join("ffprobe.exe"), bin.join("ffprobe")]
            .into_iter()
            .find(|p| p.is_file())
            .or_else(|| find_in_path("ffprobe"))
            .ok_or_else(|| anyhow!("未找到 ffprobe，请检查 tools/ffmpeg 目录"))?;
        Ok(Self { ffmpeg, ffprobe })
    }

    pub fn path(&self) -> &Path {
        &self.ffmpeg
    }

    pub fn bin_dir(&self) -> PathBuf {
        self.ffmpeg
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_default()
    }

    pub fn version(&self) -> Option<String> {
        let mut cmd = std::process::Command::new(&self.ffmpeg);
        cmd.arg("-version").stdout(Stdio::piped()).stderr(Stdio::piped());
        #[cfg(windows)]
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW：health 每 30s 轮询，不挂标志会反复弹窗
        let out = cmd.output().ok()?;
        let text = String::from_utf8_lossy(&out.stdout);
        let re = Regex::new(r"ffmpeg version (\S+)").ok()?;
        re.captures(&text)
            .map(|c| c.get(1).unwrap().as_str().to_string())
    }

    pub async fn probe(&self, path: &Path) -> Result<ProbeInfo> {
        let mut cmd = Command::new(&self.ffprobe);
        cmd.args([
            "-v",
            "error",
            "-print_format",
            "json",
            "-show_format",
            "-show_streams",
        ])
        .arg(path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
        #[cfg(windows)]
        cmd.creation_flags(0x0800_0000);
        let out = cmd.output().await
            .context("启动 ffprobe 失败")?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            return Err(anyhow!("ffprobe 失败: {}", &err[err.len().saturating_sub(300)..]));
        }
        let data: Value = serde_json::from_slice(&out.stdout).context("解析 ffprobe 输出失败")?;
        let duration = data
            .pointer("/format/duration")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(0.0);
        let size = data
            .pointer("/format/size")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse::<u64>().ok())
            .or_else(|| std::fs::metadata(path).ok().map(|m| m.len()))
            .unwrap_or(0);
        let has_video = data
            .get("streams")
            .and_then(|s| s.as_array())
            .map(|streams| {
                streams.iter().any(|s| {
                    s.get("codec_type").and_then(|v| v.as_str()) == Some("video")
                        && s.pointer("/disposition/attached_pic").and_then(|v| v.as_i64()) != Some(1)
                })
            })
            .unwrap_or(false);
        Ok(ProbeInfo {
            duration,
            has_video,
            size,
        })
    }

    /// 抽取 16k 单声道 PCM wav；progress_cb 收到 0.0~1.0。
    pub async fn extract_audio<F>(&self, src: &Path, dst: &Path, duration: f64, progress_cb: F, cancel: CancelFlag) -> Result<(), StageError>
    where
        F: Fn(f64) + Send + 'static,
    {
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let mut cmd = Command::new(&self.ffmpeg);
        cmd.args(["-y", "-i"])
            .arg(src)
            .args(["-vn", "-ac", "1", "-ar", "16000", "-c:a", "pcm_s16le"])
            .args(["-progress", "pipe:1", "-nostats"])
            .arg(dst)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(windows)]
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        #[cfg(not(windows))]
        let _ = &mut cmd;
        let mut child = cmd
            .spawn()
            .map_err(|e| StageError::Other(anyhow!("启动 ffmpeg 失败: {}", e)))?;

        let stdout = child.stdout.take().expect("stdout piped");
        let mut reader = BufReader::new(stdout).lines();
        let stderr_handle = {
            let stderr = child.stderr.take();
            tokio::spawn(async move {
                let mut tail = String::new();
                if let Some(stderr) = stderr {
                    use tokio::io::AsyncReadExt;
                    let mut buf = Vec::new();
                    let mut file = stderr;
                    if file.read_to_end(&mut buf).await.is_ok() {
                        let text = String::from_utf8_lossy(&buf);
                        let start = text.len().saturating_sub(500);
                        tail = text[start..].to_string();
                    }
                }
                tail
            })
        };

        let mut canceled = false;
        let mut finished = false;
        while let Some(line) = reader.next_line().await.map_err(|e| StageError::Other(e.into()))? {
            let line = line.trim().to_string();
            if let Some(v) = line
                .strip_prefix("out_time_ms=")
                .or_else(|| line.strip_prefix("out_time_us="))
            {
                if let Ok(t_us) = v.trim().parse::<f64>() {
                    if duration > 0.0 && t_us > 0.0 {
                        progress_cb((t_us / 1_000_000.0 / duration).min(1.0));
                    }
                }
            } else if line == "progress=end" {
                progress_cb(1.0);
                finished = true;
            }
            if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                canceled = true;
                let _ = child.kill().await;
                break;
            }
        }
        let status = child.wait().await.map_err(|e| StageError::Other(e.into()))?;
        if canceled || cancel.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(StageError::Canceled("用户取消了音频提取".into()));
        }
        let err_tail = if !status.success() || !finished {
            stderr_handle.await.unwrap_or_default()
        } else {
            String::new()
        };
        if !status.success() || !finished {
            return Err(StageError::Other(anyhow!(
                "ffmpeg 抽取音频失败: {}",
                err_tail.trim()
            )));
        }
        let meta = std::fs::metadata(dst).map_err(|e| StageError::Other(e.into()))?;
        if meta.len() == 0 {
            return Err(StageError::Other(anyhow!("ffmpeg 未生成音频文件")));
        }
        Ok(())
    }
}

/// 供 yt-dlp 使用的 ffmpeg 目录（无 ffmpeg 时回退 PATH 查找结果）。
pub fn cached_ffmpeg_dir(ffmpeg: Option<&Ffmpeg>) -> Option<PathBuf> {
    ffmpeg.map(|f| f.bin_dir()).filter(|d| !d.as_os_str().is_empty())
}

/// GPU 探测：nvidia-smi 存在且返回正常时给出显卡名。
/// 注意：whisper.cpp 是否真正用 GPU 由编译期后端决定，这里只做展示与提示。
pub fn gpu_info() -> Option<String> {
    let mut cmd = std::process::Command::new("nvidia-smi");
    cmd.args(["--query-gpu=name", "--format=csv,noheader"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW：health 轮询期间反复弹窗
    let out = cmd.output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let name = text.lines().next()?.trim();
    if name.is_empty() { None } else { Some(name.to_string()) }
}
