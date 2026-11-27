//! 构建期小工具：将 desktop/embedded/*.zst 解压到安装包资源目录，由 NSIS 统一固实压缩部署。
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

fn unpack_one(src: &Path, dst: &Path) -> Result<()> {
    if !src.is_file() {
        bail!("找不到内嵌工具包：{}", src.display());
    }
    if dst.is_file() {
        let src_mtime = fs::metadata(src)?.modified().ok();
        let dst_mtime = fs::metadata(dst)?.modified().ok();
        if src_mtime.is_some() && dst_mtime >= src_mtime {
            return Ok(());
        }
    }
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = dst.with_extension("exe.tmp");
    let input = fs::File::open(src).with_context(|| format!("读取失败：{}", src.display()))?;
    let mut decoder = zstd::stream::read::Decoder::new(input)?;
    let mut out = fs::File::create(&tmp).with_context(|| format!("写入失败：{}", tmp.display()))?;
    io::copy(&mut decoder, &mut out)?;
    out.flush()?;
    drop(out);
    fs::rename(&tmp, dst)?;
    Ok(())
}

fn main() -> Result<()> {
    let mut args = std::env::args_os().skip(1);
    let embedded = PathBuf::from(args.next().context("用法: unpack-tools <embedded_dir> <out_tools_dir>")?);
    let out = PathBuf::from(args.next().context("用法: unpack-tools <embedded_dir> <out_tools_dir>")?);
    let jobs = [
        ("ffmpeg.exe.zst", out.join("ffmpeg").join("bin").join("ffmpeg.exe")),
        ("ffprobe.exe.zst", out.join("ffmpeg").join("bin").join("ffprobe.exe")),
        ("ytdlp.exe.zst", out.join("yt-dlp").join("yt-dlp.exe")),
    ];
    for (name, dst) in jobs {
        unpack_one(&embedded.join(name), &dst)?;
    }
    Ok(())
}
