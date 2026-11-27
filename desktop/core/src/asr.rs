//! whisper.cpp ASR 封装（whisper-rs）：模型缓存、GPU/CPU 解析与回退、
//! Silero VAD（auto/on/off + 0 段自动重试）、进度/取消回调、幻觉标记。

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use whisper_rs::{
    FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters, WhisperVadParams,
};

use crate::ffmpeg::CancelFlag;

const MAX_LOADED_MODELS: usize = 2;

// Whisper 在静音/音乐段常见的幻觉文本（小写匹配）
const HALLUCINATION_SUBSTRINGS: &[&str] = &[
    "amara.org",
    "subtitles by",
    "subtitle by",
    "字幕by",
    "字幕由",
    "请不吝点赞",
    "请订阅",
    "感谢观看",
    "谢谢观看",
    "ご視聴ありがとう",
    "チャンネル登録",
    "字幕组",
    "请点赞",
    "字幕制作",
];
const HALLUCINATION_EXACT: &[&str] = &[
    "字幕", "字幕by", "谢谢你", "谢谢", "大家好", "明石市", "子",
];

pub fn flag_hallucination(text: &str) -> bool {
    let t = text.trim().to_lowercase();
    if t.is_empty() || t.chars().count() > 60 {
        return false;
    }
    if HALLUCINATION_EXACT.contains(&t.as_str()) {
        return true;
    }
    HALLUCINATION_SUBSTRINGS.iter().any(|p| t.contains(p))
}

/// 文本是否高度自重复（"哦哦哦…"、"あーあーあー"式哼鸣幻觉）：
/// 去重后字符占比过低且足够长。
fn is_degenerate_repeat(text: &str) -> bool {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() < 16 {
        return false;
    }
    let unique: std::collections::HashSet<char> = chars.iter().copied().collect();
    (unique.len() as f64) < 0.25 * (chars.len() as f64)
}

/// 幻觉游程过滤：whisper 在耳音/哼鸣/静音段常产出**连续完全相同的短段**。
/// 连续 ≥3 段文本相同（忽略首尾空白）→ 整段游程丢弃。
/// 另丢弃高重复字符段。返回丢弃段数（记入 asr.hallucination_dropped）。
pub fn filter_hallucination_runs(segments: &mut Vec<Value>) -> usize {
    let n = segments.len();
    let mut drop_mask = vec![false; n];

    // 1) 连续相同文本游程
    let norm = |s: &Value| {
        s.get("text")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string()
    };
    let mut i = 0;
    while i < n {
        let mut j = i + 1;
        while j < n && norm(&segments[j]) == norm(&segments[i]) {
            j += 1;
        }
        if j - i >= 3 {
            for k in i..j {
                drop_mask[k] = true;
            }
        }
        i = j;
    }

    // 2) 高重复字符段
    for (k, seg) in segments.iter().enumerate() {
        if let Some(t) = seg.get("text").and_then(|v| v.as_str()) {
            if is_degenerate_repeat(t) {
                drop_mask[k] = true;
            }
        }
    }

    let dropped = drop_mask.iter().filter(|d| **d).count();
    if dropped > 0 {
        let mut out = Vec::with_capacity(n - dropped);
        for (k, seg) in segments.drain(..).enumerate() {
            if !drop_mask[k] {
                out.push(seg);
            }
        }
        // 重排 index
        for (i, seg) in out.iter_mut().enumerate() {
            seg["index"] = json!(i);
        }
        *segments = out;
    }
    dropped
}

// whisper.cpp 语言表（前若干项；id → ISO 码，仅用于跳翻判定与元数据展示）
fn lang_code(id: i32) -> Option<&'static str> {
    Some(match id {
        0 => "en",
        1 => "zh",
        2 => "de",
        3 => "es",
        4 => "ru",
        5 => "ko",
        6 => "fr",
        7 => "ja",
        8 => "pt",
        9 => "tr",
        _ => return None,
    })
}

/// 读取 ffmpeg 产出的 16k 单声道 s16 wav（RIFF 解析，跳过 LIST 等附加块）。
pub fn load_wav_16k_mono(path: &std::path::Path) -> Result<Vec<f32>> {
    let bytes = std::fs::read(path).map_err(|e| anyhow!("读取音频失败: {}", e))?;
    if bytes.len() < 44 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err(anyhow!("不是有效的 WAV 文件"));
    }
    let mut pos = 12usize;
    let mut fmt_ok = false;
    let mut data: Option<(usize, usize)> = None; // (offset, len)
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let size = u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into().unwrap()) as usize;
        let body = pos + 8;
        match id {
            b"fmt " => {
                if size >= 16 {
                    let audio_format = u16::from_le_bytes(bytes[body..body + 2].try_into().unwrap());
                    let channels = u16::from_le_bytes(bytes[body + 2..body + 4].try_into().unwrap());
                    let rate = u32::from_le_bytes(bytes[body + 4..body + 8].try_into().unwrap());
                    if audio_format != 1 || channels != 1 || rate != 16000 {
                        return Err(anyhow!(
                            "音频格式不符（需 16k 单声道 PCM s16；实际 format={} ch={} rate={}）",
                            audio_format,
                            channels,
                            rate
                        ));
                    }
                    fmt_ok = true;
                }
            }
            b"data" => {
                let len = size.min(bytes.len().saturating_sub(body));
                data = Some((body, len));
            }
            _ => {}
        }
        pos = body + size + (size & 1); // 块按 2 字节对齐
        if data.is_some() && fmt_ok {
            break;
        }
    }
    let (offset, len) = data.ok_or_else(|| anyhow!("WAV 缺少 data 块"))?;
    if !fmt_ok {
        return Err(anyhow!("WAV 缺少 fmt 块"));
    }
    let n = len / 2;
    let mut samples = Vec::with_capacity(n);
    for i in 0..n {
        let raw = i16::from_le_bytes(bytes[offset + i * 2..offset + i * 2 + 2].try_into().unwrap());
        samples.push(raw as f32 / 32768.0);
    }
    Ok(samples)
}

#[derive(Serialize, Deserialize)]
pub struct AsrOptions {
    pub model_path: std::path::PathBuf,
    pub vad_model_path: Option<std::path::PathBuf>,
    pub device: String,              // auto / cpu / gpu
    pub source_lang: Option<String>, // Some(zh/en/ja)
    pub vad: String,                 // auto / on / off
}

#[derive(Serialize, Deserialize)]
pub struct AsrOutcome {
    pub segments: Vec<Value>,
    pub meta: Value,
}

struct ModelCache {
    entries: Mutex<HashMap<String, Arc<WhisperContext>>>,
    order: Mutex<Vec<String>>,
}

static MODEL_CACHE: once_cell::sync::Lazy<ModelCache> = once_cell::sync::Lazy::new(|| ModelCache {
    entries: Mutex::new(HashMap::new()),
    order: Mutex::new(Vec::new()),
});

fn load_context(model_path: &std::path::Path, use_gpu: bool) -> Result<Arc<WhisperContext>> {
    let key = format!("{}|{}", model_path.display(), use_gpu);
    {
        let entries = MODEL_CACHE.entries.lock();
        if let Some(ctx) = entries.get(&key) {
            return Ok(Arc::clone(ctx));
        }
    }
    let mut ctx_params = WhisperContextParameters::new();
    ctx_params.use_gpu = use_gpu;
    if use_gpu {
        // GPU 上开 flash attention：编码器提速 + 显存下降（coopmat 支持时）
        ctx_params.flash_attn = true;
    }
    let path_str = model_path
        .to_str()
        .ok_or_else(|| anyhow!("模型路径含非法字符"))?;
    let ctx = WhisperContext::new_with_params(path_str, ctx_params)
        .map_err(|e| anyhow!("加载模型失败（use_gpu={}）: {}", use_gpu, e))?;
    let ctx = Arc::new(ctx);
    let mut entries = MODEL_CACHE.entries.lock();
    let mut order = MODEL_CACHE.order.lock();
    entries.insert(key.clone(), Arc::clone(&ctx));
    order.push(key);
    while order.len() > MAX_LOADED_MODELS {
        let evict = order.remove(0);
        entries.remove(&evict);
    }
    Ok(ctx)
}

/// 单次转写尝试（在 blocking 线程运行；progress/cancel 均为 'static 共享句柄）。
fn transcribe_once(
    ctx: &WhisperContext,
    samples: &[f32],
    source_lang: Option<&str>,
    vad_model: Option<&std::path::Path>,
    use_vad: bool,
    progress: Arc<dyn Fn(f64) + Send + Sync>,
    cancel: CancelFlag,
) -> Result<(Vec<Value>, i32)> {
    let mut state = ctx.create_state().map_err(|e| anyhow!("创建推理状态失败: {}", e))?;
    let mut params = FullParams::new(SamplingStrategy::BeamSearch {
        beam_size: 5,
        patience: -1.0,
    });
    match source_lang {
        Some("zh") => params.set_language(Some("zh")),
        Some("en") => params.set_language(Some("en")),
        Some("ja") => params.set_language(Some("ja")),
        _ => params.set_language(None), // 自动检测
    }
    params.set_translate(false);
    params.set_no_context(true); // 对应 condition_on_previous_text=False
    params.set_print_progress(false);
    params.set_print_special(false);
    params.set_print_realtime(false);
    params.set_print_timestamps(false);
    // 混合架构（P+E 核）上 ggml 自旋屏障会因超订阅严重退化，
    // 钳到 8 线程（实测 4t≈16s vs 24t 数百秒）
    params.set_n_threads(
        std::thread::available_parallelism()
            .map(|n| n.get().min(8) as i32)
            .unwrap_or(4),
    );
    params.set_suppress_nst(true); // 抑制非语音 token（字幕组致谢类幻觉）
    params.set_debug_mode(false);  // 关闭 beam search 等调试日志刷屏

    if use_vad {
        if let Some(path) = vad_model {
            params.set_vad_model_path(Some(
                path.to_str().ok_or_else(|| anyhow!("VAD 模型路径含非法字符"))?,
            ));
            params.set_vad_params(WhisperVadParams::default());
            params.enable_vad(true);
        }
    }

    // 进度（节流 400ms）：safe 回调实现正确，直接用
    {
        let progress = Arc::clone(&progress);
        params.set_progress_callback_safe(move |pct: i32| {
            thread_local! {
                static LAST: std::cell::Cell<std::time::Instant> =
                    std::cell::Cell::new(std::time::Instant::now());
            }
            LAST.with(|last| {
                let now = std::time::Instant::now();
                if now - last.get() > std::time::Duration::from_millis(400) || pct >= 100 {
                    last.set(now);
                    progress(pct as f64 / 100.0);
                }
            });
        });
    }

    // 取消：whisper-rs 0.16 的 set_abort_callback_safe 存在类型不匹配 bug
    // （闭包双重装箱但 trampoline 按具体闭包类型解释 → UB，表现为随机 "failed to encode"）。
    // 这里照抄其 progress 回调的正确模式（统一 Box<dyn> 类型）自行接线。
    unsafe extern "C" fn abort_trampoline(ud: *mut std::ffi::c_void) -> bool {
        let f = &mut *(ud as *mut Box<dyn FnMut() -> bool>);
        f()
    }
    let abort_holder: Box<Box<dyn FnMut() -> bool>> = {
        let cancel = Arc::clone(&cancel);
        Box::new(Box::new(move || cancel.load(Ordering::Relaxed)))
    };
    let abort_ud = Box::into_raw(abort_holder) as *mut std::ffi::c_void;
    unsafe {
        params.set_abort_callback(Some(abort_trampoline));
        params.set_abort_callback_user_data(abort_ud);
    }

    state
        .full(params, samples)
        .map_err(|e| anyhow!("识别失败: {}", e))?;

    // 回收闭包（full 已同步返回，C 侧不会再触发回调）
    // SAFETY：指针来自上方 Box::into_raw，类型严格一致
    unsafe { drop(Box::from_raw(abort_ud as *mut Box<dyn FnMut() -> bool>)); }

    let lang_id = state.full_lang_id_from_state();
    let n = state.full_n_segments();
    let mut segments = Vec::new();
    for i in 0..n {
        let Some(seg) = state.get_segment(i) else { continue };
        let text = seg
            .to_str()
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        if text.is_empty() {
            continue;
        }
        segments.push(json!({
            "index": segments.len(),
            "start": (seg.start_timestamp() as f64) / 100.0,
            "end": (seg.end_timestamp() as f64) / 100.0,
            "text": text,
            "flagged": flag_hallucination(&text),
        }));
    }
    Ok((segments, lang_id))
}

/// 转写主入口（在 spawn_blocking 中调用）。
///
/// device=auto/gpu 优先 GPU；加载失败自动回退 CPU（带 warning）。
/// vad=auto：先开 VAD 识别，整段被过滤光（0 段）时自动关闭 VAD 重试。
#[allow(clippy::too_many_arguments)]
pub fn transcribe(
    opts: AsrOptions,
    wav_path: &std::path::Path,
    progress: Arc<dyn Fn(f64) + Send + Sync>,
    cancel: CancelFlag,
) -> Result<AsrOutcome> {
    let samples = load_wav_16k_mono(wav_path)?;

    // 编译期是否包含 GPU 后端（whisper.cpp 无 GPU 后端时会静默用 CPU，
    // 运行时无从探测，据特性如实上报）
    #[cfg(any(feature = "whisper-vulkan", feature = "whisper-cuda"))]
    const GPU_COMPILED: bool = true;
    #[cfg(not(any(feature = "whisper-vulkan", feature = "whisper-cuda")))]
    const GPU_COMPILED: bool = false;

    let want_gpu = opts.device != "cpu" && GPU_COMPILED;
    let (ctx, actual_gpu, warning) = match load_context(&opts.model_path, want_gpu) {
        Ok(ctx) => (ctx, want_gpu, String::new()),
        Err(e) if want_gpu => {
            let ctx = load_context(&opts.model_path, false)?;
            (
                ctx,
                false,
                format!("GPU 加载失败（{}），已回退 CPU", e),
            )
        }
        Err(e) => return Err(e),
    };

    let src = opts.source_lang.as_deref();
    let vad_mode = match opts.vad.as_str() {
        "on" => "on",
        "off" => "off",
        _ => "auto",
    };
    let vad_path = opts.vad_model_path.as_deref().filter(|_| vad_mode != "off");

    let t0 = std::time::Instant::now();
    let mut meta_extra = serde_json::Map::new();
    if !warning.is_empty() {
        meta_extra.insert("warning".into(), json!(warning));
    }

    let (mut segments, lang_id) = if vad_mode != "off" {
        let (segs, lid) = transcribe_once(&ctx, &samples, src, vad_path, true, progress.clone(), Arc::clone(&cancel))?;
        if segs.is_empty() && vad_mode == "auto" {
            // VAD 把整段音频过滤光了（歌曲/MV 常见）：关闭 VAD 重试
            meta_extra.insert("vad_retry".into(), json!(true));
            transcribe_once(&ctx, &samples, src, None, false, progress, cancel)?
        } else {
            (segs, lid)
        }
    } else {
        transcribe_once(&ctx, &samples, src, None, false, progress, cancel)?
    };

    let detected = lang_code(lang_id).map(|s| s.to_string());
    let asr_seconds = t0.elapsed().as_secs_f64();
    // 幻觉游程过滤（连续重复/高重复字符段——耳音、哼鸣段典型产物）
    let dropped = filter_hallucination_runs(&mut segments);
    if dropped > 0 {
        meta_extra.insert("hallucination_dropped".into(), json!(dropped));
    }
    let mut meta = json!({
        "backend": "whisper.cpp",
        "device": if actual_gpu { "gpu" } else { "cpu" },
        "asr_seconds": (asr_seconds * 10.0).round() / 10.0,
        "language": detected,
        "language_probability": 0.0,
    });
    if let Some(obj) = meta.as_object_mut() {
        for (k, v) in meta_extra {
            obj.insert(k, v);
        }
    }
    Ok(AsrOutcome { segments, meta })
}
