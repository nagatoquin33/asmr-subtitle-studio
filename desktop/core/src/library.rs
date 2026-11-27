//! 作品库：以作品为单位保存媒体副本、相对路径与独立字幕。
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, LazyLock};
use std::time::UNIX_EPOCH;

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::downloader::{self, DownloadProgress};
use crate::ffmpeg::Ffmpeg;
use crate::paths::Paths;
use crate::pipeline::Core;
use crate::services::MEDIA_EXTS;
use crate::tasks::{new_task_id, Task, RUNNING_STAGES};

static RJ: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?i)RJ[0-9]{6,8}(?:[^0-9]|$)").unwrap());
static LRC_TAG: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"\[(\d{1,3}):(\d{2})(?:[.:](\d{1,3}))?\]").unwrap());
static ASS_OVERRIDE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"\{[^}]*\}").unwrap());
static TASK_CREATION: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

/// 导入进度回调：(已完成文件数, 总文件数, 已复制字节, 总字节, 当前文件名)
pub type ImportProgress = Arc<dyn Fn(usize, usize, u64, u64, &str) + Send + Sync>;

const SUBTITLE_EXTS: &[&str] = &["vtt", "srt", "ass", "ssa", "lrc"];
const LANG_SUFFIXES: &[&str] = &[
    "bilingual",
    "dual",
    "zh-cn",
    "zh-hans",
    "zh-hant",
    "zh-tw",
    "zh",
    "chs",
    "cht",
    "cn",
    "ja",
    "jp",
    "jpn",
    "en",
    "eng",
    "ko",
    "kor",
    "orig",
    "translated",
];

#[derive(Clone, Serialize, Deserialize)]
pub struct Track {
    pub id: String,
    pub path: PathBuf,
    pub video: bool,
    pub subtitle: Option<PathBuf>,
}

#[derive(Serialize, Deserialize)]
pub struct Work {
    pub id: String,
    pub title: String,
    pub rj: Option<String>,
    pub cover: Option<PathBuf>,
    pub tracks: Vec<Track>,
}

#[derive(Serialize)]
pub struct WorkSummary {
    pub id: String,
    pub title: String,
    pub rj: Option<String>,
    pub has_cover: bool,
    pub track_count: usize,
}

#[derive(Serialize)]
pub struct TrackStatus {
    #[serde(flatten)]
    track: Track,
    task_id: Option<String>,
    status: Option<String>,
    has_subtitles: bool,
    subtitle_version: u64,
}

#[derive(Serialize)]
pub struct WorkDetail {
    id: String,
    title: String,
    rj: Option<String>,
    has_cover: bool,
    tracks: Vec<TrackStatus>,
}

fn extension(path: &Path) -> String {
    path.extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
}

fn extract_rj(text: &str) -> Option<String> {
    RJ.find(text).map(|m| {
        m.as_str()
            .trim_end_matches(|c: char| !c.is_ascii_digit())
            .to_ascii_uppercase()
    })
}

fn file_version(path: &Path) -> u64 {
    fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(1)
}

/// 启动时清理异常退出残留的 `.import-*` 临时目录。
pub fn cleanup_stale_imports(paths: &Paths) {
    let Ok(entries) = fs::read_dir(paths.library()) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if name.to_string_lossy().starts_with(".import-") {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}

pub fn work_dir(paths: &Paths, id: &str) -> Result<PathBuf> {
    if !valid_id(id) {
        bail!("作品编号无效");
    }
    let root = paths.library().canonicalize()?;
    let dir = root.join(id).canonicalize().context("作品不存在")?;
    if dir.parent() != Some(&root) {
        bail!("作品目录不在库内");
    }
    Ok(dir)
}

pub fn load(paths: &Paths, id: &str) -> Result<Work> {
    let work: Work = serde_json::from_slice(&fs::read(work_dir(paths, id)?.join("work.json"))?)?;
    if work.id != id {
        bail!("作品编号与目录不符");
    }
    Ok(work)
}

pub fn rename(paths: &Paths, id: &str, title: &str) -> Result<()> {
    let title = title.trim();
    if title.is_empty() {
        bail!("请输入作品名称");
    }
    let dir = work_dir(paths, id)?;
    let mut work = load(paths, id)?;
    work.title = title.to_owned();
    let temp = dir.join("work.json.tmp");
    fs::write(&temp, serde_json::to_vec_pretty(&work)?)?;
    fs::rename(temp, dir.join("work.json"))?;
    Ok(())
}

/// 删除作品及其非运行中的关联字幕任务。
pub fn delete(core: &Core, id: &str) -> Result<()> {
    let dir = work_dir(&core.paths, id)?;
    let related: Vec<Task> = core
        .manager
        .list(usize::MAX)
        .into_iter()
        .filter(|t| task_keys(t).is_some_and(|(work, _)| work == id))
        .collect();
    if related
        .iter()
        .any(|t| RUNNING_STAGES.contains(&t.status.as_str()))
    {
        bail!("该作品有字幕任务正在运行，请先取消任务后再删除");
    }
    for task in related {
        let _ = core.manager.delete(&task.id);
    }
    fs::remove_dir_all(&dir).context("删除作品目录失败（请确认文件未被占用）")?;
    Ok(())
}

/// 检查待导入路径是否与作品库中已有作品（相同 RJ 编号或同名）重复。
pub fn find_duplicate(paths: &Paths, source: &Path) -> Result<Option<WorkSummary>> {
    let candidate = if source.is_dir() {
        source.file_name().and_then(|s| s.to_str())
    } else {
        source.file_stem().and_then(|s| s.to_str())
    };
    let Some(title) = candidate.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    let rj = extract_rj(title);
    let title_lower = title.to_lowercase();
    for work in list(paths)? {
        if let (Some(a), Some(b)) = (&rj, &work.rj) {
            if a.eq_ignore_ascii_case(b) {
                return Ok(Some(work));
            }
        }
        if work.title.trim().to_lowercase() == title_lower {
            return Ok(Some(work));
        }
    }
    Ok(None)
}

pub fn list(paths: &Paths) -> Result<Vec<WorkSummary>> {
    let mut out = Vec::new();
    for entry in fs::read_dir(paths.library())? {
        let entry = entry?;
        let id = entry.file_name();
        let Some(id) = id.to_str().filter(|id| valid_id(id)) else {
            continue;
        };
        if let Ok(work) = load(paths, id) {
            out.push(WorkSummary {
                id: work.id,
                title: work.title,
                rj: work.rj,
                has_cover: work.cover.is_some(),
                track_count: work.tracks.len(),
            });
        }
    }
    out.sort_by(|a, b| b.id.cmp(&a.id));
    Ok(out)
}

fn asset(base: &Path, relative: &Path) -> Result<PathBuf> {
    if !relative
        .components()
        .all(|c| matches!(c, Component::Normal(_)))
    {
        bail!("库内文件路径无效");
    }
    let path = base.join(relative).canonicalize()?;
    if !path.starts_with(base) || !path.is_file() {
        bail!("库内文件不存在");
    }
    Ok(path)
}

fn media_dir(paths: &Paths, work: &str) -> Result<PathBuf> {
    let dir = work_dir(paths, work)?;
    let media = dir.join("media").canonicalize()?;
    if media.parent() != Some(&dir) {
        bail!("媒体目录不在作品内");
    }
    Ok(media)
}

fn track_of<'a>(work: &'a Work, track: &str) -> Result<&'a Track> {
    work.tracks
        .iter()
        .find(|t| t.id == track)
        .ok_or_else(|| anyhow!("曲目不存在"))
}

pub fn track_path(paths: &Paths, work: &str, track: &str) -> Result<PathBuf> {
    let manifest = load(paths, work)?;
    asset(&media_dir(paths, work)?, &track_of(&manifest, track)?.path)
}

pub fn cover_path(paths: &Paths, work: &str) -> Result<PathBuf> {
    let manifest = load(paths, work)?;
    let cover = manifest.cover.ok_or_else(|| anyhow!("作品没有封面"))?;
    asset(&media_dir(paths, work)?, &cover)
}

fn subtitle_dir(paths: &Paths, work: &str, track: &str) -> Result<PathBuf> {
    if !valid_id(track) {
        bail!("曲目编号无效");
    }
    Ok(work_dir(paths, work)?.join("subtitles").join(track))
}

pub fn task_keys(task: &Task) -> Option<(&str, &str)> {
    Some((
        task.options.get("library_work")?.as_str()?,
        task.options.get("library_track")?.as_str()?,
    ))
}

pub fn task_output_dir(paths: &Paths, task: &Task) -> Option<PathBuf> {
    let (work, track) = task_keys(task)?;
    subtitle_dir(paths, work, track).ok()
}

#[cfg(windows)]
fn decode_windows_codepage(bytes: &[u8], codepage: u32) -> Option<String> {
    if bytes.is_empty() {
        return Some(String::new());
    }
    const MB_ERR_INVALID_CHARS: u32 = 0x0000_0008;
    #[link(name = "kernel32")]
    extern "system" {
        fn MultiByteToWideChar(
            code_page: u32,
            flags: u32,
            multi_byte_str: *const u8,
            cb_multi_byte: i32,
            wide_char_str: *mut u16,
            cch_wide_char: i32,
        ) -> i32;
    }
    let len = i32::try_from(bytes.len()).ok()?;
    let needed = unsafe {
        MultiByteToWideChar(
            codepage,
            MB_ERR_INVALID_CHARS,
            bytes.as_ptr(),
            len,
            std::ptr::null_mut(),
            0,
        )
    };
    if needed <= 0 {
        return None;
    }
    let mut wide = vec![0u16; needed as usize];
    let written = unsafe {
        MultiByteToWideChar(
            codepage,
            MB_ERR_INVALID_CHARS,
            bytes.as_ptr(),
            len,
            wide.as_mut_ptr(),
            needed,
        )
    };
    if written <= 0 {
        return None;
    }
    wide.truncate(written as usize);
    String::from_utf16(&wide).ok()
}

/// 解码常见字幕编码：UTF-8（含 BOM）、UTF-16 LE/BE、Shift-JIS（CP932）、GB18030（CP54936）。
pub fn decode_subtitle_bytes(bytes: &[u8]) -> String {
    if bytes.starts_with(&[0xFF, 0xFE]) && bytes.len() >= 2 {
        let units: Vec<u16> = bytes[2..]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        return String::from_utf16_lossy(&units);
    }
    if bytes.starts_with(&[0xFE, 0xFF]) && bytes.len() >= 2 {
        let units: Vec<u16> = bytes[2..]
            .chunks_exact(2)
            .map(|c| u16::from_be_bytes([c[0], c[1]]))
            .collect();
        return String::from_utf16_lossy(&units);
    }
    if let Ok(text) = std::str::from_utf8(bytes) {
        return text.trim_start_matches('\u{feff}').to_owned();
    }
    #[cfg(windows)]
    {
        let sjis = decode_windows_codepage(bytes, 932);
        let gb = decode_windows_codepage(bytes, 54936);
        match (sjis, gb) {
            (Some(j), Some(g)) => {
                let score_jp = |s: &str| -> i64 {
                    let mut score = 0i64;
                    for ch in s.chars() {
                        match ch {
                            '\u{3040}'..='\u{30FF}' => score += 4, // 平假名/片假名
                            '\u{3000}'..='\u{303F}' => score += 1, // 日文标点
                            '\u{FF61}'..='\u{FF9F}' => score -= 3, // 常见 GBK 误解成半角片假名
                            _ => {}
                        }
                    }
                    score
                };
                if score_jp(&j) > 0 {
                    return j.trim_start_matches('\u{feff}').to_owned();
                }
                return g.trim_start_matches('\u{feff}').to_owned();
            }
            (Some(j), None) => return j.trim_start_matches('\u{feff}').to_owned(),
            (None, Some(g)) => return g.trim_start_matches('\u{feff}').to_owned(),
            (None, None) => {}
        }
    }
    String::from_utf8_lossy(bytes)
        .trim_start_matches('\u{feff}')
        .to_owned()
}

fn format_vtt_ts(seconds: f64) -> String {
    let ms = (seconds.max(0.0) * 1000.0).round() as u64;
    let h = ms / 3_600_000;
    let m = (ms % 3_600_000) / 60_000;
    let s = (ms % 60_000) / 1000;
    let rem = ms % 1000;
    format!("{h:02}:{m:02}:{s:02}.{rem:03}")
}

fn parse_ass_ts(raw: &str) -> Option<f64> {
    let mut parts = raw.trim().split(':');
    let h: f64 = parts.next()?.parse().ok()?;
    let m: f64 = parts.next()?.parse().ok()?;
    let s: f64 = parts.next()?.parse().ok()?;
    Some(h * 3600.0 + m * 60.0 + s)
}

fn lrc_to_vtt(text: &str) -> String {
    let mut cues: Vec<(f64, String)> = Vec::new();
    for line in text.lines() {
        let mut times = Vec::new();
        for cap in LRC_TAG.captures_iter(line) {
            let min: f64 = cap[1].parse().unwrap_or(0.0);
            let sec: f64 = cap[2].parse().unwrap_or(0.0);
            let frac = cap.get(3).map_or(0.0, |m| {
                let s = m.as_str();
                let v: f64 = s.parse().unwrap_or(0.0);
                match s.len() {
                    1 => v / 10.0,
                    2 => v / 100.0,
                    _ => v / 1000.0,
                }
            });
            times.push(min * 60.0 + sec + frac);
        }
        if times.is_empty() {
            continue;
        }
        let body = LRC_TAG.replace_all(line, "").trim().to_owned();
        if body.is_empty() {
            continue;
        }
        for t in times {
            cues.push((t, body.clone()));
        }
    }
    cues.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut out = String::from("WEBVTT\n\n");
    for i in 0..cues.len() {
        let start = cues[i].0;
        let end = cues
            .get(i + 1)
            .map(|(next, _)| (*next).max(start + 0.4))
            .unwrap_or(start + 5.0);
        out.push_str(&format!(
            "{} --> {}\n{}\n\n",
            format_vtt_ts(start),
            format_vtt_ts(end),
            cues[i].1
        ));
    }
    out
}

fn ass_to_vtt(text: &str) -> String {
    let mut in_events = false;
    let mut start_idx = 1usize;
    let mut end_idx = 2usize;
    let mut text_idx = 9usize;
    let mut cues: Vec<(f64, f64, String)> = Vec::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.starts_with('[') && line.ends_with(']') {
            in_events = line.eq_ignore_ascii_case("[Events]");
            continue;
        }
        if !in_events {
            continue;
        }
        if let Some(rest) = line.strip_prefix("Format:") {
            for (idx, col) in rest.split(',').enumerate() {
                match col.trim().to_ascii_lowercase().as_str() {
                    "start" => start_idx = idx,
                    "end" => end_idx = idx,
                    "text" => text_idx = idx,
                    _ => {}
                }
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("Dialogue:") {
            let fields: Vec<&str> = rest.splitn(text_idx + 1, ',').collect();
            if fields.len() <= text_idx {
                continue;
            }
            let (Some(start), Some(end)) = (
                fields.get(start_idx).and_then(|s| parse_ass_ts(s)),
                fields.get(end_idx).and_then(|s| parse_ass_ts(s)),
            ) else {
                continue;
            };
            let cleaned = ASS_OVERRIDE
                .replace_all(fields[text_idx], "")
                .replace("\\N", "\n")
                .replace("\\n", "\n")
                .replace("\\h", " ");
            let body = cleaned.trim().to_owned();
            if !body.is_empty() && end > start {
                cues.push((start, end, body));
            }
        }
    }
    cues.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut out = String::from("WEBVTT\n\n");
    for (start, end, body) in cues {
        out.push_str(&format!(
            "{} --> {}\n{}\n\n",
            format_vtt_ts(start),
            format_vtt_ts(end),
            body
        ));
    }
    out
}

/// 将 vtt / srt / lrc / ass / ssa 文本统一转换为 WebVTT。
pub fn subtitle_to_vtt(ext: &str, text: &str) -> String {
    let text = text.trim_start_matches('\u{feff}');
    match ext {
        "vtt" => {
            if text.trim_start().starts_with("WEBVTT") {
                text.to_owned()
            } else {
                format!("WEBVTT\n\n{text}")
            }
        }
        "lrc" => lrc_to_vtt(text),
        "ass" | "ssa" => ass_to_vtt(text),
        _ => {
            let mut output = String::from("WEBVTT\n\n");
            for line in text.lines() {
                if line.contains(" --> ") {
                    output.push_str(&line.replace(',', "."));
                } else {
                    output.push_str(line);
                }
                output.push('\n');
            }
            output
        }
    }
}

pub fn vtt(paths: &Paths, work: &str, track: &str) -> Result<String> {
    let manifest = load(paths, work)?;
    let track = track_of(&manifest, track)?;
    let generated = subtitle_dir(paths, work, &track.id)?.join("bilingual.vtt");
    if generated.is_file() {
        return Ok(fs::read_to_string(generated)?);
    }
    let relative = track.subtitle.as_ref().ok_or_else(|| anyhow!("没有字幕"))?;
    let file = asset(&media_dir(paths, work)?, relative)?;
    let bytes = fs::read(&file)?;
    let text = decode_subtitle_bytes(&bytes);
    Ok(subtitle_to_vtt(&extension(&file), &text))
}

pub fn detail(core: &Core, id: &str) -> Result<WorkDetail> {
    let work = load(&core.paths, id)?;
    let media = media_dir(&core.paths, id).ok();
    let mut tasks = HashMap::new();
    for task in core.manager.list(usize::MAX) {
        if let Some((work_id, track)) = task_keys(&task) {
            if work_id == id {
                tasks.entry(track.to_owned()).or_insert(task);
            }
        }
    }
    let tracks = work
        .tracks
        .into_iter()
        .map(|track| {
            let task = tasks.get(&track.id);
            let generated = subtitle_dir(&core.paths, id, &track.id)
                .ok()
                .map(|d| d.join("bilingual.vtt"))
                .filter(|p| p.is_file());
            let sidecar = track
                .subtitle
                .as_ref()
                .and_then(|rel| media.as_ref().and_then(|m| asset(m, rel).ok()));
            let subtitle_version = if let Some(ref p) = generated {
                file_version(p).saturating_add(1)
            } else if let Some(ref p) = sidecar {
                file_version(p)
            } else {
                0
            };
            TrackStatus {
                has_subtitles: generated.is_some() || track.subtitle.is_some(),
                subtitle_version,
                task_id: task.map(|t| t.id.clone()),
                status: task.map(|t| t.status.clone()),
                track,
            }
        })
        .collect();
    Ok(WorkDetail {
        id: work.id,
        title: work.title,
        rj: work.rj,
        has_cover: work.cover.is_some(),
        tracks,
    })
}

pub fn queue(
    core: &Core,
    work_id: &str,
    tracks: &[String],
    source: &str,
    target: &str,
) -> Result<Vec<String>> {
    let _lock = TASK_CREATION.lock();
    let work = load(&core.paths, work_id)?;
    let media = media_dir(&core.paths, work_id)?;
    let mut selected = HashSet::new();
    let files = tracks
        .iter()
        .filter(|id| selected.insert(id.as_str()))
        .map(|id| {
            let track = track_of(&work, id)?;
            Ok((track.id.as_str(), asset(&media, &track.path)?))
        })
        .collect::<Result<Vec<_>>>()?;
    let mut previous: HashMap<String, (String, String)> = HashMap::new();
    for task in core.manager.list(usize::MAX) {
        if let Some((work, track)) = task_keys(&task) {
            if work == work_id {
                previous
                    .entry(track.to_owned())
                    .or_insert((task.id.clone(), task.status.clone()));
            }
        }
    }
    let (source, target) = crate::services::validated_langs(source.into(), target.into());
    let mut ids = Vec::new();
    for (track, path) in files {
        if let Some((id, status)) = previous.get(track) {
            if matches!(status.as_str(), "error" | "canceled") {
                let _ = crate::services::retry_task(core, id);
            }
            ids.push(id.clone());
            continue;
        }
        let task = core.manager.create_from_path(&path, json!({
            "source_lang": source, "target_lang": target, "library_work": work_id, "library_track": track
        }));
        core.queue_tx
            .send(task.id.clone())
            .map_err(|_| anyhow!("字幕队列已停止"))?;
        ids.push(task.id);
    }
    Ok(ids)
}

// 临时目录与库位于同一卷，作品文件写全后一次改名，扫描不会看到半部作品。
struct ImportDir(PathBuf);

impl Drop for ImportDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn prepare(paths: &Paths) -> Result<(String, ImportDir)> {
    let id = new_task_id().replacen("t_", "w_", 1);
    let dest = paths.library().join(format!(".import-{id}"));
    fs::create_dir(&dest)?;
    let scratch = ImportDir(dest);
    fs::create_dir(scratch.0.join("media"))?;
    Ok((id, scratch))
}

fn ordinary_entry(meta: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if meta.file_attributes() & 0x400 != 0 {
            return false;
        }
    }
    !meta.file_type().is_symlink()
}

fn count_tree(source: &Path) -> Result<(usize, u64)> {
    let mut files = 0usize;
    let mut bytes = 0u64;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let meta = fs::symlink_metadata(entry.path())?;
        if !ordinary_entry(&meta) {
            continue;
        }
        if meta.is_dir() {
            let (f, b) = count_tree(&entry.path())?;
            files += f;
            bytes += b;
        } else if meta.is_file() {
            files += 1;
            bytes += meta.len();
        }
    }
    Ok((files, bytes))
}

struct CopyState<'a> {
    done_files: usize,
    total_files: usize,
    done_bytes: u64,
    total_bytes: u64,
    progress: Option<&'a ImportProgress>,
}

fn copy_tree(
    source: &Path,
    dest: &Path,
    relative: &Path,
    files: &mut Vec<PathBuf>,
    state: &mut CopyState<'_>,
) -> Result<()> {
    let mut entries = fs::read_dir(source)?.collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_cached_key(|e| e.file_name().to_string_lossy().to_lowercase());
    for entry in entries {
        let meta = fs::symlink_metadata(entry.path())?;
        if !ordinary_entry(&meta) {
            continue;
        }
        let to = dest.join(entry.file_name());
        let rel = relative.join(entry.file_name());
        if meta.is_dir() {
            fs::create_dir(&to)?;
            copy_tree(&entry.path(), &to, &rel, files, state)?;
        } else if meta.is_file() {
            fs::copy(entry.path(), &to).with_context(|| format!("复制失败：{}", rel.display()))?;
            files.push(rel);
            state.done_files += 1;
            state.done_bytes = state.done_bytes.saturating_add(meta.len());
            if let Some(cb) = state.progress {
                let name = entry.file_name().to_string_lossy().into_owned();
                cb(
                    state.done_files,
                    state.total_files,
                    state.done_bytes,
                    state.total_bytes,
                    &name,
                );
            }
        }
    }
    Ok(())
}

/// 剥离常见的语言/双语后缀（如 `.ja`、`.zh`、`.bilingual`），返回 `(规范名, 语言优先级)`。
fn normalize_subtitle_stem(raw_stem: &str) -> (String, u8) {
    let lower = raw_stem.to_lowercase();
    if let Some((prefix, suffix)) = lower.rsplit_once('.') {
        if LANG_SUFFIXES.contains(&suffix) && !prefix.is_empty() {
            let lang_rank = match suffix {
                "bilingual" | "dual" => 0,
                "zh" | "zh-cn" | "zh-hans" | "zh-hant" | "zh-tw" | "chs" | "cht" | "cn" => 1,
                _ => 2,
            };
            return (prefix.to_owned(), lang_rank);
        }
    }
    (lower, 1)
}

fn subtitle_ext_rank(ext: &str) -> u8 {
    match ext {
        "vtt" => 0,
        "srt" => 1,
        "ass" | "ssa" => 2,
        "lrc" => 3,
        _ => 4,
    }
}

fn finish(
    paths: &Paths,
    id: String,
    staging: ImportDir,
    title: String,
    files: Vec<PathBuf>,
) -> Result<Work> {
    // 精确同目录匹配（支持 `01.srt`、`01.ja.srt`、`01.bilingual.vtt` 等）
    let mut exact_subs: HashMap<String, ((u8, u8), PathBuf)> = HashMap::new();
    // 跨子目录同名兜底（如字幕统一放在 `字幕/01.srt`）
    let mut loose_subs: HashMap<String, ((u8, u8), PathBuf)> = HashMap::new();
    let mut cover = None;
    for file in &files {
        let ext = extension(file);
        if SUBTITLE_EXTS.contains(&ext.as_str()) {
            let parent = file.parent().unwrap_or_else(|| Path::new(""));
            let file_stem = file
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("");
            let (norm_name, lang_rank) = normalize_subtitle_stem(file_stem);
            let rank = (lang_rank, subtitle_ext_rank(&ext));
            let exact_key = parent.join(&norm_name).to_string_lossy().to_lowercase();
            if exact_subs
                .get(&exact_key)
                .is_none_or(|(best, _)| rank < *best)
            {
                exact_subs.insert(exact_key, (rank, file.clone()));
            }
            if loose_subs
                .get(&norm_name)
                .is_none_or(|(best, _)| rank < *best)
            {
                loose_subs.insert(norm_name, (rank, file.clone()));
            }
            continue;
        }
        match ext.as_str() {
            "jpg" | "jpeg" | "png" | "webp" | "gif" => {
                let stem = file
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("")
                    .to_ascii_lowercase();
                let named = ["cover", "folder", "front", "jacket"]
                    .iter()
                    .any(|s| stem.starts_with(s));
                let priority = (u8::from(!named), file.components().count());
                if cover.as_ref().is_none_or(|(best, _)| priority < *best) {
                    cover = Some((priority, file.clone()));
                }
            }
            _ => {}
        }
    }
    let tracks: Vec<_> = files
        .into_iter()
        .filter(|p| MEDIA_EXTS.contains(&extension(p).as_str()))
        .enumerate()
        .map(|(index, path)| {
            let exact_key = path.with_extension("").to_string_lossy().to_lowercase();
            let base_key = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_lowercase();
            let subtitle = exact_subs
                .get(&exact_key)
                .or_else(|| loose_subs.get(&base_key))
                .map(|(_, p)| p.clone());
            Track {
                id: format!("track_{}", index + 1),
                video: matches!(
                    extension(&path).as_str(),
                    "mp4"
                        | "mkv"
                        | "mov"
                        | "avi"
                        | "webm"
                        | "flv"
                        | "ts"
                        | "m4v"
                        | "wmv"
                        | "mpg"
                        | "mpeg"
                        | "3gp"
                ),
                subtitle,
                path,
            }
        })
        .collect();
    if tracks.is_empty() {
        bail!("文件夹内没有支持的音频或视频");
    }
    let rj = extract_rj(&title);
    let work = Work {
        id,
        title,
        rj,
        cover: cover.map(|(_, path)| path),
        tracks,
    };
    fs::write(
        staging.0.join("work.json"),
        serde_json::to_vec_pretty(&work)?,
    )?;
    fs::rename(&staging.0, paths.library().join(&work.id)).context("保存作品失败")?;
    Ok(work)
}

pub fn import_folder(paths: &Paths, source: &Path) -> Result<Work> {
    import_folder_with_progress(paths, source, None)
}

pub fn import_folder_with_progress(
    paths: &Paths,
    source: &Path,
    progress: Option<ImportProgress>,
) -> Result<Work> {
    let source = source.canonicalize().context("无法打开导入文件夹")?;
    let library = paths.library().canonicalize()?;
    if !source.is_dir() || source.starts_with(&library) || library.starts_with(&source) {
        bail!("请选择作品库之外的单部作品文件夹");
    }
    let title = source
        .file_name()
        .and_then(|s| s.to_str())
        .context("无效的文件夹名")?
        .to_owned();
    let (total_files, total_bytes) = if progress.is_some() {
        count_tree(&source)?
    } else {
        (0, 0)
    };
    let (id, staging) = prepare(paths)?;
    let mut files = Vec::new();
    let mut state = CopyState {
        done_files: 0,
        total_files,
        done_bytes: 0,
        total_bytes,
        progress: progress.as_ref(),
    };
    copy_tree(&source, &staging.0.join("media"), Path::new(""), &mut files, &mut state)?;
    finish(paths, id, staging, title, files)
}

pub fn import_file(paths: &Paths, source: &Path) -> Result<Work> {
    let source = source.canonicalize().context("无法打开媒体文件")?;
    if !source.is_file() || !MEDIA_EXTS.contains(&extension(&source).as_str()) {
        bail!("文件不是支持的音视频：{}", source.display());
    }
    if source.starts_with(paths.library().canonicalize()?) {
        bail!("文件已在作品库内");
    }
    let name = source.file_name().context("无效的媒体文件名")?;
    let title = source
        .file_stem()
        .context("无效的媒体文件名")?
        .to_string_lossy()
        .into_owned();
    let (id, staging) = prepare(paths)?;
    let media = staging.0.join("media");
    fs::copy(&source, media.join(name))?;
    let mut files = vec![PathBuf::from(name)];
    // 复制同目录下同前缀的外挂字幕（含 `.ja.srt`、`.lrc`、`.ass` 等）
    if let Some(parent) = source.parent() {
        let stem_lower = title.to_lowercase();
        if let Ok(entries) = fs::read_dir(parent) {
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_file() {
                    continue;
                }
                let ext = extension(&path);
                if !SUBTITLE_EXTS.contains(&ext.as_str()) {
                    continue;
                }
                let Some(fstem) = path.file_stem().and_then(|s| s.to_str()) else {
                    continue;
                };
                let (norm, _) = normalize_subtitle_stem(fstem);
                if norm == stem_lower {
                    if let Some(fname) = path.file_name() {
                        if fs::copy(&path, media.join(fname)).is_ok() {
                            files.push(PathBuf::from(fname));
                        }
                    }
                }
            }
        }
    }
    finish(paths, id, staging, title, files)
}

fn preview_target(paths: &Paths, work: &str, track: &str) -> Result<(PathBuf, bool)> {
    let manifest = load(paths, work)?;
    let track = track_of(&manifest, track)?;
    if !valid_id(&track.id) {
        bail!("曲目编号无效");
    }
    let extension = if track.video { "mp4" } else { "mp3" };
    let cache = work_dir(paths, work)?.join("preview");
    Ok((cache.join(format!("{}.{extension}", track.id)), track.video))
}

pub fn preview_path(paths: &Paths, work: &str, track: &str) -> Result<PathBuf> {
    let (path, _) = preview_target(paths, work, track)?;
    let base = work_dir(paths, work)?;
    let relative = path.strip_prefix(&base)?;
    asset(&base, relative)
}

pub async fn prepare_preview(paths: &Paths, work: &str, track: &str) -> Result<()> {
    static CONVERT: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let _guard = CONVERT.lock().await;
    if preview_path(paths, work, track).is_ok() {
        return Ok(());
    }
    let source = track_path(paths, work, track)?;
    let (dest, video) = preview_target(paths, work, track)?;
    let cache = dest.parent().context("缺少预览目录")?;
    fs::create_dir_all(cache)?;
    if cache.canonicalize()?.parent() != Some(work_dir(paths, work)?.as_path()) {
        bail!("预览目录不在作品内");
    }
    let temp = dest.with_extension(if video { "partial.mp4" } else { "partial.mp3" });
    let ffmpeg = Ffmpeg::resolve(paths)?;
    let mut cmd = tokio::process::Command::new(ffmpeg.path());
    cmd.kill_on_drop(true)
        .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y", "-i"])
        .arg(source);
    if video {
        cmd.args([
            "-map",
            "0:v:0",
            "-map",
            "0:a:0?",
            "-c:v",
            "libx264",
            "-preset",
            "veryfast",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
            "-movflags",
            "+faststart",
        ]);
    } else {
        cmd.args(["-map", "0:a:0", "-vn", "-c:a", "libmp3lame", "-q:a", "2"]);
    }
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000);
    let output = cmd.arg(&temp).output().await?;
    if !output.status.success() {
        let _ = fs::remove_file(&temp);
        bail!(
            "转换播放格式失败：{}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    fs::rename(&temp, dest)?;
    Ok(())
}

pub async fn import_url(core: &Core, url: &str, progress: DownloadProgress) -> Result<Work> {
    let url = reqwest::Url::parse(url.trim()).context("请输入有效链接")?;
    if !matches!(url.scheme(), "http" | "https") {
        bail!("链接必须使用 HTTP 或 HTTPS");
    }
    let (id, staging) = prepare(&core.paths)?;
    let media = staging.0.join("media");
    let config = core.cfg.get();
    let settings = &config["download"];
    let ffmpeg = Ffmpeg::resolve(&core.paths).ok();
    let result = downloader::download_media_with_options(
        &core.paths,
        ffmpeg.as_ref(),
        url.as_str(),
        &media,
        settings["height"].as_u64().unwrap_or(720) as u32,
        settings["proxy"].as_str().unwrap_or(""),
        settings["cookies_from_browser"].as_str().unwrap_or(""),
        true,
        progress,
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
    )
    .await
    .map_err(|e| match e {
        crate::ffmpeg::StageError::Other(err) => err,
        crate::ffmpeg::StageError::Canceled(message) => anyhow!(message),
    })?;
    let name = Path::new(&result.path)
        .file_name()
        .context("下载结果缺少文件名")?;
    if !media.join(name).is_file() {
        bail!("下载结果未找到");
    }
    // 收集主媒体与 yt-dlp 顺带下载的缩略图（封面）
    let mut files = vec![PathBuf::from(name)];
    if let Ok(entries) = fs::read_dir(&media) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() || entry.file_name() == name {
                continue;
            }
            if matches!(
                extension(&path).as_str(),
                "jpg" | "jpeg" | "png" | "webp" | "gif"
            ) {
                files.push(PathBuf::from(entry.file_name()));
            }
        }
    }
    finish(&core.paths, id, staging, result.title, files)
}
