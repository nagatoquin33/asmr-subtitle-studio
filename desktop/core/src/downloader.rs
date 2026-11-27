//! yt-dlp 封装：子进程解析/下载 bilibili、YouTube 等平台音视频。
//! 进度经自定义模板标记行（@PROG@）解析，取消通过 kill 子进程实现。

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Result};
use tokio::io::AsyncBufReadExt;

use crate::ffmpeg::{CancelFlag, Ffmpeg, StageError};
use crate::paths::find_in_path;

#[derive(Debug, Clone)]
pub struct DownloadInfo {
    pub path: String,
    pub title: String,
    pub duration: Option<f64>,
    pub webpage_url: String,
}

fn resolve_ytdlp(paths: &crate::paths::Paths) -> Result<PathBuf> {
    let dir = paths.tools_ytdlp();
    [dir.join("yt-dlp.exe"), dir.join("yt-dlp")]
        .into_iter()
        .find(|p| p.is_file())
        .or_else(|| find_in_path("yt-dlp"))
        .ok_or_else(|| anyhow!("未找到 yt-dlp，请检查 tools/yt-dlp 目录"))
}

fn format_choice(height: u32) -> String {
    format!("bv*[height<={height}]+ba/b[height<={height}]/bv*+ba/b")
}

/// 错误信息人性化（对齐 Python 版 _human_error）。
fn human_error(msg: &str) -> String {
    let low = msg.to_lowercase();
    let hint = if low.contains("unsupported url") {
        "不支持该链接（yt-dlp 无法识别）"
    } else if low.contains("login")
        || low.contains("sign in")
        || low.contains("cookies")
        || msg.contains("登录")
    {
        "该视频需要登录：可在设置里配置浏览器 cookies（chrome/edge/firefox）后重试"
    } else if low.contains("private") || msg.contains("会员") || low.contains("charge") {
        "该视频为私密/付费内容，无法下载"
    } else if low.contains("timed out")
        || low.contains("connection")
        || low.contains("unreachable")
        || low.contains("unable to download")
    {
        "网络连接失败：若是 YouTube 等境外平台，请在设置里配置代理"
    } else {
        ""
    };
    let trimmed: String = msg.chars().take(160).collect();
    if hint.is_empty() {
        trimmed
    } else {
        format!("{}（{}）", hint, trimmed)
    }
}

/// 下载进度回调：(已下载字节, 总字节(0=未知), 速度 B/s)
pub type DownloadProgress = Arc<dyn Fn(u64, u64, Option<f64>) + Send + Sync>;

/// 下载音视频到 dest_dir（合并为 mp4）。
pub async fn download_media(
    paths: &crate::paths::Paths,
    ffmpeg: Option<&Ffmpeg>,
    url: &str,
    dest_dir: &Path,
    height: u32,
    proxy: &str,
    cookies_from_browser: &str,
    progress_cb: DownloadProgress,
    cancel: CancelFlag,
) -> Result<DownloadInfo, StageError> {
    download_media_with_options(
        paths,
        ffmpeg,
        url,
        dest_dir,
        height,
        proxy,
        cookies_from_browser,
        false,
        progress_cb,
        cancel,
    )
    .await
}

/// 下载音视频到 dest_dir，可选同时保存缩略图作为封面。
#[allow(clippy::too_many_arguments)]
pub async fn download_media_with_options(
    paths: &crate::paths::Paths,
    ffmpeg: Option<&Ffmpeg>,
    url: &str,
    dest_dir: &Path,
    height: u32,
    proxy: &str,
    cookies_from_browser: &str,
    write_thumbnail: bool,
    progress_cb: DownloadProgress,
    cancel: CancelFlag,
) -> Result<DownloadInfo, StageError> {
    use tokio::process::Command;
    let ytdlp = resolve_ytdlp(paths).map_err(StageError::Other)?;
    std::fs::create_dir_all(dest_dir).ok();

    let mut cmd = Command::new(&ytdlp);
    cmd.args([
        "--no-playlist",
        "--newline",
        "--no-warnings",
        "--windows-filenames",
        "--merge-output-format",
        "mp4",
        "--retries",
        "5",
        "--fragment-retries",
        "5",
        "--concurrent-fragments",
        "8",
        "-f",
        &format_choice(height),
        "-P",
    ])
    .arg(dest_dir)
    .arg("-o")
    .arg("%(title).60B-%(id).20s.%(ext)s");
    if write_thumbnail {
        cmd.arg("--write-thumbnail");
    }
    cmd.args(["--progress", "--progress-template"])
    .arg("download:@PROG@ %(progress.downloaded_bytes)s %(progress.total_bytes)s %(progress.total_bytes_estimate)s %(progress.speed)s")
    .args(["--print", "pre_process:@META_DUR@ %(duration)s"])
    .args(["--print", "pre_process:@META_TITLE@ %(title)s"])
    .args(["--print", "after_move:@FILE@ %(filepath)s"])
    .arg("--no-simulate")
    .arg(url)
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());

    if let Some(ffmpeg) = ffmpeg {
        let dir = ffmpeg.bin_dir();
        if !dir.as_os_str().is_empty() {
            cmd.args(["--ffmpeg-location"]).arg(dir);
        }
    }
    if !proxy.trim().is_empty() {
        cmd.args(["--proxy"]).arg(proxy.trim());
    }
    if !cookies_from_browser.trim().is_empty() {
        cmd.args(["--cookies-from-browser"]).arg(cookies_from_browser.trim());
    }
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000);

    cmd.kill_on_drop(true);
    let mut child = cmd
        .spawn()
        .map_err(|e| StageError::Other(anyhow!("启动 yt-dlp 失败: {}", e)))?;

    // 目录字节监控：fragment/合并/重试模式下 yt-dlp 的模板进度行不可靠，
    // 直接采样任务目录大小作为下载进度主来源（@PROG@ 行仅补充速度与总量）。
    let monitor_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let total_hint = Arc::new(std::sync::atomic::AtomicU64::new(0));
    {
        let dir = dest_dir.to_path_buf();
        let stop = Arc::clone(&monitor_stop);
        let hint = Arc::clone(&total_hint);
        let progress = Arc::clone(&progress_cb);
        tokio::spawn(async move {
            let mut last = 0u64;
            let mut last_ms = 0u64;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                let bytes = dir_size(&dir);
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0);
                if bytes != last && now > last_ms {
                    // 采样差分估算速度（B/s）
                    let speed = ((bytes - last) as f64 / (now - last_ms) as f64 * 1000.0).max(0.0);
                    last = bytes;
                    last_ms = now;
                    let total = hint.load(std::sync::atomic::Ordering::Relaxed);
                    progress(bytes, total, Some(speed));
                }
                tokio::time::sleep(std::time::Duration::from_millis(700)).await;
            }
        });
    }

    let stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take();
    let stderr_task = tokio::spawn(async move {
        use tokio::io::AsyncReadExt;
        let mut buf = Vec::new();
        if let Some(mut s) = stderr {
            let _ = s.read_to_end(&mut buf).await;
        }
        String::from_utf8_lossy(&buf).to_string()
    });

    let mut reader = tokio::io::BufReader::new(stdout).lines();
    let mut final_path: Option<String> = None;
    let mut duration: Option<f64> = None;
    let mut title: Option<String> = None;
    let mut canceled = false;

    while let Ok(Some(line)) = reader.next_line().await {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("@PROG@") {
            let parts: Vec<&str> = rest.trim().split_whitespace().collect();
            let num = |i: usize| parts.get(i).and_then(|v| v.parse::<f64>().ok()).filter(|v| *v > 0.0);
            let done = num(0).unwrap_or(0.0) as u64;
            let total = num(1).or_else(|| num(2)).unwrap_or(0.0) as u64;
            let speed = num(3);
            if total > 0 {
                total_hint.store(total, std::sync::atomic::Ordering::Relaxed);
            }
            if done > 0 {
                progress_cb(done, total, speed);
            }
        } else if let Some(rest) = line.strip_prefix("@FILE@") {
            let p = rest.trim().trim_matches('"').to_string();
            if !p.is_empty() {
                final_path = Some(p);
            }
        } else if let Some(rest) = line.strip_prefix("@META_DUR@") {
            if let Ok(v) = rest.trim().parse::<f64>() {
                duration = Some(v);
            }
        } else if let Some(rest) = line.strip_prefix("@META_TITLE@") {
            let t = rest.trim().to_string();
            if !t.is_empty() {
                title = Some(t);
            }
        }
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            canceled = true;
            let _ = child.kill().await;
            break;
        }
    }

    let status = child.wait().await.map_err(|e| StageError::Other(e.into()))?;
    monitor_stop.store(true, std::sync::atomic::Ordering::Relaxed);
    if canceled || cancel.load(std::sync::atomic::Ordering::Relaxed) {
        return Err(StageError::Canceled("用户取消了下载".into()));
    }
    if !status.success() {
        let err = stderr_task.await.unwrap_or_default();
        let tail: String = err.chars().rev().take(300).collect::<Vec<_>>().into_iter().rev().collect();
        return Err(StageError::Other(anyhow!("{}", human_error(tail.trim()))));
    }

    // 兜底：取目录里最新的媒体文件
    let path = match final_path {
        Some(p) if Path::new(&p).exists() => PathBuf::from(p),
        _ => newest_media(dest_dir).ok_or_else(|| StageError::Other(anyhow!("下载完成但未找到产物文件")))?,
    };
    Ok(DownloadInfo {
        title: title
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| {
                path.file_stem()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_default()
            })
            .chars()
            .take(120)
            .collect(),
        path: path.to_string_lossy().to_string(),
        duration,
        webpage_url: url.to_string(),
    })
}

/// 目录内所有文件字节总和（浅层即可：yt-dlp 产物都在同一目录）。
fn dir_size(dir: &Path) -> u64 {
    let mut total = 0u64;
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            if let Ok(meta) = entry.metadata() {
                if meta.is_file() {
                    total += meta.len();
                }
            }
        }
    }
    total
}

fn newest_media(dir: &Path) -> Option<PathBuf> {
    const EXTS: &[&str] = &["mp4", "mkv", "webm", "m4a", "mp3", "flac", "wav"];
    let mut best: Option<(SystemTime, PathBuf)> = None;
    for entry in std::fs::read_dir(dir).ok()? {
        let entry = entry.ok()?;
        let path = entry.path();
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_lowercase())
            .unwrap_or_default();
        if !path.is_file() || !EXTS.contains(&ext.as_str()) {
            continue;
        }
        let mtime = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .unwrap_or(UNIX_EPOCH);
        if best.as_ref().map(|(t, _)| mtime > *t).unwrap_or(true) {
            best = Some((mtime, path));
        }
    }
    best.map(|(_, p)| p)
}
