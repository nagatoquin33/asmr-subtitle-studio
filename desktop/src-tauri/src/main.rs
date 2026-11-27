//! ASMR 字幕工作室 桌面版入口：Tauri v2 壳（无本地 HTTP 服务、无端口监听）。
//!
//! - 前端内嵌于 exe（tauri context 编译期打包），WebView2 渲染
//! - 后端逻辑在 asmr-core；IPC 走 Tauri 命令与事件，彻底摆脱 TCP 端口
//! - 首启自动解包内嵌工具（ffmpeg/ffprobe/yt-dlp）并创建 data/output 目录

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod commands;
mod stream;

use std::sync::Arc;

use asmr_core::pipeline::Core;
use tauri::{Emitter, Manager};

fn main() {
    // panic 落盘：release 下 panic=abort 直接闪退无任何线索，
    // hook 在 abort 前执行，把崩溃信息写进 <exe目录>/data/panic.log
    {
        let log_path = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.join("data").join("panic.log")))
            .unwrap_or_else(|| std::path::PathBuf::from("panic.log"));
        std::panic::set_hook(Box::new(move |info| {
            let msg = format!(
                "[{}] 线程 {:?} panic: {}\n  位置: {}\n\n",
                chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
                std::thread::current().name(),
                info,
                info.location()
                    .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
                    .unwrap_or_default(),
            );
            eprintln!("{}", msg);
            let _ = std::fs::create_dir_all(log_path.parent().unwrap());
            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&log_path) {
                let _ = f.write_all(msg.as_bytes());
            }
        }));
    }

    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            // 二次启动：聚焦已有窗口
            if let Some(win) = app.get_webview_window("main") {
                let _ = win.show();
                let _ = win.set_focus();
            }
        }))
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            // build_core 内部会 spawn worker——放进 Tauri 的 tokio 运行时上下文执行
            let core: Arc<Core> = tauri::async_runtime::block_on(async {
                asmr_core::build_core()
            })
            .map_err(|e| {
                eprintln!("[启动失败] {:#}", e);
                e
            })?;
            app.manage(core.clone());

            // 事件桥：core 广播 → 前端 listen('task-events')
            let app_handle = app.handle().clone();
            let mut rx = core.manager.subscribe();
            tauri::async_runtime::spawn(async move {
                loop {
                    match rx.recv().await {
                        Ok(evt) => {
                            let _ = app_handle.emit("task-events", evt);
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            eprintln!("[事件桥] 落后 {} 条事件（UI 刷新会自行补齐）", n);
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            });
            Ok(())
        })
        .register_asynchronous_uri_scheme_protocol("asmrmedia", move |ctx, request, responder| {
            let state = ctx
                .app_handle()
                .try_state::<Arc<Core>>()
                .expect("core state");
            stream::handle(state.inner().clone(), request, responder);
        })
        .invoke_handler(tauri::generate_handler![
            commands::health,
            commands::get_config,
            commands::save_config,
            commands::test_translate,
            commands::list_models,
            commands::download_model,
            commands::create_local_tasks,
            commands::create_url_tasks,
            commands::list_tasks,
            commands::get_task,
            commands::delete_task,
            commands::cancel_task,
            commands::retry_task,
            commands::patch_segment,
            commands::retranslate_segment,
            commands::patch_segment_fields,
            commands::shift_segments,
            commands::merge_segments,
            commands::split_segment,
            commands::retranslate_batch,
            commands::open_folder,
            commands::export_output,
            commands::export_all_outputs,
            commands::media_urls,
            commands::export_burned_video,
            commands::cancel_burn,
            commands::delete_model,
            commands::app_paths,
            commands::reveal_path,
        ])
        .run(tauri::generate_context!())
        .expect("tauri 运行失败");
}
