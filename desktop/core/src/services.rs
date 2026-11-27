//! 业务服务层：任务创建与生命周期（命令层只做参数校验与委托）。

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::pipeline::Core;
use crate::tasks::{EventKind, RUNNING_STAGES};

const VALID_LANGS: &[&str] = &["auto", "zh", "en", "ja"];

/// 视为媒体文件的扩展名（与前端 ACCEPT_EXT 一致）。
pub const MEDIA_EXTS: &[&str] = &[
    "mp3", "wav", "flac", "m4a", "aac", "ogg", "oga", "opus", "wma", "aiff", "aif", "ape",
    "mp4", "mkv", "mov", "avi", "webm", "flv", "ts", "m4v", "wmv", "mpg", "mpeg", "3gp",
];

/// 单次批量导入的任务数上限（防误操作撑爆队列）。
const FOLDER_IMPORT_CAP: usize = 500;

fn is_media_file(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .map(|e| MEDIA_EXTS.contains(&e.to_lowercase().as_str()))
        .unwrap_or(false)
}

/// 递归收集目录下的媒体文件（跳过隐藏目录；超出上限计为 skipped）。
fn walk_media_files(dir: &Path, out: &mut Vec<PathBuf>, skipped: &mut u64, depth: usize) {
    if depth > 6 || out.len() >= FOLDER_IMPORT_CAP {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') || name.starts_with('~') {
            continue;
        }
        let p = entry.path();
        if p.is_dir() {
            walk_media_files(&p, out, skipped, depth + 1);
        } else if is_media_file(&p) {
            if out.len() < FOLDER_IMPORT_CAP {
                out.push(p);
            } else {
                *skipped += 1;
            }
        } else {
            *skipped += 1;
        }
        if out.len() >= FOLDER_IMPORT_CAP {
            return;
        }
    }
}

pub(crate) fn validated_langs(mut source_lang: String, mut target_lang: String) -> (String, String) {
    if !VALID_LANGS.contains(&source_lang.as_str()) {
        source_lang = "auto".into();
    }
    if !VALID_LANGS.contains(&target_lang.as_str()) {
        target_lang = "zh".into();
    }
    (source_lang, target_lang)
}

fn options_of(source_lang: &str, target_lang: &str) -> Value {
    json!({"source_lang": source_lang, "target_lang": target_lang})
}

fn enqueue(core: &Core, task_id: &str) {
    let _ = core.queue_tx.send(task_id.to_string());
}

/// 本地路径添加（桌面版主路径：拖拽/文件选择器给路径，零拷贝引用）。
/// 传入目录时递归展开为批量任务（只收媒体文件）。
/// 返回 (任务列表, 缺失路径, 跳过的非媒体文件数)。
pub fn create_tasks_from_paths(
    core: &Core,
    paths: &[String],
    source_lang: &str,
    target_lang: &str,
) -> (Vec<Value>, Vec<String>, u64) {
    let (sl, tl) = validated_langs(source_lang.to_string(), target_lang.to_string());
    let mut tasks = Vec::new();
    let mut missing = Vec::new();
    let mut skipped: u64 = 0;

    let mut enqueue_path = |p: PathBuf| {
        let task = core.manager.create_from_path(&p, options_of(&sl, &tl));
        enqueue(core, &task.id);
        tasks.push(serde_json::to_value(&task).unwrap_or(Value::Null));
    };

    for raw in paths {
        let p = raw.trim().trim_matches('"').to_string();
        if p.is_empty() {
            continue;
        }
        let src = PathBuf::from(&p);
        if src.is_dir() {
            // 文件夹批量导入：递归展开
            let mut found = Vec::new();
            walk_media_files(&src, &mut found, &mut skipped, 0);
            for f in found {
                enqueue_path(f);
            }
        } else if src.is_file() {
            if is_media_file(&src) {
                enqueue_path(src);
            } else {
                skipped += 1;
            }
        } else {
            missing.push(p);
        }
    }
    (tasks, missing, skipped)
}

/// URL 任务（yt-dlp 解析下载）。
pub fn create_tasks_from_urls(core: &Core, urls: &[String], source_lang: &str, target_lang: &str) -> Result<(Vec<Value>, Vec<String>), String> {
    let (sl, tl) = validated_langs(source_lang.to_string(), target_lang.to_string());
    let mut tasks = Vec::new();
    let mut invalid = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for u in urls {
        let u = u.trim().to_string();
        if u.is_empty() || !seen.insert(u.clone()) {
            continue;
        }
        let lower = u.to_lowercase();
        if !(lower.starts_with("http://") || lower.starts_with("https://")) {
            invalid.push(u);
            continue;
        }
        let task = core.manager.create_url(&u, options_of(&sl, &tl));
        enqueue(core, &task.id);
        tasks.push(serde_json::to_value(&task).unwrap_or(Value::Null));
    }
    if tasks.is_empty() && !invalid.is_empty() {
        return Err(format!("{} 个链接无效（需要 http/https 开头）", invalid.len()));
    }
    Ok((tasks, invalid))
}

pub fn get_task_detail(core: &Core, task_id: &str, with_segments: bool) -> Result<Value, String> {
    let task = core.manager.get(task_id).ok_or("任务不存在")?;
    let mut v = serde_json::to_value(&task).unwrap_or(Value::Null);
    if with_segments {
        let seg_file = core.paths.tasks().join(task_id).join("segments.json");
        if let Ok(text) = std::fs::read_to_string(&seg_file) {
            if let Ok(segs) = serde_json::from_str::<Vec<Value>>(&text) {
                v["segments"] = Value::Array(segs);
            }
        }
    }
    Ok(v)
}

pub fn cancel_task(core: &Core, task_id: &str) -> Result<Value, String> {
    core.manager
        .cancel(task_id)
        .map(|t| serde_json::to_value(&t).unwrap_or(Value::Null))
        .ok_or_else(|| "任务不存在".into())
}

pub fn retry_task(core: &Core, task_id: &str) -> Result<Value, String> {
    {
        let task = core.manager.get(task_id).ok_or("任务不存在")?;
        if RUNNING_STAGES.contains(&task.status.as_str()) {
            return Err("任务尚在运行".into());
        }
    }
    core.manager.update(task_id, true, EventKind::Progress, |t| {
        t.status = "queued".into();
        t.stage = "queued".into();
        t.progress = 0.0;
        t.message = "排队中".into();
        t.error = None;
        t.cancel = false;
        t.has_result = false;
    });
    enqueue(core, task_id);
    core.manager
        .get(task_id)
        .map(|t| serde_json::to_value(&t).unwrap_or(Value::Null))
        .ok_or_else(|| "任务不存在".into())
}

pub fn delete_task(core: &Core, task_id: &str) -> Result<(), String> {
    core.manager.delete(task_id)
}

pub fn open_task_folder(core: &Core, task_id: &str) -> Result<String, String> {
    let task = core.manager.get(task_id).ok_or("任务不存在")?;
    let mut dir = core.paths.tasks().join(task_id);
    if let Some(out) = crate::library::task_output_dir(&core.paths, &task)
        .or_else(|| task.output_dir.as_ref().map(PathBuf::from))
        .filter(|out| out.is_dir())
    {
        dir = out;
    }
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.to_string_lossy().to_string();
    #[cfg(windows)]
    {
        std::process::Command::new("explorer")
            .arg(&dir)
            .spawn()
            .map_err(|e| format!("无法打开文件夹：{}", e))?;
    }
    #[cfg(not(windows))]
    {
        std::process::Command::new("xdg-open")
            .arg(&dir)
            .spawn()
            .map_err(|e| format!("无法打开文件夹：{}", e))?;
    }
    Ok(path)
}

/// 读取指定产物文件的字节（导出/预览用）。
pub fn read_output_file(core: &Core, task_id: &str, kind: &str) -> Result<(PathBuf, Vec<u8>), String> {
    let fname = crate::subtitles::output_filename(kind).ok_or(format!("未知类型: {}", kind))?;
    let task = core.manager.get(task_id).ok_or("任务不存在")?;
    let mut candidates = Vec::new();
    if let Some(dir) = crate::library::task_output_dir(&core.paths, &task) {
        candidates.push(dir.join(fname));
    }
    if let Some(outputs) = task.outputs.as_object() {
        if let Some(p) = outputs.get(kind).and_then(|v| v.as_str()) {
            candidates.push(PathBuf::from(p));
        }
    }
    candidates.push(core.paths.tasks().join(task_id).join("output").join(fname));
    if let Some(out) = &task.output_dir {
        candidates.push(PathBuf::from(out).join(fname));
    }
    for c in candidates {
        if c.is_file() {
            let bytes = std::fs::read(&c).map_err(|e| format!("读取失败: {}", e))?;
            return Ok((c, bytes));
        }
    }
    Err("产物尚未生成".into())
}

/// 任务的媒体文件路径（流媒体协议用）。
pub fn media_path(core: &Core, task_id: &str) -> Result<PathBuf, String> {
    let task = core.manager.get(task_id).ok_or("任务不存在")?;
    if let Some((work, track)) = crate::library::task_keys(&task) {
        return crate::library::track_path(&core.paths, work, track).map_err(|e| e.to_string());
    }
    task.media
        .path
        .map(PathBuf::from)
        .filter(|p| p.exists())
        .ok_or_else(|| "媒体文件不存在".into())
}

/// 指定产物类型的实际文件路径（不读内容；渲染/打开用）。
pub fn output_path(core: &Core, task_id: &str, kind: &str) -> Result<PathBuf, String> {
    let fname = crate::subtitles::output_filename(kind).ok_or(format!("未知类型: {}", kind))?;
    let task = core.manager.get(task_id).ok_or("任务不存在")?;
    let mut candidates = Vec::new();
    if let Some(dir) = crate::library::task_output_dir(&core.paths, &task) {
        candidates.push(dir.join(fname));
    }
    if let Some(outputs) = task.outputs.as_object() {
        if let Some(p) = outputs.get(kind).and_then(|v| v.as_str()) {
            candidates.push(PathBuf::from(p));
        }
    }
    if let Some(out) = &task.output_dir {
        candidates.push(PathBuf::from(out).join(fname));
    }
    candidates.push(core.paths.tasks().join(task_id).join("output").join(fname));
    candidates
        .into_iter()
        .find(|c| c.is_file())
        .ok_or_else(|| "产物尚未生成".into())
}

/// 启动时把排队中的历史任务重新入队。
pub fn requeue_pending(core: &Core) {
    for t in core.manager.list(1000) {
        if t.status == "queued" {
            enqueue(core, &t.id);
        }
    }
}

// ---------- 易用性：全量导出 / 模型删除 ----------

/// 可删除的模型 id（与 models::REGISTRY 同步；白名单校验，天然拒绝路径穿越）。
pub const DELETABLE_MODELS: &[&str] = &["small", "medium", "large-v3", "large-v3-turbo"];

/// 读取任务当前 segments.json（编辑/导出兜底用；空结果视为无产物）。
pub fn read_segments(core: &Core, task_id: &str) -> Result<Vec<Value>, String> {
    if core.manager.get(task_id).is_none() {
        return Err("任务不存在".into());
    }
    let seg_file = core.paths.tasks().join(task_id).join("segments.json");
    let text = std::fs::read_to_string(&seg_file).map_err(|e| format!("读取识别结果失败: {}", e))?;
    let segments: Vec<Value> =
        serde_json::from_str(&text).map_err(|e| format!("解析识别结果失败: {}", e))?;
    if segments.is_empty() {
        return Err("字幕结果为空，无可导出内容".into());
    }
    Ok(segments)
}

/// 用当前 segments.json 的全部段落重新 write_all 到指定目录；返回写出的产物文件数。
pub fn export_all_to(core: &Core, task_id: &str, dir: &Path) -> Result<u64, String> {
    let segments = read_segments(core, task_id)?;
    std::fs::create_dir_all(dir).map_err(|e| format!("创建导出目录失败: {}", e))?;
    let out_cfg = core.cfg.get().get("output").cloned().unwrap_or(json!({}));
    let outputs = crate::subtitles::write_all(dir, &segments, &out_cfg)
        .map_err(|e| format!("导出字幕失败：{e}"))?;
    Ok(outputs.len() as u64)
}

/// 删除已下载的 whisper 模型目录（只接受已知模型 id，拒绝路径穿越）。
pub fn delete_model(core: &Core, name: &str) -> Result<(), String> {
    let name = name.trim();
    if !DELETABLE_MODELS.contains(&name) {
        return Err(format!(
            "未知模型：{}（仅允许 small / medium / large-v3 / large-v3-turbo）",
            name
        ));
    }
    let dir = core.paths.model_dir(name);
    if dir.exists() {
        std::fs::remove_dir_all(&dir).map_err(|e| format!("删除模型目录失败：{}", e))?;
    }
    Ok(())
}

#[allow(dead_code)]
fn _path_hint(p: &Path) -> String {
    p.to_string_lossy().to_string()
}
