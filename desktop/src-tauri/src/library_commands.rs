//! 作品库操作与导入事件。
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use asmr_core::{library, Core};
use serde_json::json;
use tauri::{AppHandle, Emitter, State};

type CoreState<'a> = State<'a, Arc<Core>>;

fn error(err: impl std::fmt::Display) -> String {
    format!("{err:#}")
}

fn make_import_progress(app: AppHandle) -> library::ImportProgress {
    let last_emit = Arc::new(AtomicU64::new(0));
    Arc::new(move |done_files, total_files, done_bytes, total_bytes, name| {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let prev = last_emit.load(Ordering::Relaxed);
        if done_files < total_files && now.saturating_sub(prev) < 120 {
            return;
        }
        last_emit.store(now, Ordering::Relaxed);
        let _ = app.emit(
            "library-import-progress",
            json!({
                "done_files": done_files,
                "total_files": total_files,
                "done_bytes": done_bytes,
                "total_bytes": total_bytes,
                "file": name,
            }),
        );
    })
}

#[tauri::command]
pub fn library_list(core: CoreState<'_>) -> Result<Vec<library::WorkSummary>, String> {
    library::list(&core.paths).map_err(error)
}

#[tauri::command]
pub fn library_detail(core: CoreState<'_>, work_id: String) -> Result<library::WorkDetail, String> {
    library::detail(&core, &work_id).map_err(error)
}

#[tauri::command]
pub fn library_check_duplicate(
    core: CoreState<'_>,
    paths: Vec<String>,
) -> Result<Vec<serde_json::Value>, String> {
    let mut matches = Vec::new();
    for path in paths {
        if let Some(existing) = library::find_duplicate(&core.paths, Path::new(&path)).map_err(error)? {
            matches.push(json!({
                "path": path,
                "existing": existing,
            }));
        }
    }
    Ok(matches)
}

#[tauri::command]
pub async fn library_import_folder(
    app: AppHandle,
    core: CoreState<'_>,
    path: String,
) -> Result<library::Work, String> {
    let paths = core.paths.clone();
    let progress = make_import_progress(app);
    tauri::async_runtime::spawn_blocking(move || {
        library::import_folder_with_progress(&paths, Path::new(&path), Some(progress))
    })
    .await
    .map_err(|err| err.to_string())?
    .map_err(error)
}

#[tauri::command]
pub async fn library_import_paths(
    app: AppHandle,
    core: CoreState<'_>,
    paths: Vec<String>,
) -> Result<serde_json::Value, String> {
    let app_paths = core.paths.clone();
    let progress = make_import_progress(app);
    tauri::async_runtime::spawn_blocking(move || {
        let mut imported = Vec::new();
        let mut errors = Vec::new();
        for path in paths {
            let src = Path::new(&path);
            let result = if src.is_dir() {
                library::import_folder_with_progress(&app_paths, src, Some(Arc::clone(&progress)))
            } else {
                library::import_file(&app_paths, src)
            };
            match result {
                Ok(work) => imported.push(work),
                Err(err) => errors.push(error(err)),
            }
        }
        json!({ "works": imported, "errors": errors })
    })
    .await
    .map_err(|err| err.to_string())
}

#[tauri::command]
pub async fn library_import_url(
    app: AppHandle,
    core: CoreState<'_>,
    url: String,
) -> Result<library::Work, String> {
    let progress = Arc::new(move |done, total, speed| {
        let _ = app.emit(
            "library-download",
            json!({ "done": done, "total": total, "speed": speed }),
        );
    });
    library::import_url(&core, &url, progress)
        .await
        .map_err(error)
}

#[tauri::command]
pub fn library_queue(
    core: CoreState<'_>,
    work_id: String,
    tracks: Vec<String>,
    source_lang: String,
    target_lang: String,
) -> Result<Vec<String>, String> {
    library::queue(&core, &work_id, &tracks, &source_lang, &target_lang).map_err(error)
}

#[tauri::command]
pub async fn library_preview(
    core: CoreState<'_>,
    work_id: String,
    track_id: String,
) -> Result<(), String> {
    library::prepare_preview(&core.paths, &work_id, &track_id)
        .await
        .map_err(error)
}

#[tauri::command]
pub fn library_rename(core: CoreState<'_>, work_id: String, title: String) -> Result<(), String> {
    library::rename(&core.paths, &work_id, &title).map_err(error)
}

#[tauri::command]
pub fn library_delete(core: CoreState<'_>, work_id: String) -> Result<(), String> {
    library::delete(&core, &work_id).map_err(error)
}

#[tauri::command]
pub fn library_open(core: CoreState<'_>, work_id: String) -> Result<(), String> {
    let dir = library::work_dir(&core.paths, &work_id).map_err(error)?;
    #[cfg(windows)]
    let exe = "explorer";
    #[cfg(not(windows))]
    let exe = "xdg-open";
    std::process::Command::new(exe)
        .arg(dir)
        .spawn()
        .map_err(|err| format!("无法打开作品目录：{err}"))?;
    Ok(())
}
