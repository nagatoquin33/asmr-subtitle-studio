//! asmr-core：ASMR 字幕工作室核心（与 UI 框架解耦，可独立测试）。
//!
//! 模块划分与 Python 版一一对应：
//! paths（目录/首启解包）/ config（配置+.env）/ tasks（任务与事件）
//! / subtitles（字幕输出）/ ffmpeg / downloader（yt-dlp）/ models（ggml 模型下载）
//! / asr（whisper.cpp）/ llm + translate（翻译协议）/ pipeline（编排）/ services（服务层）

pub mod asr;
pub mod config;
pub mod downloader;
pub mod ffmpeg;
pub mod llm;
pub mod models;
pub mod paths;
pub mod pipeline;
pub mod render;
pub mod services;
pub mod subtitles;
pub mod tasks;
pub mod translate;

use std::sync::Arc;

pub use pipeline::Core;

/// 构建核心并在当前 tokio 运行时启动工作队列。
pub fn build_core() -> anyhow::Result<Arc<Core>> {
    let paths = paths::Paths::detect()?;
    let extracted = paths::bootstrap(&paths)?;
    if !extracted.is_empty() {
        println!("[首启] 已解包工具: {}", extracted.join(", "));
    }
    let manager = tasks::TaskManager::new(paths.clone());
    let cfg = config::ConfigStore::new(paths.clone());
    let models = models::ModelStore::new(paths.clone(), tokio::runtime::Handle::current());

    let workers = cfg
        .get()
        .pointer("/pipeline/workers")
        .and_then(|v| v.as_u64())
        .unwrap_or(1)
        .clamp(1, 8) as usize;

    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let core = Arc::new(Core {
        paths,
        cfg,
        manager,
        models,
        queue_tx: tx,
    });

    // worker 池：共享同一接收端
    let rx = Arc::new(tokio::sync::Mutex::new(rx));
    for _ in 0..workers {
        let core = Arc::clone(&core);
        let rx = Arc::clone(&rx);
        tokio::spawn(async move {
            loop {
                let task_id = rx.lock().await.recv().await;
                match task_id {
                    Some(id) => pipeline::run_task(&core, &id).await,
                    None => break,
                }
            }
        });
    }

    // 恢复上次中断的排队任务
    services::requeue_pending(&core);
    Ok(core)
}
