//! IPC 命令层：参数校验 + 委托 asmr-core 服务（对应 Python 版 REST 路由）。

use asmr_core::pipeline::Core;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tauri::State;

type CoreState<'a> = State<'a, Arc<Core>>;

fn err(e: impl std::fmt::Display) -> String {
    format!("{:#}", e)
}

// ---------- 成品视频渲染取消注册表（task_id → 取消标志） ----------

static BURN_CANCELS: std::sync::OnceLock<std::sync::Mutex<HashMap<String, Arc<AtomicBool>>>> =
    std::sync::OnceLock::new();

fn burn_registry() -> &'static std::sync::Mutex<HashMap<String, Arc<AtomicBool>>> {
    BURN_CANCELS.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

fn burn_register(task_id: &str, flag: Arc<AtomicBool>) {
    if let Ok(mut reg) = burn_registry().lock() {
        reg.insert(task_id.to_string(), flag);
    }
}

fn burn_flag(task_id: &str) -> Option<Arc<AtomicBool>> {
    burn_registry()
        .lock()
        .ok()
        .and_then(|reg| reg.get(task_id).cloned())
}

fn burn_unregister(task_id: &str) {
    if let Ok(mut reg) = burn_registry().lock() {
        reg.remove(task_id);
    }
}

// ---------- 健康 ----------

#[tauri::command]
pub fn health(core: CoreState) -> Value {
    let ffmpeg = asmr_core::ffmpeg::Ffmpeg::resolve(&core.paths).ok();
    let (gpu_name, gpu_available) = match asmr_core::ffmpeg::gpu_info() {
        Some(name) => (Some(name), true),
        None => (None, false),
    };
    json!({
        "ok": true,
        "app": "asmr-studio",
        "version": asmr_core::config::APP_VERSION,
        "ffmpeg": ffmpeg.is_some(),
        "ffmpeg_version": ffmpeg.as_ref().and_then(|f| f.version()),
        "ffmpeg_path": ffmpeg.as_ref().map(|f| f.path().to_string_lossy().to_string()),
        "gpu": {"available": gpu_available, "name": gpu_name},
    })
}

// ---------- 配置 ----------

#[tauri::command]
pub fn get_config(core: CoreState) -> Value {
    core.cfg.public()
}

#[tauri::command]
pub fn save_config(core: CoreState, patch: Value) -> Result<Value, String> {
    Ok(core.cfg.update(&patch))
}

#[tauri::command]
pub async fn test_translate(
    core: CoreState<'_>,
    base_url: String,
    api_key: String,
    model: String,
) -> Result<Value, String> {
    let mut tcfg = asmr_core::translate::TranslateConfig::from_cfg(
        &core.cfg.get().get("translate").cloned().unwrap_or(json!({})),
    );
    if !base_url.trim().is_empty() {
        tcfg.base_url = base_url.trim().to_string();
    }
    if !model.trim().is_empty() {
        tcfg.model = model.trim().to_string();
    }
    if !api_key.trim().is_empty() && api_key != "***" {
        tcfg.api_key = api_key;
    }
    let llm = asmr_core::llm::LlmConfig {
        base_url: tcfg.base_url.clone(),
        api_key: tcfg.api_key.clone(),
        model: tcfg.model.clone(),
        max_retries: tcfg.max_retries,
    };
    Ok(asmr_core::llm::test_connection(&llm, tcfg.mock).await)
}

// ---------- 模型 ----------

#[tauri::command]
pub fn list_models(core: CoreState) -> Value {
    json!({ "models": core.models.all_status() })
}

#[tauri::command]
pub async fn download_model(core: CoreState<'_>, name: String) -> Result<Value, String> {
    // async 命令跑在 tokio runtime 上（同步命令线程无 reactor，历史 panic 闪退根因）
    core.models.start_download(&name).map_err(err)?;
    Ok(json!({"ok": true, "model": core.models.status(&name)}))
}

/// 删除已下载模型（仅白名单 id；拒绝路径穿越）。返回 {"ok":true}。
#[tauri::command]
pub fn delete_model(core: CoreState<'_>, name: String) -> Result<Value, String> {
    asmr_core::services::delete_model(&core, &name)?;
    Ok(json!({"ok": true}))
}

/// 应用目录绝对路径（设置页「打开数据目录」用；交给 reveal_path 打开）。
#[tauri::command]
pub fn app_paths(core: CoreState<'_>) -> Value {
    let p = &core.paths;
    json!({
        "root": p.root.to_string_lossy().to_string(),
        "data": p.data().to_string_lossy().to_string(),
        "output": p.output().to_string_lossy().to_string(),
        "models": p.models().to_string_lossy().to_string(),
        "tasks": p.tasks().to_string_lossy().to_string(),
        "tools": p.root.join("tools").to_string_lossy().to_string(),
    })
}

// ---------- 任务 ----------

#[tauri::command]
pub fn create_local_tasks(
    core: CoreState,
    paths: Vec<String>,
    source_lang: String,
    target_lang: String,
) -> Result<Value, String> {
    let (tasks, missing, skipped) =
        asmr_core::services::create_tasks_from_paths(&core, &paths, &source_lang, &target_lang);
    Ok(json!({"tasks": tasks, "missing": missing, "skipped": skipped}))
}

#[tauri::command]
pub fn create_url_tasks(
    core: CoreState,
    urls: Vec<String>,
    source_lang: String,
    target_lang: String,
) -> Result<Value, String> {
    let (tasks, invalid) =
        asmr_core::services::create_tasks_from_urls(&core, &urls, &source_lang, &target_lang)
            .map_err(err)?;
    Ok(json!({"tasks": tasks, "invalid": invalid}))
}

/// 任务列表（倒序）：offset/limit 分页，total 为全部任务数（不受分页影响）。
#[tauri::command]
pub fn list_tasks(core: CoreState, limit: Option<u64>, offset: Option<u64>) -> Value {
    let limit = limit.unwrap_or(100).min(500) as usize;
    let offset = offset.unwrap_or(0) as usize;
    let all = core.manager.list(usize::MAX);
    let total = all.len();
    let tasks: Vec<Value> = all
        .into_iter()
        .skip(offset)
        .take(limit)
        .map(|t| serde_json::to_value(&t).unwrap_or(Value::Null))
        .collect();
    json!({ "tasks": tasks, "total": total })
}

#[tauri::command]
pub fn get_task(core: CoreState, task_id: String, segments: Option<bool>) -> Result<Value, String> {
    asmr_core::services::get_task_detail(&core, &task_id, segments.unwrap_or(false))
}

#[tauri::command]
pub fn delete_task(core: CoreState, task_id: String) -> Result<Value, String> {
    asmr_core::services::delete_task(&core, &task_id).map_err(err)?;
    Ok(json!({"ok": true}))
}

#[tauri::command]
pub fn cancel_task(core: CoreState, task_id: String) -> Result<Value, String> {
    Ok(json!({"ok": true, "task": asmr_core::services::cancel_task(&core, &task_id)?}))
}

#[tauri::command]
pub fn retry_task(core: CoreState, task_id: String) -> Result<Value, String> {
    Ok(json!({"ok": true, "task": asmr_core::services::retry_task(&core, &task_id)?}))
}

#[tauri::command]
pub fn patch_segment(core: CoreState, task_id: String, index: u64, translated: String) -> Result<Value, String> {
    let seg = asmr_core::pipeline::update_segment_text(&core, &task_id, index as usize, &translated)?;
    Ok(json!({"ok": true, "segment": seg}))
}

#[tauri::command]
pub async fn retranslate_segment(core: CoreState<'_>, task_id: String, index: u64) -> Result<Value, String> {
    let core = Arc::clone(&core);
    let seg = asmr_core::pipeline::retranslate_segment(&core, &task_id, index as usize).await?;
    Ok(json!({"ok": true, "segment": seg}))
}

// ---------- 段落编辑：字段 / 时间轴 / 合并 / 拆分 / 批量重翻 ----------

/// 段落字段编辑：仅更新传入字段（null/缺省 = 不改），返回全量 segments。
#[tauri::command]
pub fn patch_segment_fields(
    core: CoreState,
    task_id: String,
    index: u64,
    text: Option<String>,
    translated: Option<String>,
    start: Option<f64>,
    end: Option<f64>,
) -> Result<Value, String> {
    let segments = asmr_core::pipeline::patch_segment_fields(
        &core,
        &task_id,
        index,
        text.as_deref(),
        translated.as_deref(),
        start,
        end,
    )?;
    Ok(json!({"ok": true, "segments": segments}))
}

/// 时间轴整体平移（秒）：返回全量 segments。
#[tauri::command]
pub fn shift_segments(core: CoreState, task_id: String, offset: f64) -> Result<Value, String> {
    let segments = asmr_core::pipeline::shift_segments(&core, &task_id, offset)?;
    Ok(json!({"ok": true, "segments": segments}))
}

/// 合并 index 与 index+1：返回全量 segments。
#[tauri::command]
pub fn merge_segments(core: CoreState, task_id: String, index: u64) -> Result<Value, String> {
    let segments = asmr_core::pipeline::merge_segments(&core, &task_id, index)?;
    Ok(json!({"ok": true, "segments": segments}))
}

/// 在绝对秒 at 处拆分 index 段：返回全量 segments。
#[tauri::command]
pub fn split_segment(
    core: CoreState,
    task_id: String,
    index: u64,
    at: f64,
) -> Result<Value, String> {
    let segments = asmr_core::pipeline::split_segment(&core, &task_id, index, at)?;
    Ok(json!({"ok": true, "segments": segments}))
}

/// 批量重翻：并发 ≤ clamp(translate.concurrency,1,6)；返回全量 segments 与 failed。
#[tauri::command]
pub async fn retranslate_batch(
    core: CoreState<'_>,
    task_id: String,
    indices: Vec<u64>,
) -> Result<Value, String> {
    let core = Arc::clone(&core);
    let (segments, failed) =
        asmr_core::pipeline::retranslate_batch(&core, &task_id, &indices).await?;
    Ok(json!({"ok": true, "segments": segments, "failed": failed}))
}

#[tauri::command]
pub fn open_folder(core: CoreState, task_id: String) -> Result<Value, String> {
    let dir = asmr_core::services::open_task_folder(&core, &task_id)?;
    Ok(json!({"ok": true, "dir": dir}))
}

/// 导出产物：弹保存对话框并写入；用户取消返回 path=null。
#[tauri::command]
pub async fn export_output(
    app: tauri::AppHandle,
    core: CoreState<'_>,
    task_id: String,
    kind: String,
    suggested_name: String,
) -> Result<Value, String> {
    use tauri_plugin_dialog::DialogExt;
    let (_, bytes) = asmr_core::services::read_output_file(&core, &task_id, &kind)?;
    let filter_name = match kind.as_str() {
        "json" => "JSON",
        "bilingual_ass" => "ASS 字幕",
        "bilingual_vtt" => "VTT 字幕",
        _ => "SRT 字幕",
    };
    let ext = suggested_name.rsplit('.').next().unwrap_or("srt").to_string();
    let name = suggested_name;
    let chosen = app
        .dialog()
        .file()
        .add_filter(filter_name, &[ext.as_str()])
        .set_file_name(&name)
        .blocking_save_file();
    let Some(dest) = chosen else {
        return Ok(json!({"ok": true, "path": Value::Null}));
    };
    let dest_path = dest
        .as_path()
        .ok_or_else(|| "保存路径无效".to_string())?
        .to_path_buf();
    let dest_str = dest_path.to_string_lossy().to_string();
    tokio::task::spawn_blocking(move || std::fs::write(&dest_path, &bytes))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("写入失败: {}", e))?;
    Ok(json!({"ok": true, "path": dest_str}))
}

/// 一键导出全部产物：弹文件夹选择框，用当前 segments.json 重新写出 SRT/VTT/ASS/JSON。
/// 成功 {"ok":true,"dir":"...","count":写入文件数}；用户取消 {"ok":true,"path":null}。
#[tauri::command]
pub async fn export_all_outputs(
    app: tauri::AppHandle,
    core: CoreState<'_>,
    task_id: String,
) -> Result<Value, String> {
    use tauri_plugin_dialog::DialogExt;

    // 先确认存在字幕结果：无结果直接报错，不弹框
    asmr_core::services::read_segments(&core, &task_id)?;
    let chosen = app
        .dialog()
        .file()
        .set_title("选择导出字幕的文件夹")
        .blocking_pick_folder();
    let Some(dir) = chosen else {
        return Ok(json!({"ok": true, "path": Value::Null}));
    };
    let dest = dir
        .as_path()
        .ok_or_else(|| "导出目录无效".to_string())?
        .to_path_buf();
    let dest_str = dest.to_string_lossy().to_string();
    let core = Arc::clone(&core);
    let count = tokio::task::spawn_blocking(move || {
        asmr_core::services::export_all_to(&core, &task_id, &dest)
    })
    .await
    .map_err(|e| format!("导出线程异常：{}", e))??;
    Ok(json!({"ok": true, "dir": dest_str, "count": count}))
}

/// 供前端获取流媒体 URL（Windows: http://asmrmedia.localhost/...）。
#[tauri::command]
pub fn media_urls(task_id: String) -> Value {
    json!({
        "media": format!("http://asmrmedia.localhost/{}/media", task_id),
        "vtt": format!("http://asmrmedia.localhost/{}/vtt", task_id),
    })
}

/// 成品视频导出：硬字幕烧录（NVENC→libx264）或软字幕 mkv 封装。
/// 先弹保存对话框；确认后后台渲染，进度经 burn-events 事件推送前端。
#[tauri::command]
pub async fn export_burned_video(
    app: tauri::AppHandle,
    core: CoreState<'_>,
    task_id: String,
    mode: String,
) -> Result<Value, String> {
    use tauri::Emitter;
    use tauri_plugin_dialog::DialogExt;

    let core = Arc::clone(&core);
    let task = core.manager.get(&task_id).ok_or("任务不存在")?;
    if !task.has_result {
        return Err("任务尚未完成，无字幕可合成".into());
    }
    let ass = asmr_core::services::output_path(&core, &task_id, "bilingual_ass")
        .map_err(|e| format!("双语 ASS 字幕缺失（{}）", e))?;
    let media = asmr_core::services::media_path(&core, &task_id)
        .map_err(|e| format!("视频文件缺失（{}）", e))?;
    let duration = task.media.duration;
    let soft = mode == "mkv";

    let suggested = asmr_core::render::burned_name(&media, soft);
    let (filter_name, ext): (&str, &str) = if soft {
        ("MKV 视频", "mkv")
    } else {
        ("MP4 视频", "mp4")
    };
    let chosen = app
        .dialog()
        .file()
        .add_filter(filter_name, &[ext])
        .set_file_name(&suggested)
        .blocking_save_file();
    let Some(dest) = chosen else {
        return Ok(json!({"ok": true, "started": false, "path": Value::Null}));
    };
    let dest_path = dest
        .as_path()
        .ok_or_else(|| "保存路径无效".to_string())?
        .to_path_buf();
    let dest_str = dest_path.to_string_lossy().to_string();

    let app2 = app.clone();
    let tid = task_id.clone();
    let cancel = Arc::new(AtomicBool::new(false));
    // 注册取消标志：cancel_burn(task_id) 可随时中断渲染
    burn_register(&tid, Arc::clone(&cancel));
    let cancel2 = Arc::clone(&cancel);
    tauri::async_runtime::spawn(async move {
        let result = async {
            let ff = asmr_core::ffmpeg::Ffmpeg::resolve(&core.paths)?;
            let progress = {
                let app = app2.clone();
                let tid = tid.clone();
                Arc::new(move |f: f64| {
                    let _ = app.emit(
                        "burn-events",
                        serde_json::json!({"taskId": tid, "progress": f, "phase": "running"}),
                    );
                }) as std::sync::Arc<dyn Fn(f64) + Send + Sync>
            };
            asmr_core::render::render_video(
                &ff,
                &media,
                &ass,
                &dest_path,
                duration,
                soft,
                progress,
                cancel2,
            )
            .await
        }
        .await;
        burn_unregister(&tid);
        match result {
            Ok(()) => {
                let _ = app2.emit(
                    "burn-events",
                    serde_json::json!({"taskId": tid, "progress": 1.0, "phase": "done",
                                       "path": dest_path.to_string_lossy().to_string()}),
                );
            }
            Err(e) => {
                let msg = format!("{:#}", e);
                if cancel.load(Ordering::Relaxed) || msg.contains("取消") {
                    // 用户取消：渲染已中断（残留文件由 render 自行清理）
                    let _ = app2.emit(
                        "burn-events",
                        serde_json::json!({"taskId": tid, "phase": "canceled"}),
                    );
                } else {
                    let _ = app2.emit(
                        "burn-events",
                        serde_json::json!({"taskId": tid, "phase": "error", "message": msg}),
                    );
                }
            }
        }
    });
    Ok(json!({
        "ok": true,
        "started": true,
        "path": dest_str,
    }))
}

/// 取消正在进行的成品视频渲染（无进行中的任务时也返回 ok，幂等）。
#[tauri::command]
pub fn cancel_burn(task_id: String) -> Value {
    if let Some(flag) = burn_flag(&task_id) {
        flag.store(true, Ordering::Relaxed);
    }
    json!({"ok": true})
}

/// 打开任意目录（资源管理器）。
#[tauri::command]
pub fn reveal_path(path: String) -> Result<Value, String> {
    let p = std::path::PathBuf::from(&path);
    if !p.exists() {
        return Err("路径不存在".into());
    }
    #[cfg(windows)]
    {
        std::process::Command::new("explorer")
            .arg(&p)
            .spawn()
            .map_err(|e| format!("无法打开: {}", e))?;
    }
    #[cfg(not(windows))]
    {
        std::process::Command::new("xdg-open")
            .arg(&p)
            .spawn()
            .map_err(|e| format!("无法打开: {}", e))?;
    }
    Ok(json!({"ok": true}))
}
