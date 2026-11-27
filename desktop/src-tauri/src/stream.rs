//! asmrmedia:// 自定义协议：任务媒体流（支持 Range，供 <video> 拖动）与 VTT 字幕轨。
//!
//! 前端 URL 形态（Windows/WebView2）：http://asmrmedia.localhost/{task_id}/{kind}
//! kind ∈ { media, vtt }

use std::path::Path;

use asmr_core::pipeline::Core;
use std::sync::Arc;
use tauri::http::{self, HeaderMap, Request, Response};
use tauri::UriSchemeResponder;

const CHUNK: u64 = 4 * 1024 * 1024; // 单次响应最大分片

pub fn mime_for(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase())
        .as_deref()
    {
        Some("mp4") | Some("m4v") => "video/mp4",
        Some("mkv") => "video/x-matroska",
        Some("webm") => "video/webm",
        Some("mov") => "video/quicktime",
        Some("avi") => "video/x-msvideo",
        Some("ts") => "video/mp2t",
        Some("flv") => "video/x-flv",
        Some("wmv") => "video/x-ms-wmv",
        Some("mpg") | Some("mpeg") => "video/mpeg",
        Some("wav") => "audio/wav",
        Some("mp3") => "audio/mpeg",
        Some("m4a") => "audio/mp4",
        Some("aac") => "audio/aac",
        Some("flac") => "audio/flac",
        Some("ogg") | Some("oga") | Some("opus") => "audio/ogg",
        Some("wma") => "audio/x-ms-wma",
        Some("vtt") => "text/vtt; charset=utf-8",
        _ => "application/octet-stream",
    }
}

fn parse_range(headers: &HeaderMap, size: u64) -> Option<(u64, u64)> {
    let raw = headers.get("range")?.to_str().ok()?;
    let spec = raw.strip_prefix("bytes=")?;
    let (start_s, end_s) = spec.split_once('-')?;
    let start: u64 = if start_s.is_empty() {
        0
    } else {
        start_s.parse().ok()?
    };
    let end: u64 = if end_s.is_empty() {
        size.saturating_sub(1)
    } else {
        end_s.parse::<u64>().ok()?.min(size.saturating_sub(1))
    };
    if start > end || start >= size {
        return None; // 无效
    }
    // 大区间按 CHUNK 截断（播放器会继续请求后续区间）
    let end = end.min(start + CHUNK - 1).min(size - 1);
    Some((start, end))
}

fn read_slice(path: &Path, start: u64, len: u64) -> Result<Vec<u8>, String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path).map_err(|e| format!("打开媒体失败: {}", e))?;
    f.seek(SeekFrom::Start(start)).map_err(|e| e.to_string())?;
    let mut buf = vec![0u8; len as usize];
    let mut read = 0usize;
    while read < buf.len() {
        match f.read(&mut buf[read..]) {
            Ok(0) => break,
            Ok(n) => read += n,
            Err(e) => return Err(e.to_string()),
        }
    }
    buf.truncate(read);
    Ok(buf)
}

fn error_response(status: u16, msg: &str) -> Response<Vec<u8>> {
    http::Response::builder()
        .status(status)
        .header("Content-Type", "text/plain; charset=utf-8")
        .header("Cache-Control", "no-store")
        .body(msg.as_bytes().to_vec())
        .unwrap()
}

/// 协议处理器：解析 /{task_id}/{kind}，按 Range 返回分片。
pub fn handle(core: Arc<Core>, request: Request<Vec<u8>>, responder: UriSchemeResponder) {
    std::thread::spawn(move || {
        let resp = respond(&core, &request);
        responder.respond(resp);
    });
}

fn respond(core: &Core, request: &Request<Vec<u8>>) -> Response<Vec<u8>> {
    // Windows/WebView2 下 request.uri() 是完整 URL（http://asmrmedia.localhost/{id}/{kind}），
    // 必须用 path() 只取路径部分——to_string() 会把 scheme/host 混进分段导致 404
    let path = request.uri().path().to_string();
    let parts: Vec<&str> = path.trim_matches('/').split('/').filter(|s| !s.is_empty()).collect();
    if parts.len() != 2 {
        return error_response(404, "无效的媒体路径");
    }
    let (task_id, kind) = (parts[0], parts[1]);

    // 目标文件
    let file = match kind {
        "media" => match asmr_core::services::media_path(core, task_id) {
            Ok(p) => p,
            Err(e) => return error_response(404, &e),
        },
        "vtt" => {
            match asmr_core::services::read_output_file(core, task_id, "bilingual_vtt") {
                Ok((_, bytes)) => {
                    return http::Response::builder()
                        .status(200)
                        .header("Content-Type", "text/vtt; charset=utf-8")
                        .header("Cache-Control", "no-store")
                        .header("Access-Control-Allow-Origin", "*")
                        .body(bytes)
                        .unwrap();
                }
                Err(e) => return error_response(404, &e),
            }
        }
        _ => return error_response(404, "未知资源类型"),
    };

    let Ok(meta) = std::fs::metadata(&file) else {
        return error_response(404, "媒体文件不存在");
    };
    let size = meta.len();
    let content_type = mime_for(&file).to_string();

    // 小文件 / 无 Range → 200 全量（>8MB 时切首个分片 206）
    match parse_range(request.headers(), size) {
        None => {
            if request.method() == "HEAD" {
                return http::Response::builder()
                    .status(200)
                    .header("Content-Type", &content_type)
                    .header("Content-Length", size.to_string())
                    .header("Accept-Ranges", "bytes")
                    .header("Cache-Control", "no-store")
                    .body(Vec::new())
                    .unwrap();
            }
            if size <= 8 * 1024 * 1024 {
                let body = match std::fs::read(&file) {
                    Ok(b) => b,
                    Err(e) => return error_response(500, &e.to_string()),
                };
                return http::Response::builder()
                    .status(200)
                    .header("Content-Type", &content_type)
                    .header("Content-Length", body.len().to_string())
                    .header("Accept-Ranges", "bytes")
                    .header("Cache-Control", "no-store")
                    .body(body)
                    .unwrap();
            }
            let (start, end) = (0u64, CHUNK.min(size) - 1);
            let body = match read_slice(&file, start, end - start + 1) {
                Ok(b) => b,
                Err(e) => return error_response(500, &e),
            };
            http::Response::builder()
                .status(206)
                .header("Content-Type", &content_type)
                .header("Content-Length", body.len().to_string())
                .header("Accept-Ranges", "bytes")
                .header(
                    "Content-Range",
                    format!("bytes {}-{}/{}", start, end, size),
                )
                .header("Cache-Control", "no-store")
                .body(body)
                .unwrap()
        }
        Some((start, end)) => {
            let body = match read_slice(&file, start, end - start + 1) {
                Ok(b) => b,
                Err(e) => return error_response(500, &e),
            };
            http::Response::builder()
                .status(206)
                .header("Content-Type", &content_type)
                .header("Content-Length", body.len().to_string())
                .header("Accept-Ranges", "bytes")
                .header(
                    "Content-Range",
                    format!("bytes {}-{}/{}", start, end, size),
                )
                .header("Cache-Control", "no-store")
                .body(body)
                .unwrap()
        }
    }
}
