//! 目录布局与首启引导：单 exe 首次运行时在同级目录展开工具、创建数据目录。
//!
//! ```text
//! 程序所在目录/
//!   ASMR字幕工作室.exe
//!   .env                  LLM API Key（不入 config.json）
//!   data/
//!     config.json
//!     tasks/{id}/         任务工作目录（下载的视频、audio.wav、segments.json、task.json）
//!     models/{name}/      ggml 模型（model.bin）+ vad/
//!   output/{视频名}/      字幕产物
//!   tools/
//!     ffmpeg/bin/         ffmpeg.exe / ffprobe.exe（内嵌解包或用户自备）
//!     yt-dlp/yt-dlp.exe
//! ```

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

#[derive(Debug, Clone)]
pub struct Paths {
    pub root: PathBuf,
}

impl Paths {
    /// root = exe 所在目录；ASMR_HOME 环境变量可覆盖（开发用）。
    pub fn detect() -> Result<Self> {
        let root = match std::env::var_os("ASMR_HOME") {
            Some(v) if !v.is_empty() => PathBuf::from(v),
            _ => std::env::current_exe()
                .context("无法定位可执行文件路径")?
                .parent()
                .context("exe 路径没有父目录")?
                .to_path_buf(),
        };
        fs::create_dir_all(&root).with_context(|| format!("创建根目录失败: {}", root.display()))?;
        Ok(Self { root })
    }

    pub fn data(&self) -> PathBuf {
        self.root.join("data")
    }
    pub fn tasks(&self) -> PathBuf {
        self.data().join("tasks")
    }
    pub fn models(&self) -> PathBuf {
        self.data().join("models")
    }
    pub fn output(&self) -> PathBuf {
        self.root.join("output")
    }
    pub fn config_path(&self) -> PathBuf {
        self.data().join("config.json")
    }
    pub fn env_path(&self) -> PathBuf {
        self.root.join(".env")
    }
    pub fn tools_ffmpeg_bin(&self) -> PathBuf {
        self.root.join("tools").join("ffmpeg").join("bin")
    }
    pub fn tools_ytdlp(&self) -> PathBuf {
        self.root.join("tools").join("yt-dlp")
    }
    pub fn model_dir(&self, name: &str) -> PathBuf {
        self.models().join(name)
    }
    pub fn vad_model_path(&self) -> PathBuf {
        self.models().join("vad").join("ggml-silero-v5.1.2.bin")
    }

    pub fn ensure_dirs(&self) -> Result<()> {
        for p in [self.data(), self.tasks(), self.models(), self.output()] {
            fs::create_dir_all(&p).with_context(|| format!("创建目录失败: {}", p.display()))?;
        }
        Ok(())
    }
}

/// 在 PATH 里查找可执行文件（name 不带扩展名，Windows 下优先 .exe）。
pub fn find_in_path(name: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(format!("{}.exe", name));
        if candidate.is_file() {
            return Some(candidate);
        }
        let bare = dir.join(name);
        if bare.is_file() {
            return Some(bare);
        }
    }
    None
}

/// 文件名清洗（对应 Python sanitize_filename：去掉 Windows 非法字符）。
pub fn sanitize_filename(name: &str) -> String {
    let base = Path::new(name)
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| name.to_string());
    let cleaned: String = base
        .chars()
        .map(|c| match c {
            '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '\r' | '\n' => '_',
            _ => c,
        })
        .collect();
    let trimmed = cleaned.trim().to_string();
    if trimmed.is_empty() {
        "media".to_string()
    } else {
        trimmed
    }
}

/// 首启引导：建目录 + 解包内嵌工具（已存在则跳过）。
/// 返回本次解包的工具描述（用于日志/UI 提示）。
pub fn bootstrap(paths: &Paths) -> Result<Vec<String>> {
    paths.ensure_dirs()?;
    #[allow(unused_mut)]
    let mut extracted = Vec::new();

    #[cfg(feature = "bundled-tools")]
    {
        const FFMPEG_ZST: &[u8] = include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../embedded/ffmpeg.exe.zst"
        ));
        const FFPROBE_ZST: &[u8] = include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../embedded/ffprobe.exe.zst"
        ));
        const YTDLP_ZST: &[u8] = include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../embedded/ytdlp.exe.zst"
        ));

        let jobs = [
            (FFMPEG_ZST, paths.tools_ffmpeg_bin().join("ffmpeg.exe"), "ffmpeg"),
            (FFPROBE_ZST, paths.tools_ffmpeg_bin().join("ffprobe.exe"), "ffprobe"),
            (YTDLP_ZST, paths.tools_ytdlp().join("yt-dlp.exe"), "yt-dlp"),
        ];
        for (bytes, dest, label) in jobs {
            if dest.exists() {
                continue; // 用户自备/已解包的版本优先，不覆盖
            }
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("创建目录失败: {}", parent.display()))?;
            }
            let tmp = dest.with_extension("exe.tmp");
            decompress_to(bytes, &tmp)
                .with_context(|| format!("解包 {label} 失败（目标 {}）", dest.display()))?;
            fs::rename(&tmp, &dest)
                .with_context(|| format!("落盘 {label} 失败: {}", dest.display()))?;
            extracted.push(label.to_string());
        }
    }

    Ok(extracted)
}

/// zstd 流式解压到文件。
#[cfg_attr(not(feature = "bundled-tools"), allow(dead_code))]
fn decompress_to(bytes: &[u8], dest: &Path) -> Result<()> {
    use std::io::{copy, Write};
    let mut decoder = zstd::stream::read::Decoder::new(bytes)?;
    let mut out = fs::File::create(dest)?;
    copy(&mut decoder, &mut out)?;
    out.flush()?;
    Ok(())
}
