//! 成品视频渲染：双语字幕与原视频合成。
//!
//! - 硬字幕：`ass` 滤镜烧录进画面（NVENC 硬件编码优先，失败自动回退 libx264）
//! - 软字幕：mkv 封装（`-c copy`，秒级完成，播放器可开关字幕）
//! - 进度经 `-progress pipe:1` 解析；取消通过 kill 子进程实现。

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

use crate::ffmpeg::{CancelFlag, Ffmpeg};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// ass 滤镜的路径转义：以产物目录为 CWD 传相对文件名（规避盘符冒号），
/// 单引号用 '\'' 序列转义。
fn escape_ass_filename(name: &str) -> String {
    name.replace('\'', "'\\''")
}

fn build_cmd(ff: &Ffmpeg, dir: &Path, args: &[String]) -> Command {
    let mut cmd = Command::new(ff.path());
    cmd.args(args)
        .current_dir(dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);
    cmd
}

/// 跑一次 ffmpeg，解析 out_time_ms 上报进度；返回 (是否成功, stderr 尾部)。
async fn run_with_progress(
    ff: &Ffmpeg,
    dir: &Path,
    args: &[String],
    duration: Option<f64>,
    progress: Arc<dyn Fn(f64) + Send + Sync>,
    cancel: CancelFlag,
) -> (bool, String) {
    let mut child = match build_cmd(ff, dir, args).spawn() {
        Ok(c) => c,
        Err(e) => return (false, format!("启动 ffmpeg 失败: {}", e)),
    };
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

    let mut reader = BufReader::new(stdout).lines();
    let mut canceled = false;
    while let Ok(Some(line)) = reader.next_line().await {
        let line = line.trim();
        if let Some(v) = line
            .strip_prefix("out_time_ms=")
            .or_else(|| line.strip_prefix("out_time_us="))
        {
            if let Ok(us) = v.trim().parse::<f64>() {
                if let Some(d) = duration {
                    if d > 0.0 && us > 0.0 {
                        progress((us / 1_000_000.0 / d).min(1.0));
                    }
                }
            }
        }
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            canceled = true;
            let _ = child.kill().await;
            break;
        }
    }
    let status = child.wait().await;
    if canceled {
        return (false, "__CANCELED__".into());
    }
    match status {
        Ok(s) if s.success() => {
            progress(1.0);
            (true, String::new())
        }
        _ => {
            let err = stderr_task.await.unwrap_or_default();
            let tail: String = err.chars().rev().take(400).collect::<Vec<_>>().into_iter().rev().collect();
            (false, tail)
        }
    }
}

/// 渲染成品视频。
pub async fn render_video(
    ff: &Ffmpeg,
    video: &Path,
    ass: &Path,
    dest: &Path,
    duration: Option<f64>,
    soft: bool,
    progress: Arc<dyn Fn(f64) + Send + Sync>,
    cancel: CancelFlag,
) -> Result<()> {
    let dir = dest
        .parent()
        .ok_or_else(|| anyhow!("保存路径无效: {}", dest.display()))?
        .to_path_buf();
    let _ = std::fs::create_dir_all(&dir);
    let ass_name = ass
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| anyhow!("字幕文件路径含非法字符"))?
        .to_string();

    let (args, attempt_note): (Vec<String>, &str) = if soft {
        // 软字幕：流复制封装进 mkv，秒级
        (
            vec![
                "-y".into(),
                "-i".into(),
                video.to_string_lossy().to_string(),
                "-i".into(),
                ass.to_string_lossy().to_string(),
                "-map".into(),
                "0".into(),
                "-map".into(),
                "1".into(),
                "-c".into(),
                "copy".into(),
                "-c:s".into(),
                "ass".into(),
                "-disposition:s:0".into(),
                "default".into(),
                dest.to_string_lossy().to_string(),
            ],
            "软字幕封装",
        )
    } else {
        // 硬字幕：NVENC 优先（RTX 显卡），失败回退 libx264
        (
            vec![
                "-y".into(),
                "-i".into(),
                video.to_string_lossy().to_string(),
                "-vf".into(),
                format!("ass='{}'", escape_ass_filename(&ass_name)),
                "-c:v".into(),
                "h264_nvenc".into(),
                "-preset".into(),
                "p4".into(),
                "-cq".into(),
                "23".into(),
                "-b:v".into(),
                "0".into(),
                "-c:a".into(),
                "copy".into(),
                dest.to_string_lossy().to_string(),
            ],
            "NVENC 硬件编码",
        )
    };

    let (ok, err) = run_with_progress(ff, &dir, &args, duration, Arc::clone(&progress), Arc::clone(&cancel)).await;
    if ok {
        return Ok(());
    }
    if err == "__CANCELED__" {
        let _ = std::fs::remove_file(dest);
        return Err(anyhow!("用户取消了导出"));
    }
    if soft {
        return Err(anyhow!("{}失败: {}", attempt_note, err.trim()));
    }

    // NVENC 失败（无 N 卡/驱动不支持）→ libx264 软编码重试
    eprintln!("[render] {} 失败（{}），回退 libx264", attempt_note, err.chars().take(120).collect::<String>());
    let args = vec![
        "-y".into(),
        "-i".into(),
        video.to_string_lossy().to_string(),
        "-vf".into(),
        format!("ass='{}'", escape_ass_filename(&ass_name)),
        "-c:v".into(),
        "libx264".into(),
        "-preset".into(),
        "veryfast".into(),
        "-crf".into(),
        "21".into(),
        "-c:a".into(),
        "copy".into(),
        dest.to_string_lossy().to_string(),
    ];
    let (ok, err) = run_with_progress(ff, &dir, &args, duration, progress, cancel).await;
    if ok {
        return Ok(());
    }
    if err == "__CANCELED__" {
        let _ = std::fs::remove_file(dest);
        return Err(anyhow!("用户取消了导出"));
    }
    Err(anyhow!("硬字幕渲染失败: {}", err.trim()))
}

/// 产物命名：{原名}-硬字幕.mp4 / {原名}-软字幕.mkv
pub fn burned_name(source_video: &Path, soft: bool) -> String {
    let stem = source_video
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("video");
    if soft {
        format!("{}-软字幕.mkv", stem)
    } else {
        format!("{}-硬字幕.mp4", stem)
    }
}

#[allow(dead_code)]
fn _unused(p: PathBuf) -> PathBuf {
    p
}
