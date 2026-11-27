//! 端到端测试（无 UI）：完整管线 媒体 → ASR → 翻译 → 审校 → 字幕产物。
//!
//! 用法：cargo run --release -p asmr-core [--features whisper-vulkan] --example e2e
//! 环境变量：
//!   ASMR_E2E_HOME   测试根目录（默认 <repo>/desktop/target/e2e-home）
//!   ASMR_E2E_SRC    本地媒体文件路径（默认 data/samples/sample_zh.wav）
//!   ASMR_E2E_URL    视频页链接（设置后走 yt-dlp 下载路径，优先于 SRC）
//!   ASMR_E2E_SL/TL  源/目标语言（默认 auto/zh）
//!   ASMR_E2E_MODEL  ASR 模型名（默认 small）
//!   ASMR_E2E_VAD    vad 模式（默认 off）
//!   ASMR_E2E_REAL   =1 用真实翻译 API（读测试根目录 .env 的 LLM_API_KEY）
//!   ASMR_E2E_PRINT  打印前 N 段（默认 8）

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use asmr_core::pipeline::Core;

fn envs(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root")
}

fn main() -> anyhow::Result<()> {
    let root = std::env::var("ASMR_E2E_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| repo_root().join("desktop/target/e2e-home"));
    std::fs::create_dir_all(&root)?;
    // 工具目录：junction 指向仓库 tools（避免重复占用 300MB）
    let tools_link = root.join("tools");
    if !tools_link.exists() {
        let _ = std::process::Command::new("powershell")
            .args([
                "-NoProfile",
                "-Command",
                &format!(
                    "New-Item -ItemType Junction -Path '{}' -Value '{}' | Out-Null",
                    tools_link.display(),
                    repo_root().join("tools").display()
                ),
            ])
            .status();
    }
    std::env::set_var("ASMR_HOME", &root);

    let url = std::env::var("ASMR_E2E_URL").ok();
    let src = std::env::var("ASMR_E2E_SRC")
        .map(PathBuf::from)
        .unwrap_or_else(|_| repo_root().join("data/samples/sample_zh.wav"));
    let source_lang = envs("ASMR_E2E_SL", "auto");
    let target_lang = envs("ASMR_E2E_TL", "zh");
    let model = envs("ASMR_E2E_MODEL", "small");
    let vad = envs("ASMR_E2E_VAD", "off");
    let print_n: usize = envs("ASMR_E2E_PRINT", "8").parse().unwrap_or(8);

    let real = std::env::var("ASMR_E2E_REAL").ok().as_deref() == Some("1");
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async move {
        let core: Arc<Core> = asmr_core::build_core()?;
        core.cfg.update(&serde_json::json!({
            "translate": {"mock": !real, "proofread": true},
            "asr": {"model": model, "device": "auto", "vad": vad}
        }));

        let (tasks, bad) = if let Some(url) = &url {
            let (tasks, invalid) = asmr_core::services::create_tasks_from_urls(
                &core,
                &[url.clone()],
                &source_lang,
                &target_lang,
            )
            .map_err(|e| anyhow::anyhow!(e))?;
            (tasks, invalid)
        } else {
            let (t, m, _s) = asmr_core::services::create_tasks_from_paths(
                &core,
                &[src.to_string_lossy().to_string()],
                &source_lang,
                &target_lang,
            );
            (t, m)
        };
        if !bad.is_empty() {
            println!("[e2e] 无效输入: {:?}", bad);
        }
        let task_id = tasks
            .first()
            .and_then(|t| t["id"].as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow::anyhow!("任务创建失败"))?;
        println!("[e2e] 任务已创建: {}", task_id);

        // 事件跟随
        let mut rx = core.manager.subscribe();
        let monitor_core = Arc::clone(&core);
        let tid = task_id.clone();
        let done_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag2 = Arc::clone(&done_flag);
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(evt) => {
                        let t = &evt["task"];
                        if t["id"].as_str() == Some(tid.as_str()) {
                            let status = t["status"].as_str().unwrap_or("");
                            let msg = t["message"].as_str().unwrap_or("");
                            let prog = t["progress"].as_f64().unwrap_or(0.0);
                            println!("[e2e] {:>12} {:>5.0}% {}", status, prog * 100.0, msg);
                            if matches!(status, "done" | "error" | "canceled") {
                                if status == "error" {
                                    println!("[e2e] 失败详情: {}", t["error"].as_str().unwrap_or(""));
                                }
                                flag2.store(true, std::sync::atomic::Ordering::Relaxed);
                                let _ = &monitor_core;
                                break;
                            }
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        for _ in 0..43200 { // 最多等 6 小时
            if done_flag.load(std::sync::atomic::Ordering::Relaxed) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }

        let task = core.manager.get(&task_id).expect("task");
        println!("\n[e2e] 最终状态: {} | {}", task.status, task.message);
        let detail =
            asmr_core::services::get_task_detail(&core, &task_id, true).map_err(|e| anyhow::anyhow!(e))?;
        let segs = detail["segments"].as_array().cloned().unwrap_or_default();
        println!("[e2e] 段数: {}", segs.len());
        for s in segs.iter().take(print_n) {
            println!(
                "  [{:>6.1} → {:>6.1}] {}  ⇒  {}",
                s["start"].as_f64().unwrap_or(0.0),
                s["end"].as_f64().unwrap_or(0.0),
                s["text"].as_str().unwrap_or(""),
                s["translated"].as_str().unwrap_or("")
            );
        }
        println!("[e2e] 产物目录: {:?}", task.output_dir);
        println!("[e2e] ASR 元数据: {}", task.asr);

        // 校验产物
        let Some(out_dir_str) = task.output_dir else {
            anyhow::bail!("任务失败，无产物目录");
        };
        let out_dir = std::path::PathBuf::from(out_dir_str);
        for f in ["orig.srt", "trans.srt", "bilingual.srt", "bilingual.ass", "bilingual.vtt"] {
            let p = out_dir.join(f);
            let ok = p.is_file() && std::fs::metadata(&p).map(|m| m.len() > 0).unwrap_or(false);
            println!("[e2e] 产物 {}: {}", f, if ok { "OK" } else { "缺失!" });
        }
        // 断点续跑：重跑一次应秒完成（跳过全部阶段）
        let t0 = std::time::Instant::now();
        asmr_core::services::retry_task(&core, &task_id).map_err(|e| anyhow::anyhow!(e))?;
        loop {
            let t = core.manager.get(&task_id).unwrap();
            if matches!(t.status.as_str(), "done" | "error") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        println!("[e2e] 重试耗时: {:.1}s（应显著短于首次）", t0.elapsed().as_secs_f64());

        // 成品视频渲染（ASMR_E2E_BURN=1 开启）：硬字幕烧录 + 软字幕 mkv
        if std::env::var("ASMR_E2E_BURN").ok().as_deref() == Some("1") {
            let ass = asmr_core::services::output_path(&core, &task_id, "bilingual_ass")
                .map_err(|e| anyhow::anyhow!(e))?;
            let media = asmr_core::services::media_path(&core, &task_id)
                .map_err(|e| anyhow::anyhow!(e))?;
            let ff = asmr_core::ffmpeg::Ffmpeg::resolve(core.paths()).map_err(|e| anyhow::anyhow!(e))?;
            let duration = task.media.duration;
            let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));

            let t0 = std::time::Instant::now();
            let dest_hard = out_dir.join("burn-test-hard.mp4");
            asmr_core::render::render_video(
                &ff,
                &media,
                &ass,
                &dest_hard,
                duration,
                false,
                Arc::new(|f: f64| print!("\r[e2e] 硬字幕渲染 {:.0}%", f * 100.0)),
                Arc::clone(&cancel),
            )
            .await
            .map_err(|e| anyhow::anyhow!("硬字幕渲染失败: {:#}", e))?;
            let hard_size = std::fs::metadata(&dest_hard).map(|m| m.len()).unwrap_or(0);
            println!(
                "\n[e2e] 硬字幕 mp4: {:.1}s, {}MB（{}）",
                t0.elapsed().as_secs_f64(),
                hard_size / 1048576,
                dest_hard.display()
            );

            let t0 = std::time::Instant::now();
            let dest_soft = out_dir.join("burn-test-soft.mkv");
            asmr_core::render::render_video(
                &ff,
                &media,
                &ass,
                &dest_soft,
                duration,
                true,
                Arc::new(|_: f64| {}),
                cancel,
            )
            .await
            .map_err(|e| anyhow::anyhow!("软字幕封装失败: {:#}", e))?;
            let soft_size = std::fs::metadata(&dest_soft).map(|m| m.len()).unwrap_or(0);
            println!("[e2e] 软字幕 mkv: {:.1}s, {}MB", t0.elapsed().as_secs_f64(), soft_size / 1048576);
        }
        Ok(())
    })
}
