//! 字幕文件输出：SRT / VTT / ASS / JSON（与 Python 版逐字节兼容，含 BOM）。

use std::{fs, io};
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

pub const OUTPUT_TYPES: &[(&str, &str)] = &[
    ("orig_srt", "orig.srt"),
    ("trans_srt", "trans.srt"),
    ("bilingual_srt", "bilingual.srt"),
    ("bilingual_ass", "bilingual.ass"),
    ("bilingual_vtt", "bilingual.vtt"),
    ("json", "segments.json"),
];

pub fn output_filename(kind: &str) -> Option<&'static str> {
    OUTPUT_TYPES
        .iter()
        .find(|(k, _)| *k == kind)
        .map(|(_, f)| *f)
}

fn fmt_srt(t: f64) -> String {
    let t = t.max(0.0);
    let h = (t / 3600.0) as u64;
    let m = ((t - h as f64 * 3600.0) / 60.0) as u64;
    let s = t - h as f64 * 3600.0 - m as f64 * 60.0;
    format!(
        "{:02}:{:02}:{:02},{:03}",
        h,
        m,
        s as u64,
        ((s - s.floor()) * 1000.0).round() as u64
    )
}

fn fmt_vtt(t: f64) -> String {
    fmt_srt(t).replace(',', ".")
}

fn fmt_ass(t: f64) -> String {
    let t = t.max(0.0);
    let h = (t / 3600.0) as u64;
    let m = ((t - h as f64 * 3600.0) / 60.0) as u64;
    let s = t - h as f64 * 3600.0 - m as f64 * 60.0;
    format!("{}:{:02}:{:05.2}", h, m, s)
}

const PUNCT: &str = "，。！？、；：…—,.!?;: ";

/// 超长行按标点/空格在中点附近折成两行。
fn wrap(text: &str, max_chars: usize) -> String {
    let text = text.trim();
    if max_chars == 0 || text.chars().count() <= max_chars {
        return text.to_string();
    }
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    let mid = n / 2;
    let mut best = mid;
    // 从后往前找距中点较近的标点
    for i in (1..n).rev() {
        if PUNCT.contains(chars[i]) && (i as i64 - mid as i64).abs() <= (n as i64) / 4 + 4 {
            best = i + 1;
            break;
        }
    }
    let head: String = chars[..best].iter().collect();
    let tail: String = chars[best..].iter().collect();
    format!("{}\n{}", head.trim_end(), tail.trim_start())
}

fn seg_str(seg: &Value, key: &str) -> String {
    seg.get(key).and_then(|v| v.as_str()).unwrap_or("").trim().to_string()
}

fn seg_start(seg: &Value) -> f64 {
    seg.get("start").and_then(|v| v.as_f64()).unwrap_or(0.0)
}

fn seg_end(seg: &Value) -> f64 {
    seg.get("end").and_then(|v| v.as_f64()).unwrap_or(0.0)
}

fn pair(seg: &Value, mode: &str, order: &str) -> String {
    let orig = seg_str(seg, "text");
    let trans = {
        let t = seg_str(seg, "translated");
        if t.is_empty() { orig.clone() } else { t }
    };
    match mode {
        "orig" => orig,
        "trans" => trans,
        _ => {
            if order == "trans_first" {
                format!("{}\n{}", trans, orig)
            } else {
                format!("{}\n{}", orig, trans)
            }
        }
    }
}

fn write_text(path: &Path, text: &str, bom: bool) -> io::Result<PathBuf> {
    let mut out = Vec::new();
    if bom {
        out.extend_from_slice(&[0xEF, 0xBB, 0xBF]);
    }
    out.extend_from_slice(text.as_bytes());
    fs::write(path, out)?;
    Ok(path.to_path_buf())
}

fn write_srt(path: &Path, segments: &[Value], mode: &str, order: &str, max_chars: usize) -> io::Result<PathBuf> {
    let mut blocks = Vec::new();
    for (i, seg) in segments.iter().enumerate() {
        let text = wrap(&pair(seg, mode, order), max_chars);
        blocks.push(format!(
            "{}\n{} --> {}\n{}\n",
            i + 1,
            fmt_srt(seg_start(seg)),
            fmt_srt(seg_end(seg)),
            text
        ));
    }
    write_text(path, &blocks.join("\n"), true)
}

fn write_vtt(path: &Path, segments: &[Value], order: &str, max_chars: usize) -> io::Result<PathBuf> {
    let mut lines = vec!["WEBVTT".to_string(), String::new()];
    for seg in segments {
        lines.push(format!(
            "{} --> {}",
            fmt_vtt(seg_start(seg)),
            fmt_vtt(seg_end(seg))
        ));
        lines.push(wrap(&pair(seg, "bilingual", order), max_chars));
        lines.push(String::new());
    }
    write_text(path, &lines.join("\n"), false)
}

const ASS_HEADER: &str = "\
[Script Info]
ScriptType: v4.00+
PlayResX: 1920
PlayResY: 1080
WrapStyle: 0
ScaledBorderAndShadow: yes

[V4+ Styles]
Format: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding
Style: Trans,Microsoft YaHei,62,&H00FFFFFF,&H000000FF,&H00000000,&H96000000,0,0,0,0,100,100,0,0,1,2.2,1,2,80,80,72,1

[Events]
Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text
";

fn write_ass(path: &Path, segments: &[Value], order: &str, max_chars: usize) -> io::Result<PathBuf> {
    let mut events = Vec::new();
    for seg in segments {
        let trans = wrap(&seg_str(seg, "translated"), max_chars).replace('\n', "\\N");
        let trans = if trans.is_empty() { wrap(&seg_str(seg, "text"), max_chars).replace('\n', "\\N") } else { trans };
        let orig = wrap(&seg_str(seg, "text"), max_chars).replace('\n', "\\N");
        let text = if order == "trans_first" {
            format!("{}{{\\fs42\\c&HD8D8D8&}}\\N{}", trans, orig)
        } else {
            format!("{}{{\\fs42\\c&HD8D8D8&}}\\N{}", orig, trans)
        };
        events.push(format!(
            "Dialogue: 0,{},{},Trans,,0,0,0,,{}",
            fmt_ass(seg_start(seg)),
            fmt_ass(seg_end(seg)),
            text
        ));
    }
    write_text(path, &format!("{}\n{}\n", ASS_HEADER, events.join("\n")), true)
}

/// 在 out_dir 写出全部产物，返回 type → 绝对路径。
pub fn write_all(out_dir: &Path, segments: &[Value], out_cfg: &Value) -> io::Result<serde_json::Map<String, Value>> {
    let order = out_cfg
        .get("bilingual_order")
        .and_then(|v| v.as_str())
        .unwrap_or("trans_first")
        .to_string();
    let max_chars = out_cfg
        .get("max_line_chars")
        .and_then(|v| v.as_u64())
        .unwrap_or(42)
        .clamp(1, 500) as usize;

    fs::create_dir_all(out_dir)?;
    let mut paths = serde_json::Map::new();
    paths.insert(
        "orig_srt".into(),
        json!(write_srt(&out_dir.join("orig.srt"), segments, "orig", &order, max_chars)?.to_string_lossy()),
    );
    paths.insert(
        "trans_srt".into(),
        json!(write_srt(&out_dir.join("trans.srt"), segments, "trans", &order, max_chars)?.to_string_lossy()),
    );
    paths.insert(
        "bilingual_srt".into(),
        json!(write_srt(&out_dir.join("bilingual.srt"), segments, "bilingual", &order, max_chars)?.to_string_lossy()),
    );
    paths.insert(
        "bilingual_ass".into(),
        json!(write_ass(&out_dir.join("bilingual.ass"), segments, &order, max_chars)?.to_string_lossy()),
    );
    paths.insert(
        "bilingual_vtt".into(),
        json!(write_vtt(&out_dir.join("bilingual.vtt"), segments, &order, max_chars)?.to_string_lossy()),
    );
    let json_path = {
        let p = out_dir.join("segments.json");
        fs::write(&p, serde_json::to_vec(segments)?)?;
        p
    };
    paths.insert("json".into(), json!(json_path.to_string_lossy()));
    Ok(paths)
}
