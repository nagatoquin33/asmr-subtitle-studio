//! whisper.cpp 最小隔离测试：直转 e2e 任务目录里的 audio.wav（无回调/无VAD/Greedy）。

use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

fn main() -> anyhow::Result<()> {
    let wav = std::env::args()
        .nth(1)
        .expect("用法: asr_min <16k-mono-wav>");
    let samples = asmr_core::asr::load_wav_16k_mono(std::path::Path::new(&wav))?;
    println!("samples: {}", samples.len());

    let model = r"D:\ai\plugin test\asmr-subtitle-studio\desktop\target\e2e-home\data\models\small\model.bin";
    let mut cp = WhisperContextParameters::new();
    cp.use_gpu = false;
    let ctx = WhisperContext::new_with_params(model, cp)?;

    for (name, threads, lang) in [
        ("beam5-auto-4t+abort", 4, ""),
        ("beam5-auto-4t+prog", 4, ""),
        ("greedy-auto-4t+abort", 4, ""),
    ] {
        let mut state = ctx.create_state()?;
        let use_beam = name.starts_with("beam");
        let mut params = if use_beam {
            FullParams::new(SamplingStrategy::BeamSearch { beam_size: 5, patience: -1.0 })
        } else {
            FullParams::new(SamplingStrategy::Greedy { best_of: 1 })
        };
        params.set_n_threads(threads);
        if lang.is_empty() {
            params.set_language(None);
        } else {
            params.set_language(Some(lang));
        }
        if name.ends_with("+abort") {
            let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            params.set_abort_callback_safe(move || cancel.load(std::sync::atomic::Ordering::Relaxed));
        }
        if name.ends_with("+prog") {
            params.set_progress_callback_safe(|_pct| {});
        }
        let t0 = std::time::Instant::now();
        match state.full(params, &samples) {
            Ok(()) => {
                let n = state.full_n_segments();
                let lang = state.full_lang_id_from_state();
                println!(
                    "[{}] OK: {} 段, lang_id={}, {:.1}s",
                    name,
                    n,
                    lang,
                    t0.elapsed().as_secs_f64()
                );
                for i in 0..n.min(3) {
                    if let Some(seg) = state.get_segment(i) {
                        println!("   {} -> {}", seg.start_timestamp(), seg.to_str_lossy().unwrap_or_default());
                    }
                }
            }
            Err(e) => println!("[{}] 失败: {:?}", name, e),
        }
    }
    Ok(())
}
