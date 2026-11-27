//! 管线编排：下载 → 抽音频 → ASR → 翻译 → 审校 → 写字幕。
//! 断点续跑基于磁盘产物（视频已下载 / audio.wav 已存在 / segments.json 已存在）。

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::config::ConfigStore;
use crate::ffmpeg::{CancelFlag, StageError};
use crate::models::ModelStore;
use crate::tasks::{plan_output_dir, EventKind, TaskManager};
use crate::{asr, downloader, ffmpeg as ffmpeg_mod, subtitles, translate};

pub struct Core {
    pub paths: crate::paths::Paths,
    pub cfg: ConfigStore,
    pub manager: Arc<TaskManager>,
    pub models: Arc<ModelStore>,
    pub queue_tx: tokio::sync::mpsc::UnboundedSender<String>,
}

impl Core {
    pub fn paths(&self) -> &crate::paths::Paths {
        &self.paths
    }
}

// ---------- 纯决策函数 ----------

fn looks_mock(text: &str) -> bool {
    text.trim().starts_with("[译]")
}

fn seg_translated(seg: &Value) -> bool {
    let t = seg
        .get("translated")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    !t.is_empty() && !looks_mock(t)
}

pub fn segments_all_translated(segments: &[Value]) -> bool {
    !segments.is_empty() && segments.iter().all(seg_translated)
}

/// 是否跳过翻译：显式指定 源=目标，或检测置信度 ≥0.9 且同语言。
/// （Rust 版语言概率暂不可得，仅保留「显式指定」路径——宁可多翻一次也不错翻。）
pub fn should_skip_translation(source_lang: &str, target_lang: &str) -> (bool, String) {
    if source_lang != "auto" && !source_lang.is_empty() && source_lang == target_lang {
        return (true, "源语言=目标语言（手动指定），跳过翻译".into());
    }
    (false, String::new())
}

fn load_existing_segments(task_dir: &Path) -> Option<Vec<Value>> {
    let seg_file = task_dir.join("segments.json");
    let text = std::fs::read_to_string(seg_file).ok()?;
    let loaded: Vec<Value> = serde_json::from_str(&text).ok()?;
    if loaded.is_empty() {
        return None;
    }
    if loaded
        .iter()
        .all(|s| s.get("text").and_then(|v| v.as_str()).map(|t| !t.is_empty()).unwrap_or(false))
    {
        Some(loaded)
    } else {
        None
    }
}

fn fmt_clock(sec: f64) -> String {
    let s = sec.max(0.0) as u64;
    format!("{:02}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60)
}

fn mb(n: u64) -> String {
    format!("{:.1}MB", n as f64 / 1048576.0)
}

// ---------- 运行上下文 ----------

struct StageCtx {
    task_id: String,
    task_dir: PathBuf,
    source_lang: String,
    target_lang: String,
    cfg: Value,
    cancel: CancelFlag,
    segments: Option<Vec<Value>>,
    asr_meta: Value,
    need_translate: bool,
    all_translated: bool,
    out_dir: Option<PathBuf>,
    outputs: serde_json::Map<String, Value>,
}

// ---------- 阶段实现 ----------

async fn stage_download(core: &Core, ctx: &mut StageCtx) -> Result<(), StageError> {
    let Some(url) = core
        .manager
        .get(&ctx.task_id)
        .and_then(|t| t.source_url)
    else {
        return Ok(());
    };
    let media_path = core
        .manager
        .get(&ctx.task_id)
        .and_then(|t| t.media.path)
        .filter(|p| Path::new(p).exists());
    if media_path.is_some() {
        core.manager.update(&ctx.task_id, false, EventKind::Progress, |t| {
            t.progress = 0.04;
            t.message = "视频已下载，跳过下载".into();
        });
        return Ok(());
    }

    core.manager.update(&ctx.task_id, true, EventKind::Progress, |t| {
        t.status = "downloading".into();
        t.stage = "downloading".into();
    });

    let dcfg = ctx.cfg.get("download").cloned().unwrap_or(json!({}));
    let height = dcfg.get("height").and_then(|v| v.as_u64()).unwrap_or(720) as u32;
    let proxy = dcfg.get("proxy").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let cookies = dcfg
        .get("cookies_from_browser")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let ff = ffmpeg_mod::Ffmpeg::resolve(core.paths()).ok();
    let dest_dir = ctx.task_dir.clone();
    let task_id = ctx.task_id.clone();
    let manager = Arc::clone(&core.manager);
    let cancel = Arc::clone(&ctx.cancel);

    let progress = {
        let manager = Arc::clone(&manager);
        let task_id = task_id.clone();
        Arc::new(move |done: u64, total: u64, speed: Option<f64>| {
            manager.update(&task_id, false, EventKind::Progress, |t| {
                let speed_s = speed
                    .map(|s| format!("，{:.1}MB/s", s / 1048576.0))
                    .unwrap_or_default();
                t.message = format!(
                    "下载中 {}/{}{}",
                    mb(done),
                    if total > 0 { mb(total) } else { "?".into() },
                    speed_s
                );
                t.progress = if total > 0 {
                    (done as f64 / total as f64) * 0.05
                } else {
                    0.02
                };
            });
        })
    };

    let info = downloader::download_media(
        core.paths(),
        ff.as_ref(),
        &url,
        &dest_dir,
        height,
        &proxy,
        &cookies,
        progress,
        cancel,
    )
    .await?;

    let size = Path::new(&info.path).metadata().map(|m| m.len()).unwrap_or(0);
    let name = Path::new(&info.path)
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    core.manager.update(&ctx.task_id, true, EventKind::Progress, |t| {
        t.name = name.clone();
        t.media.path = Some(info.path.clone());
        t.media.duration = info.duration.or(t.media.duration);
        t.media.size = Some(size);
        t.media.has_video = Some(true);
        t.message = format!("下载完成：{}", mb(size));
    });
    Ok(())
}

async fn stage_extract(core: &Core, ctx: &mut StageCtx) -> Result<(), StageError> {
    core.manager.update(&ctx.task_id, true, EventKind::Progress, |t| {
        t.status = "extracting".into();
        t.stage = "extracting".into();
        t.progress = 0.05;
        t.message = "正在探测媒体信息…".into();
    });
    let media_path = core
        .manager
        .get(&ctx.task_id)
        .and_then(|t| t.media.path)
        .ok_or_else(|| StageError::Other(anyhow::anyhow!("任务缺少媒体文件路径")))?;
    let media_path = PathBuf::from(media_path);
    let ff = ffmpeg_mod::Ffmpeg::resolve(core.paths()).map_err(StageError::Other)?;
    let probe = ff.probe(&media_path).await.map_err(StageError::Other)?;
    let duration = probe.duration;
    {
        let duration = duration;
        let has_video = probe.has_video;
        let size = probe.size;
        core.manager.update(&ctx.task_id, true, EventKind::Progress, |t| {
            t.media.duration = Some(duration);
            t.media.has_video = Some(has_video);
            t.media.size = Some(size);
        });
    }

    let wav = ctx.task_dir.join("audio.wav");
    if wav.exists() {
        core.manager.update(&ctx.task_id, false, EventKind::Progress, |t| {
            t.message = "复用已提取的音频，跳过抽取".into();
        });
        return Ok(());
    }

    let manager = Arc::clone(&core.manager);
    let task_id = ctx.task_id.clone();
    let cancel = Arc::clone(&ctx.cancel);
    ff.extract_audio(
        &media_path,
        &wav,
        duration,
        move |frac| {
            manager.update(&task_id, false, EventKind::Progress, |t| {
                t.progress = 0.05 + frac * 0.03;
                t.message = format!("提取音频 {}%", (frac * 100.0) as u64);
            });
        },
        cancel,
    )
    .await?;
    // 记录 duration 到 ctx 供 ASR 进度使用
    ctx.cfg
        .as_object_mut()
        .unwrap()
        .insert("__duration".into(), json!(duration));
    Ok(())
}

async fn stage_asr(core: &Core, ctx: &mut StageCtx) -> Result<(), StageError> {
    if let Some(segs) = load_existing_segments(&ctx.task_dir) {
        let n = segs.len();
        ctx.segments = Some(segs);
        core.manager.update(&ctx.task_id, false, EventKind::Progress, |t| {
            t.message = format!("已存在识别结果（{} 段），跳过识别", n);
        });
        return Ok(());
    }

    let cfg_asr = ctx.cfg.get("asr").cloned().unwrap_or(json!({}));
    let model_name = cfg_asr
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("small")
        .to_string();
    core.manager.update(&ctx.task_id, true, EventKind::Progress, |t| {
        t.status = "asr".into();
        t.stage = "asr".into();
        t.progress = 0.08;
        t.message = format!("准备模型 {}…", model_name);
    });

    // 模型就绪（缺则下载，带进度文案）
    let model_path = {
        let models = Arc::clone(&core.models);
        let manager = Arc::clone(&core.manager);
        let task_id = ctx.task_id.clone();
        let name = model_name.clone();
        // 模型下载进度反映到任务消息
        let progress_model = {
            let models = Arc::clone(&models);
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(std::time::Duration::from_millis(800)).await;
                    let st = models.status(&name);
                    if st["status"] == "downloading" {
                        let detail = st["detail"].as_str().unwrap_or("").to_string();
                        let pct = st["progress"].as_f64().unwrap_or(0.0);
                        manager.update(&task_id, false, EventKind::Progress, |t| {
                            t.message = format!(
                                "下载模型 {}… {} {}%",
                                name,
                                detail,
                                (pct * 100.0) as u64
                            );
                        });
                    } else if st["status"] == "ready" {
                        break;
                    }
                    if st["status"] == "error" {
                        break;
                    }
                }
            })
        };
        let result = models.ensure_ready(&model_name).await;
        progress_model.abort();
        result.map_err(StageError::Other)?
    };

    // VAD 模型（auto/on 时需要；缺失自动下载，~885KB）
    let vad_mode = cfg_asr
        .get("vad")
        .and_then(|v| v.as_str())
        .unwrap_or("auto")
        .to_string();
    let vad_model_path = if vad_mode != "off" {
        let p = core
            .models
            .ensure_vad()
            .await
            .map_err(StageError::Other)
            .ok();
        p
    } else {
        None
    };

    let wav = ctx.task_dir.join("audio.wav");
    let duration = ctx
        .cfg
        .get("__duration")
        .and_then(|v| v.as_f64())
        .or_else(|| core.manager.get(&ctx.task_id).and_then(|t| t.media.duration))
        .unwrap_or(0.0);
    let device = cfg_asr
        .get("device")
        .and_then(|v| v.as_str())
        .unwrap_or("auto")
        .to_string();
    let source_lang = if ctx.source_lang == "auto" {
        None
    } else {
        Some(ctx.source_lang.clone())
    };
    let manager = Arc::clone(&core.manager);
    let task_id = ctx.task_id.clone();
    let progress = {
        Arc::new(move |frac: f64| {
            manager.update(&task_id, false, EventKind::Progress, |t| {
                t.progress = 0.08 + frac * 0.52;
                t.message = format!(
                    "识别中 {} / {}",
                    fmt_clock(frac * duration),
                    fmt_clock(duration)
                );
            });
        })
    };
    let cancel = Arc::clone(&ctx.cancel);
    let opts = asr::AsrOptions {
        model_path,
        vad_model_path,
        device,
        source_lang,
        vad: vad_mode,
    };
    let outcome = tokio::task::spawn_blocking(move || asr::transcribe(opts, &wav, progress, cancel))
        .await
        .map_err(|e| StageError::Other(anyhow::anyhow!("识别线程异常: {}", e)))?
        .map_err(StageError::Other)?;

    let n = outcome.segments.len();
    ctx.segments = Some(outcome.segments.clone());
    ctx.asr_meta = outcome.meta.clone();
    {
        let meta = outcome.meta.clone();
        let n = n;
        core.manager.update(&ctx.task_id, true, EventKind::Progress, |t| {
            t.asr = meta.clone();
            t.counts.segments = n as u64;
            if let Some(w) = t.asr.get("warning").and_then(|v| v.as_str()) {
                t.message = format!("识别中…（{}）", w);
            }
        });
    }
    // 识别结果立即落盘：翻译失败重试无需重跑识别
    let seg_file = ctx.task_dir.join("segments.json");
    let _ = std::fs::write(&seg_file, serde_json::to_string(&outcome.segments).unwrap_or_default());
    Ok(())
}

async fn stage_translate(core: &Core, ctx: &mut StageCtx) -> Result<(), StageError> {
    let (skip, reason) =
        should_skip_translation(&ctx.source_lang, &ctx.target_lang);
    ctx.need_translate = !skip;
    let segments = ctx.segments.clone().unwrap_or_default();
    ctx.all_translated = segments_all_translated(&segments);

    let tr_cfg_val = ctx.cfg.get("translate").cloned().unwrap_or(json!({}));
    let tcfg = translate::TranslateConfig::from_cfg(&tr_cfg_val);

    if !segments.is_empty() && ctx.need_translate && !ctx.all_translated {
        if !tcfg.mock
            && (tcfg.api_key.trim().is_empty() || tcfg.base_url.trim().is_empty())
        {
            return Err(StageError::Other(anyhow::anyhow!(
                "未配置翻译 API：请到 设置 → 翻译 API 填写 Base URL 与 API Key 并保存，然后在任务上点「重试」即可继续（识别结果已保留，无需重跑）；也可以先开启 Mock 模式体验全流程"
            )));
        }
        core.manager.update(&ctx.task_id, true, EventKind::Progress, |t| {
            t.status = "translating".into();
            t.stage = "translating".into();
            t.progress = 0.60;
            t.message = "开始翻译…".into();
        });

        let texts: Vec<String> = segments
            .iter()
            .map(|s| s.get("text").and_then(|v| v.as_str()).unwrap_or("").to_string())
            .collect();
        let detected = ctx
            .asr_meta
            .get("language")
            .and_then(|v| v.as_str())
            .filter(|l| ["zh", "en", "ja"].contains(l))
            .map(|s| s.to_string());
        let target = ctx.target_lang.clone();
        let manager = Arc::clone(&core.manager);
        let task_id = ctx.task_id.clone();
        let progress: translate::ProgressCb = Arc::new(move |done, total, msg| {
            manager.update(&task_id, false, EventKind::Progress, |t| {
                t.progress = 0.60 + 0.25 * (done as f64 / total.max(1) as f64);
                t.message = if msg.is_empty() {
                    format!("翻译 {}/{}", done, total)
                } else {
                    msg.to_string()
                };
            });
        });
        let translated =
            translate::translate_segments(&tcfg, texts, detected, target, progress)
                .await
                .map_err(StageError::Other)?;
        let segs = ctx.segments.as_mut().unwrap();
        for (seg, tr) in segs.iter_mut().zip(translated) {
            seg["translated"] = Value::String(tr);
        }
    } else if !ctx.need_translate {
        let r = reason.clone();
        let segs = ctx.segments.as_mut().unwrap();
        for seg in segs.iter_mut() {
            seg["translated"] = seg["text"].clone();
        }
        core.manager.update(&ctx.task_id, false, EventKind::Progress, |t| {
            t.message = r.clone();
        });
    } else {
        core.manager.update(&ctx.task_id, false, EventKind::Progress, |t| {
            t.message = "已有译文，跳过翻译（续跑）".into();
        });
    }
    Ok(())
}

async fn stage_proofread(core: &Core, ctx: &mut StageCtx) -> Result<(), StageError> {
    let proofread_on = ctx
        .cfg
        .pointer("/translate/proofread")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let segments = ctx.segments.clone().unwrap_or_default();
    if !(ctx.need_translate && !ctx.all_translated && !segments.is_empty() && proofread_on) {
        return Ok(());
    }
    core.manager.update(&ctx.task_id, true, EventKind::Progress, |t| {
        t.status = "proofreading".into();
        t.stage = "proofreading".into();
        t.progress = 0.85;
        t.message = "开始审校…".into();
    });

    let tcfg = translate::TranslateConfig::from_cfg(&ctx.cfg.get("translate").cloned().unwrap_or(json!({})));
    let sources: Vec<String> = segments
        .iter()
        .map(|s| s.get("text").and_then(|v| v.as_str()).unwrap_or("").to_string())
        .collect();
    let drafts: Vec<String> = segments
        .iter()
        .map(|s| s.get("translated").and_then(|v| v.as_str()).unwrap_or("").to_string())
        .collect();
    let target = ctx.target_lang.clone();
    let manager = Arc::clone(&core.manager);
    let task_id = ctx.task_id.clone();
    let progress: translate::ProgressCb = Arc::new(move |done, total, msg| {
        manager.update(&task_id, false, EventKind::Progress, |t| {
            t.progress = 0.85 + 0.10 * (done as f64 / total.max(1) as f64);
            t.message = if msg.is_empty() {
                format!("审校 {}/{}", done, total)
            } else {
                msg.to_string()
            };
        });
    });
    let polished = translate::proofread_segments(&tcfg, sources, drafts, target, progress)
        .await
        .map_err(StageError::Other)?;
    let segs = ctx.segments.as_mut().unwrap();
    for (seg, tr) in segs.iter_mut().zip(polished) {
        seg["translated"] = Value::String(tr);
    }
    for seg in segs.iter_mut() {
        if seg.get("translated").and_then(|v| v.as_str()).unwrap_or("").is_empty() {
            seg["translated"] = seg["text"].clone();
        }
    }
    Ok(())
}

async fn stage_write(core: &Core, ctx: &mut StageCtx) -> Result<(), StageError> {
    core.manager.update(&ctx.task_id, true, EventKind::Progress, |t| {
        t.status = "writing".into();
        t.stage = "writing".into();
        t.progress = 0.95;
        t.message = "生成字幕文件…".into();
    });
    let task = core.manager.get(&ctx.task_id).ok_or_else(|| {
        StageError::Other(anyhow::anyhow!("任务不存在: {}", ctx.task_id))
    })?;
    // 重试/续跑时复用已记录的产物目录（避免另建带后缀的新文件夹，
    // 保证「视频与字幕同目录」在重跑后依然成立）；目录被用户删掉则重新规划
    let out_dir = match task.output_dir.as_ref() {
        Some(d) if Path::new(d).is_dir() => PathBuf::from(d),
        _ => plan_output_dir(core.paths(), &ctx.task_id, &task.name),
    };
    let segments = ctx.segments.clone().unwrap_or_default();
    let outputs = subtitles::write_all(&out_dir, &segments, &ctx.cfg.get("output").cloned().unwrap_or(json!({})));
    let _ = std::fs::write(
        ctx.task_dir.join("segments.json"),
        serde_json::to_string(&segments).unwrap_or_default(),
    );
    ctx.out_dir = Some(out_dir.clone());
    ctx.outputs = outputs.clone();

    let keep_audio = ctx
        .cfg
        .pointer("/pipeline/keep_audio")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if !keep_audio {
        let _ = std::fs::remove_file(ctx.task_dir.join("audio.wav"));
    }

    // URL 下载的视频移入产物目录，与字幕同文件夹（本地文件保持零拷贝引用不动）
    let move_video = ctx
        .cfg
        .pointer("/pipeline/move_video_to_output")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    if move_video && task.source_url.is_some() {
        if let Some(mp) = task.media.path.clone() {
            let src = PathBuf::from(&mp);
            if src.is_file() && src.parent() != Some(out_dir.as_path()) {
                if let Some(fname) = src.file_name() {
                    let dest = out_dir.join(fname);
                    let moved = std::fs::rename(&src, &dest).or_else(|_| {
                        std::fs::copy(&src, &dest).map(|_| std::fs::remove_file(&src)).map(|_| ())
                    });
                    if moved.is_ok() {
                        let new_path = dest.to_string_lossy().to_string();
                        core.manager.update(&ctx.task_id, true, EventKind::Progress, |t| {
                            t.media.path = Some(new_path.clone());
                        });
                    }
                }
            }
        }
    }

    let n = segments.len();
    let translated_n = segments.iter().filter(|s| seg_translated(s)).count();
    let out_dir_str = out_dir.to_string_lossy().to_string();
    let outputs_v = Value::Object(outputs);
    core.manager.update(&ctx.task_id, true, EventKind::Progress, |t| {
        t.status = "done".into();
        t.stage = "done".into();
        t.progress = 1.0;
        t.message = format!("完成：{} 段", n);
        t.counts.segments = n as u64;
        t.counts.translated = translated_n as u64;
        t.has_result = true;
        t.outputs = outputs_v.clone();
        t.output_dir = Some(out_dir_str.clone());
    });
    Ok(())
}

// ---------- 主流程 ----------

pub async fn run_task(core: &Core, task_id: &str) {
    let Some(task) = core.manager.get(task_id) else { return };
    if task.status != "queued" {
        return;
    }
    let options = task.options.clone();
    let mut ctx = StageCtx {
        task_id: task_id.to_string(),
        task_dir: core.paths.tasks().join(task_id),
        source_lang: options
            .get("source_lang")
            .and_then(|v| v.as_str())
            .unwrap_or("auto")
            .to_string(),
        target_lang: options
            .get("target_lang")
            .and_then(|v| v.as_str())
            .unwrap_or("zh")
            .to_string(),
        cfg: core.cfg.get(),
        segments: None,
        asr_meta: Value::Null,
        need_translate: true,
        all_translated: false,
        out_dir: None,
        outputs: serde_json::Map::new(),
        cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
    };

    // 取消监视器：manager 状态 → ctx.cancel（阶段内部的所有子进程/推理轮询它）
    {
        let manager = Arc::clone(&core.manager);
        let cancel = Arc::clone(&ctx.cancel);
        let tid = task_id.to_string();
        tokio::spawn(async move {
            loop {
                let Some(t) = manager.get(&tid) else { break };
                if t.cancel {
                    cancel.store(true, Ordering::Relaxed);
                    break;
                }
                if matches!(t.status.as_str(), "done" | "error" | "canceled") {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            }
        });
    }

    macro_rules! stage {
        ($f:expr) => {
            if core.manager.is_cancelled(task_id) {
                ctx.cancel.store(true, Ordering::Relaxed);
            }
            match $f(core, &mut ctx).await {
                Ok(()) => {}
                Err(StageError::Canceled(msg)) => {
                    core.manager.update(task_id, true, EventKind::Canceled, |t| {
                        t.status = "canceled".into();
                        t.stage = "canceled".into();
                        t.message = "已取消".into();
                        let _ = msg;
                    });
                    core.manager.emit_final(task_id);
                    return;
                }
                Err(StageError::Other(e)) => {
                    let msg_short: String = format!("{:#}", e).chars().take(200).collect();
                    let msg_full: String = format!("{:#}", e).chars().take(500).collect();
                    core.manager.update(task_id, true, EventKind::Error, |t| {
                        t.status = "error".into();
                        t.stage = "error".into();
                        t.error = Some(msg_full.clone());
                        t.message = format!("失败：{}", msg_short);
                    });
                    core.manager.emit_final(task_id);
                    return;
                }
            }
        };
    }

    stage!(stage_download);
    stage!(stage_extract);
    stage!(stage_asr);
    stage!(stage_translate);
    stage!(stage_proofread);
    stage!(stage_write);
    core.manager.emit_final(task_id);
}

// ---------- 单句编辑 / 重翻 ----------

fn rewrite_outputs(
    core: &Core,
    task: &crate::tasks::Task,
    segments: &[Value],
) -> (PathBuf, serde_json::Map<String, Value>) {
    let out_dir = task
        .output_dir
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(|| core.paths.tasks().join(&task.id).join("output"));
    let out_cfg = core.cfg.get().get("output").cloned().unwrap_or(json!({}));
    let outputs = subtitles::write_all(&out_dir, segments, &out_cfg);
    // 把产物路径回填（output 目录可能首次生成）
    (out_dir, outputs)
}

pub fn update_segment_text(core: &Core, task_id: &str, index: usize, translated: &str) -> Result<Value, String> {
    let seg_file = core.paths.tasks().join(task_id).join("segments.json");
    let text = std::fs::read_to_string(&seg_file).map_err(|e| format!("读取识别结果失败: {}", e))?;
    let mut segments: Vec<Value> =
        serde_json::from_str(&text).map_err(|e| format!("解析识别结果失败: {}", e))?;
    if index >= segments.len() {
        return Err("段索引越界".into());
    }
    segments[index]["translated"] = Value::String(translated.trim().to_string());
    std::fs::write(&seg_file, serde_json::to_string(&segments).unwrap_or_default())
        .map_err(|e| format!("写入失败: {}", e))?;
    let task = core
        .manager
        .get(task_id)
        .ok_or("任务不存在")?;
    let _ = rewrite_outputs(core, &task, &segments);
    let translated_n = segments.iter().filter(|s| seg_translated(s)).count();
    core.manager.update(task_id, true, EventKind::Progress, |t| {
        t.counts.translated = translated_n as u64;
    });
    Ok(segments[index].clone())
}

pub async fn retranslate_segment(core: &Core, task_id: &str, index: usize) -> Result<Value, String> {
    let seg_file = core.paths.tasks().join(task_id).join("segments.json");
    let text = std::fs::read_to_string(&seg_file).map_err(|e| format!("读取识别结果失败: {}", e))?;
    let mut segments: Vec<Value> =
        serde_json::from_str(&text).map_err(|e| format!("解析识别结果失败: {}", e))?;
    if index >= segments.len() {
        return Err("段索引越界".into());
    }

    let cfg_full = core.cfg.get();
    let tcfg = translate::TranslateConfig::from_cfg(&cfg_full.get("translate").cloned().unwrap_or(json!({})));
    let target_lang = core
        .manager
        .get(task_id)
        .and_then(|t| {
            t.options
                .get("target_lang")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        })
        .unwrap_or_else(|| "zh".into());

    // 前文上下文（最多 3 句已定稿 + 目标句）
    let lo = index.saturating_sub(3);
    let mut ctx_texts: Vec<String> = Vec::new();
    for seg in &segments[lo..index] {
        let t = seg
            .get("translated")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| seg.get("text").and_then(|v| v.as_str()).unwrap_or(""));
        ctx_texts.push(t.to_string());
    }
    let target_text = segments[index]
        .get("text")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    ctx_texts.push(target_text);

    let result = translate::retranslate_one(&tcfg, ctx_texts, &target_lang)
        .await
        .map_err(|e| format!("重翻失败：{}", e))?;
    if !result.trim().is_empty() {
        segments[index]["translated"] = Value::String(result);
    }
    std::fs::write(&seg_file, serde_json::to_string(&segments).unwrap_or_default())
        .map_err(|e| format!("写入失败: {}", e))?;
    let task = core.manager.get(task_id).ok_or("任务不存在")?;
    let _ = rewrite_outputs(core, &task, &segments);
    let translated_n = segments.iter().filter(|s| seg_translated(s)).count();
    core.manager.update(task_id, true, EventKind::Progress, |t| {
        t.counts.translated = translated_n as u64;
    });
    Ok(segments[index].clone())
}

// ---------- 段编辑纯逻辑（无 IO，可独立单元测试） ----------

/// 毫秒级精度：四舍五入保留 3 位小数（时间轴浮点累加不漂移）。
pub fn round_ms(x: f64) -> f64 {
    if !x.is_finite() {
        return 0.0;
    }
    (x * 1000.0).round() / 1000.0
}

fn seg_f64(seg: &Value, key: &str) -> f64 {
    seg.get(key).and_then(|v| v.as_f64()).unwrap_or(0.0)
}

fn seg_text(seg: &Value, key: &str) -> String {
    seg.get(key)
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

/// index 连续重排为 0..n-1（所有编辑命令落盘前必做）。
pub fn reindex_segments(segments: &mut [Value]) {
    for (i, seg) in segments.iter_mut().enumerate() {
        if let Some(obj) = seg.as_object_mut() {
            obj.insert("index".into(), json!(i as u64));
        }
    }
}

/// 已翻译（非空且非 mock 标记）段数。
pub fn translated_count(segments: &[Value]) -> usize {
    segments.iter().filter(|s| seg_translated(s)).count()
}

/// 批量重翻并发上限：config translate.concurrency 夹到 1..=6。
pub fn clamp_concurrency(concurrency: usize) -> usize {
    concurrency.clamp(1, 6)
}

fn seg_join(a: String, b: String) -> String {
    let a = a.trim();
    let b = b.trim();
    match (a.is_empty(), b.is_empty()) {
        (true, true) => String::new(),
        (true, false) => b.to_string(),
        (false, true) => a.to_string(),
        (false, false) => format!("{} {}", a, b),
    }
}

/// 仅更新传入的字段；text/translated trim；start 下限 0。
/// 保证 end > start（不足则 end = start + 0.2）。时间保留 3 位小数。
pub fn patch_segment_fields_in(
    segments: &mut [Value],
    index: usize,
    text: Option<&str>,
    translated: Option<&str>,
    start: Option<f64>,
    end: Option<f64>,
) -> Result<(), String> {
    if index >= segments.len() {
        return Err(format!(
            "段索引越界：index={}，当前共 {} 段",
            index,
            segments.len()
        ));
    }
    if let Some(s) = start {
        if !s.is_finite() {
            return Err("开始时间必须为有效数字".into());
        }
    }
    if let Some(e) = end {
        if !e.is_finite() {
            return Err("结束时间必须为有效数字".into());
        }
    }
    let seg = &mut segments[index];
    if let Some(t) = text {
        seg["text"] = Value::String(t.trim().to_string());
    }
    if let Some(t) = translated {
        seg["translated"] = Value::String(t.trim().to_string());
    }
    if let Some(s) = start {
        seg["start"] = json!(round_ms(s.max(0.0)));
    }
    if let Some(e) = end {
        seg["end"] = json!(round_ms(e));
    }
    let s = seg_f64(seg, "start").max(0.0);
    if seg_f64(seg, "end") <= s {
        seg["end"] = json!(round_ms(s + 0.2));
    }
    Ok(())
}

/// 全部 start/end += offset；start 下限 0；保证 end >= start + 0.05；保留 3 位小数。
pub fn shift_segments_in(segments: &mut [Value], offset: f64) -> Result<(), String> {
    if !offset.is_finite() {
        return Err("平移秒数必须为有效数字".into());
    }
    for seg in segments.iter_mut() {
        let s = round_ms((seg_f64(seg, "start") + offset).max(0.0));
        let mut e = round_ms(seg_f64(seg, "end") + offset);
        if e < s + 0.05 {
            e = round_ms(s + 0.05);
        }
        seg["start"] = json!(s);
        seg["end"] = json!(e);
    }
    Ok(())
}

/// 合并 index 与 index+1：start 取前者、end 取后者，文本非空则以空格拼接，flagged 取或。
/// 合并后 index 连续重排。
pub fn merge_segments_in(segments: &mut Vec<Value>, index: usize) -> Result<(), String> {
    if index + 1 >= segments.len() {
        return Err(format!(
            "段索引越界：index={}，需要 index 与 index+1 都存在（当前共 {} 段）",
            index,
            segments.len()
        ));
    }
    let next = segments.remove(index + 1);
    let seg = &mut segments[index];
    let text = seg_join(seg_text(seg, "text"), seg_text(&next, "text"));
    let translated = seg_join(seg_text(seg, "translated"), seg_text(&next, "translated"));
    let flagged = seg
        .get("flagged")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
        || next
            .get("flagged")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
    seg["text"] = Value::String(text);
    seg["translated"] = Value::String(translated);
    seg["flagged"] = Value::Bool(flagged);
    if let Some(e) = next.get("end").and_then(|v| v.as_f64()) {
        seg["end"] = json!(round_ms(e));
    }
    let s = seg_f64(seg, "start");
    if seg_f64(seg, "end") < s + 0.05 {
        seg["end"] = json!(round_ms(s + 0.05));
    }
    reindex_segments(segments);
    Ok(())
}

/// 按时间比例切分字符（时间比例不变式：0 <= ratio <= 1）。
fn split_chars(text: &str, ratio: f64) -> (String, String) {
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    if n == 0 {
        return (String::new(), String::new());
    }
    let cut = if n >= 2 {
        ((ratio * n as f64).round() as usize).clamp(1, n - 1)
    } else if ratio >= 0.5 {
        n
    } else {
        0
    };
    (
        chars[..cut].iter().collect(),
        chars[cut..].iter().collect(),
    )
}

/// 在 start < at < end-0.05 处拆分；text/translated 按时间比例切字符；index 重排。
pub fn split_segment_in(
    segments: &mut Vec<Value>,
    index: usize,
    at: f64,
) -> Result<(), String> {
    if index >= segments.len() {
        return Err(format!(
            "段索引越界：index={}，当前共 {} 段",
            index,
            segments.len()
        ));
    }
    if !at.is_finite() {
        return Err("拆分时间点必须为有效数字".into());
    }
    let start = seg_f64(&segments[index], "start");
    let end = seg_f64(&segments[index], "end");
    if !(at > start && at < end - 0.05) {
        return Err(format!(
            "拆分时间点不合法：需满足 {:.3} < at < {:.3}（至少保留 50ms）",
            start,
            (end - 0.05).max(0.0)
        ));
    }
    let at = round_ms(at);
    let ratio = ((at - start) / (end - start).max(1e-6)).clamp(0.0, 1.0);
    let (head_text, tail_text) = split_chars(&seg_text(&segments[index], "text"), ratio);
    let has_translated = segments[index].get("translated").is_some();
    let (head_tr, tail_tr) = split_chars(&seg_text(&segments[index], "translated"), ratio);

    let mut tail = segments[index].clone();
    {
        let cur = &mut segments[index];
        cur["end"] = json!(at);
        cur["text"] = Value::String(head_text);
        if has_translated {
            cur["translated"] = Value::String(head_tr);
        }
    }
    tail["start"] = json!(at);
    tail["end"] = json!(round_ms(end));
    tail["text"] = Value::String(tail_text);
    if has_translated {
        tail["translated"] = Value::String(tail_tr);
    }
    segments.insert(index + 1, tail);
    reindex_segments(segments);
    Ok(())
}

// ---------- 段编辑落盘（segments.json → 产物 → counts） ----------

/// 读取任务当前 segments.json（编辑类命令共用）。
fn load_task_segments(core: &Core, task_id: &str) -> Result<(PathBuf, Vec<Value>), String> {
    if core.manager.get(task_id).is_none() {
        return Err("任务不存在".into());
    }
    let seg_file = core.paths.tasks().join(task_id).join("segments.json");
    let text = std::fs::read_to_string(&seg_file).map_err(|e| format!("读取识别结果失败：{}", e))?;
    let segments: Vec<Value> =
        serde_json::from_str(&text).map_err(|e| format!("解析识别结果失败：{}", e))?;
    Ok((seg_file, segments))
}

/// 编辑落盘统一出口：index 重排 → 写 segments.json → 重写全部产物 → 更新 counts（含段数/译数）。
fn persist_edited(core: &Core, task_id: &str, segments: &mut Vec<Value>) -> Result<(), String> {
    reindex_segments(segments);
    let task = core.manager.get(task_id).ok_or("任务不存在")?;
    let seg_file = core.paths.tasks().join(task_id).join("segments.json");
    let json_text =
        serde_json::to_string(segments).map_err(|e| format!("序列化识别结果失败：{}", e))?;
    std::fs::write(&seg_file, json_text).map_err(|e| format!("写入识别结果失败：{}", e))?;

    let (out_dir, outputs) = rewrite_outputs(core, &task, segments);
    let out_dir_s = out_dir.to_string_lossy().to_string();
    let n = segments.len();
    let translated_n = translated_count(segments);
    let outputs_v = Value::Object(outputs);
    core.manager.update(task_id, true, EventKind::Progress, |t| {
        t.counts.segments = n as u64;
        t.counts.translated = translated_n as u64;
        t.outputs = outputs_v.clone();
        t.has_result = true;
        if t.output_dir.is_none() {
            t.output_dir = Some(out_dir_s.clone());
        }
    });
    Ok(())
}

/// 段落字段编辑（仅更新传入字段）。返回最新全量 segments。
pub fn patch_segment_fields(
    core: &Core,
    task_id: &str,
    index: u64,
    text: Option<&str>,
    translated: Option<&str>,
    start: Option<f64>,
    end: Option<f64>,
) -> Result<Vec<Value>, String> {
    let (_, mut segments) = load_task_segments(core, task_id)?;
    patch_segment_fields_in(
        &mut segments,
        index as usize,
        text,
        translated,
        start,
        end,
    )?;
    persist_edited(core, task_id, &mut segments)?;
    Ok(segments)
}

/// 时间轴整体平移。返回最新全量 segments。
pub fn shift_segments(core: &Core, task_id: &str, offset: f64) -> Result<Vec<Value>, String> {
    let (_, mut segments) = load_task_segments(core, task_id)?;
    shift_segments_in(&mut segments, offset)?;
    persist_edited(core, task_id, &mut segments)?;
    Ok(segments)
}

/// 合并 index 与 index+1。返回最新全量 segments。
pub fn merge_segments(core: &Core, task_id: &str, index: u64) -> Result<Vec<Value>, String> {
    let (_, mut segments) = load_task_segments(core, task_id)?;
    merge_segments_in(&mut segments, index as usize)?;
    persist_edited(core, task_id, &mut segments)?;
    Ok(segments)
}

/// 在 at 处拆分段。返回最新全量 segments。
pub fn split_segment(
    core: &Core,
    task_id: &str,
    index: u64,
    at: f64,
) -> Result<Vec<Value>, String> {
    let (_, mut segments) = load_task_segments(core, task_id)?;
    split_segment_in(&mut segments, index as usize, at)?;
    persist_edited(core, task_id, &mut segments)?;
    Ok(segments)
}

/// 批量重翻：并发 clamp(translate.concurrency,1,6)，复用单句重翻提示词与上下文。
/// 整批只写一次 segments.json、只重写一次产物。返回 (最新全量 segments, failed)。
pub async fn retranslate_batch(
    core: &Core,
    task_id: &str,
    indices: &[u64],
) -> Result<(Vec<Value>, Vec<Value>), String> {
    use futures_util::StreamExt;

    let (_, mut segments) = load_task_segments(core, task_id)?;
    let total = segments.len();

    let mut failed: Vec<Value> = Vec::new();
    let mut jobs: Vec<usize> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for &raw in indices {
        let idx = raw as usize;
        if !seen.insert(idx) {
            continue;
        }
        if idx >= total {
            failed.push(json!({"index": raw, "error": "段索引越界"}));
            continue;
        }
        jobs.push(idx);
    }
    if jobs.is_empty() {
        return Ok((segments, failed));
    }

    let cfg_full = core.cfg.get();
    let tcfg = Arc::new(translate::TranslateConfig::from_cfg(
        &cfg_full.get("translate").cloned().unwrap_or(json!({})),
    ));
    let target_lang = core
        .manager
        .get(task_id)
        .and_then(|t| {
            t.options
                .get("target_lang")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        })
        .unwrap_or_else(|| "zh".into());
    let concurrency = clamp_concurrency(tcfg.concurrency);
    let snapshot = Arc::new(segments.clone());

    let futures: Vec<_> = jobs
        .into_iter()
        .map(|idx| {
            let tcfg = Arc::clone(&tcfg);
            let snapshot = Arc::clone(&snapshot);
            let target = target_lang.clone();
            async move {
                // 前文上下文：最多 3 句已定稿 + 目标句（与单句重翻一致）
                let lo = idx.saturating_sub(3);
                let mut ctx_texts: Vec<String> = snapshot[lo..idx]
                    .iter()
                    .map(|seg| {
                        seg.get("translated")
                            .and_then(|v| v.as_str())
                            .filter(|s| !s.trim().is_empty())
                            .unwrap_or_else(|| {
                                seg.get("text").and_then(|v| v.as_str()).unwrap_or("")
                            })
                            .to_string()
                    })
                    .collect();
                ctx_texts.push(
                    snapshot[idx]
                        .get("text")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                );
                let result = translate::retranslate_one(&tcfg, ctx_texts, &target).await;
                (idx, result)
            }
        })
        .collect();

    let results: Vec<(usize, anyhow::Result<String>)> =
        futures_util::stream::iter(futures)
            .buffer_unordered(concurrency)
            .collect()
            .await;

    for (idx, result) in results {
        match result {
            Ok(text) => {
                let text = text.trim().to_string();
                if !text.is_empty() {
                    segments[idx]["translated"] = Value::String(text);
                }
            }
            Err(e) => failed.push(json!({"index": idx as u64, "error": format!("{:#}", e)})),
        }
    }
    persist_edited(core, task_id, &mut segments)?;
    Ok((segments, failed))
}

// 供未来扩展使用的占位（保留与 Python 相同的阶段顺序文档）
#[allow(dead_code)]
fn _stages_order() -> &'static str {
    "download → extract → asr → translate → proofread → write"
}

#[allow(dead_code)]
fn _unused_time() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
