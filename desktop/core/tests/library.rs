use std::fs;
use std::path::{Path, PathBuf};

use asmr_core::{config::ConfigStore, library, models::ModelStore, paths::Paths, tasks, Core};
use serde_json::json;

struct Temp(PathBuf);

impl Temp {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("asmr-library-{}", tasks::new_task_id()));
        fs::create_dir(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn imports_a_complete_work_and_plays_after_moving_the_library() {
    let tmp = Temp::new();
    let paths = Paths {
        root: tmp.0.join("app"),
    };
    paths.ensure_dirs().unwrap();
    let source = tmp.0.join("夜の試聴 [rj01234567]");
    let disc = source.join("本編");
    fs::create_dir_all(&disc).unwrap();
    fs::write(disc.join("01.wav"), b"demo wav").unwrap();
    fs::write(disc.join("01.flac"), b"demo flac").unwrap();
    fs::write(
        disc.join("01.srt"),
        "\u{feff}1\r\n00:00:00,100 --> 00:00:01,000\r\n試聴です。\r\n",
    )
    .unwrap();
    fs::write(source.join("cover.jpg"), b"cover").unwrap();
    fs::write(source.join("说明.txt"), "read me").unwrap();

    let work = library::import_folder(&paths, &source).unwrap();
    assert_eq!(work.rj.as_deref(), Some("RJ01234567"));
    assert_eq!(work.tracks.len(), 2);
    assert!(work.tracks.iter().all(|track| track.subtitle.is_some()));
    assert_eq!(library::list(&paths).unwrap().len(), 1);
    assert!(paths
        .library()
        .join(&work.id)
        .join("media")
        .join("说明.txt")
        .is_file());
    assert!(disc.join("01.wav").is_file());
    assert!(library::import_folder(&paths, &paths.library()).is_err());

    library::rename(&paths, &work.id, "夜の試聴").unwrap();
    let relocated = Paths {
        root: tmp.0.join("moved"),
    };
    fs::rename(&paths.root, &relocated.root).unwrap();
    let first = &work.tracks[0].id;
    let playable = library::track_path(&relocated, &work.id, first).unwrap();
    assert!(playable.starts_with(relocated.library().canonicalize().unwrap()));
    let vtt = library::vtt(&relocated, &work.id, first).unwrap();
    assert!(vtt.starts_with("WEBVTT\n\n"));
    assert!(vtt.contains("00:00:00.100 --> 00:00:01.000"));
    assert!(vtt.contains("試聴です。"));
    assert!(library::cover_path(&relocated, &work.id).unwrap().is_file());
    assert_eq!(
        library::load(&relocated, &work.id).unwrap().title,
        "夜の試聴"
    );
    assert!(library::track_path(&relocated, "..", first).is_err());
}

#[tokio::test]
async fn only_queues_after_import_and_uses_a_stable_subtitle_folder() {
    let tmp = Temp::new();
    let paths = Paths {
        root: tmp.0.join("app"),
    };
    paths.ensure_dirs().unwrap();
    let media = tmp.0.join("试听.mp3");
    fs::write(&media, b"demo").unwrap();
    let (queue_tx, mut queue_rx) = tokio::sync::mpsc::unbounded_channel();
    let core = Core {
        cfg: ConfigStore::new(paths.clone()),
        manager: tasks::TaskManager::new(paths.clone()),
        models: ModelStore::new(paths.clone(), tokio::runtime::Handle::current()),
        paths,
        queue_tx,
    };
    let work = library::import_file(&core.paths, &media).unwrap();
    let track = work.tracks[0].id.clone();
    assert!(core.manager.list(1).is_empty());
    assert!(queue_rx.try_recv().is_err());
    assert!(library::queue(
        &core,
        &work.id,
        &[track.clone(), "missing".into()],
        "auto",
        "zh"
    )
    .is_err());
    assert!(core.manager.list(1).is_empty());
    let ids = library::queue(&core, &work.id, std::slice::from_ref(&track), "ja", "zh").unwrap();
    assert_eq!(ids.len(), 1);
    assert_eq!(queue_rx.try_recv().unwrap(), ids[0]);
    assert_eq!(
        library::queue(&core, &work.id, std::slice::from_ref(&track), "ja", "zh").unwrap(),
        ids
    );
    assert!(queue_rx.try_recv().is_err());

    let task = core.manager.get(&ids[0]).unwrap();
    let dir = library::task_output_dir(&core.paths, &task).unwrap();
    assert!(dir.ends_with(Path::new(&work.id).join("subtitles").join(&track)));
    assert_eq!(task.options["source_lang"], "ja");
    asmr_core::subtitles::write_all(
        &dir,
        &[json!({
            "start": 0.0, "end": 1.0, "text": "音", "translated": "声音"
        })],
        &json!({}),
    )
    .unwrap();
    assert!(dir.join("bilingual.srt").is_file());
    assert!(library::vtt(&core.paths, &work.id, &track)
        .unwrap()
        .contains("声音"));
    assert!(
        asmr_core::services::output_path(&core, &ids[0], "bilingual_srt")
            .unwrap()
            .is_file()
    );

    // 验证重复导入检测、残留 .import-* 清理、以及删除作品
    assert!(library::find_duplicate(&core.paths, &media).unwrap().is_some());
    let stale = core.paths.library().join(".import-w_stale_test");
    fs::create_dir_all(&stale).unwrap();
    library::cleanup_stale_imports(&core.paths);
    assert!(!stale.exists());

    // 取消/失败的任务在重新 queue 时应自动重试而不是停在失败态
    core.manager
        .update(&ids[0], true, tasks::EventKind::Error, |t| {
            t.status = "error".into();
        });
    let retried = library::queue(&core, &work.id, std::slice::from_ref(&track), "ja", "zh").unwrap();
    assert_eq!(retried, ids);
    assert_eq!(core.manager.get(&ids[0]).unwrap().status, "queued");
    assert_eq!(queue_rx.try_recv().unwrap(), ids[0]);

    // 完成后删除作品应同时移除目录和关联任务
    core.manager
        .update(&ids[0], true, tasks::EventKind::Done, |t| {
            t.status = "done".into();
        });
    library::delete(&core, &work.id).unwrap();
    assert!(library::list(&core.paths).unwrap().is_empty());
    assert!(core.manager.get(&ids[0]).is_none());
}

#[test]
fn decodes_multilingual_subtitles_and_matches_tagged_or_lrc_ass_files() {
    let tmp = Temp::new();
    let paths = Paths {
        root: tmp.0.join("app"),
    };
    paths.ensure_dirs().unwrap();
    let source = tmp.0.join("多格式字幕 [RJ998877]");
    let sub_dir = source.join("字幕");
    fs::create_dir_all(&sub_dir).unwrap();
    fs::write(source.join("01_track.mp3"), b"audio").unwrap();
    fs::write(source.join("02_track.mp3"), b"audio").unwrap();
    fs::write(source.join("03_track.mp3"), b"audio").unwrap();

    // 01: 语言后缀 .zh.srt + UTF-16LE BOM
    let srt_utf16 = "1\r\n00:00:01,000 --> 00:00:03,500\r\n你好，耳语\r\n";
    let mut utf16_bytes = vec![0xFFu8, 0xFE];
    for unit in srt_utf16.encode_utf16() {
        utf16_bytes.extend_from_slice(&unit.to_le_bytes());
    }
    fs::write(source.join("01_track.zh.srt"), utf16_bytes).unwrap();

    // 02: 子目录 LRC 字幕
    fs::write(
        sub_dir.join("02_track.lrc"),
        "[ti:测试]\n[00:01.20]第一句台词\n[00:04.50]第二句台词\n",
    )
    .unwrap();

    // 03: ASS 字幕（含特效标签与 \N 换行）
    fs::write(
        source.join("03_track.ja.ass"),
        "[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\nDialogue: 0,0:00:02.10,0:00:05.40,Default,,0,0,0,,{\\an8}耳元で\\Nささやく\n",
    )
    .unwrap();

    let work = library::import_folder(&paths, &source).unwrap();
    assert_eq!(work.tracks.len(), 3);

    let vtt1 = library::vtt(&paths, &work.id, &work.tracks[0].id).unwrap();
    assert!(vtt1.contains("00:00:01.000 --> 00:00:03.500"));
    assert!(vtt1.contains("你好，耳语"));

    let vtt2 = library::vtt(&paths, &work.id, &work.tracks[1].id).unwrap();
    assert!(vtt2.contains("00:00:01.200 --> 00:00:04.500"));
    assert!(vtt2.contains("第一句台词"));

    let vtt3 = library::vtt(&paths, &work.id, &work.tracks[2].id).unwrap();
    assert!(vtt3.contains("00:00:02.100 --> 00:00:05.400"));
    assert!(vtt3.contains("耳元で\nささやく"));
    assert!(!vtt3.contains("\\an8"));

    // Shift-JIS 日文字幕字节验证（"あいう" 在 CP932 中为 82 A0 82 A2 82 A4）
    #[cfg(windows)]
    {
        let sjis = [0x82u8, 0xA0, 0x82, 0xA2, 0x82, 0xA4];
        assert_eq!(library::decode_subtitle_bytes(&sjis), "あいう");
    }
}
