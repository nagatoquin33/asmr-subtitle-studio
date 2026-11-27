//! 核心纯逻辑单元测试：翻译批处理协议 / 幻觉过滤 / 字幕输出 / mock。
//! 全部离线可跑（cargo test -p asmr-core），证明 core 与 UI/网络完全解耦。

use serde_json::json;

// ---------- 翻译批处理协议 ----------

#[test]
fn parse_reply_basic_and_order() {
    let text = "1|你好\n解释行应被忽略\n2|世界\n3|再次\n2|重复行保留首次\n";
    let parsed = asmr_core::translate::parse_reply(text, None);
    assert_eq!(parsed.len(), 3);
    assert_eq!(parsed[0], (1, "你好".to_string()));
    assert_eq!(parsed[1], (2, "世界".to_string()));
    assert_eq!(parsed[2], (3, "再次".to_string()));
}

#[test]
fn parse_reply_expected_filter() {
    let text = "1|a\n2|b\n7|c\n99|d\n";
    let parsed = asmr_core::translate::parse_reply(text, Some(&[1, 2, 7]));
    assert_eq!(parsed.len(), 3);
    assert!(asmr_core::translate::batch_complete(&parsed, &[1, 2, 7]));
    assert!(!asmr_core::translate::batch_complete(&parsed, &[1, 2, 3]));
}

#[test]
fn align_fallback_keeps_length() {
    let parsed = vec![(1i64, "a".to_string())];
    let out = asmr_core::translate::align_fallback(&parsed, &[1, 2, 3, 4]);
    assert_eq!(out.len(), 4);
    assert_eq!(out[0], "a");
    assert!(out[1..].iter().all(|s| s.is_empty()));
}

#[test]
fn parse_reply_tolerates_whitespace() {
    let parsed = asmr_core::translate::parse_reply("  12 | 译文 带 空格 \n", None);
    assert_eq!(parsed, vec![(12, "译文 带 空格".to_string())]);
}

#[test]
fn mock_translate_marks_and_keeps_symbols() {
    assert_eq!(asmr_core::translate::mock_translate_text("こんにちは"), "[译]こんにちは");
    assert_eq!(asmr_core::translate::mock_translate_text("♪"), "♪"); // 纯符号原样
    assert_eq!(asmr_core::translate::mock_translate_text(""), "");
}

// ---------- 幻觉过滤 ----------

fn seg(i: usize, start: f64, text: &str) -> serde_json::Value {
    json!({"index": i, "start": start, "end": start + 1.0, "text": text, "flagged": false})
}

#[test]
fn filter_drops_repeated_runs() {
    let mut segs: Vec<serde_json::Value> = vec![seg(0, 0.0, "正常A")];
    for i in 1..=5 {
        segs.push(seg(i, i as f64, "お腹に治ってきて")); // x5 连续重复
    }
    segs.push(seg(6, 6.0, "正常B"));
    segs.push(seg(7, 7.0, "お腹に治ってきて")); // 单次出现：保留
    let dropped = asmr_core::asr::filter_hallucination_runs(&mut segs);
    assert_eq!(dropped, 5);
    assert_eq!(segs.len(), 3);
    assert_eq!(segs[0]["text"], "正常A");
    assert_eq!(segs[1]["text"], "正常B");
    assert_eq!(segs[2]["text"], "お腹に治ってきて");
    // index 重排连续
    assert_eq!(segs[2]["index"], 2);
}

#[test]
fn filter_drops_degenerate_repeats() {
    let mut segs = vec![
        seg(0, 0.0, &"哦".repeat(60)),   // 单字符高重复
        seg(1, 1.0, &"あー".repeat(20)), // 双字符高重复
        seg(2, 2.0, "普通の文章です、これは残るはず"),
    ];
    let dropped = asmr_core::asr::filter_hallucination_runs(&mut segs);
    assert_eq!(dropped, 2);
    assert_eq!(segs.len(), 1);
    assert_eq!(segs[0]["text"], "普通の文章です、これは残るはず");
}

#[test]
fn filter_keeps_short_repeats() {
    // 连续 2 次相同是正常口吃/重复台词，不能误杀
    let mut segs = vec![
        seg(0, 0.0, "はい"),
        seg(1, 1.0, "はい"),
        seg(2, 2.0, "そうですね"),
    ];
    let dropped = asmr_core::asr::filter_hallucination_runs(&mut segs);
    assert_eq!(dropped, 0);
    assert_eq!(segs.len(), 3);
}

// ---------- 幻觉标记 ----------

#[test]
fn hallucination_flags() {
    assert!(asmr_core::asr::flag_hallucination("字幕由 Amara.org 提供"));
    assert!(asmr_core::asr::flag_hallucination("谢谢"));
    assert!(!asmr_core::asr::flag_hallucination("今日もお疲れ様でした"));
    assert!(!asmr_core::asr::flag_hallucination("普通"));
}

// ---------- 字幕输出 ----------

#[test]
fn subtitles_write_all_roundtrip() {
    let dir = std::env::temp_dir().join(format!("asmr-test-{}", std::process::id()));
    let segments = vec![
        json!({"index": 0, "start": 0.0, "end": 2.5, "text": "こんにちは", "translated": "你好"}),
        json!({"index": 1, "start": 3.0, "end": 5.25, "text": "Good night", "translated": "晚安"}),
    ];
    let outputs = asmr_core::subtitles::write_all(&dir, &segments, &json!({"bilingual_order": "trans_first", "max_line_chars": 42}));
    // 6 个产物全生成
    for k in ["orig_srt", "trans_srt", "bilingual_srt", "bilingual_ass", "bilingual_vtt", "json"] {
        let p = outputs.get(k).and_then(|v| v.as_str()).unwrap();
        assert!(std::path::Path::new(p).is_file(), "{} 缺失", k);
    }
    // SRT 带 BOM、序号、逗号毫秒
    let srt = std::fs::read(dir.join("bilingual.srt")).unwrap();
    assert_eq!(&srt[..3], &[0xEF, 0xBB, 0xBF]);
    let text = String::from_utf8_lossy(&srt[3..]).to_string();
    assert!(text.contains("00:00:00,000 --> 00:00:02,500"));
    assert!(text.contains("你好"));
    assert!(text.contains("こんにちは"));
    // 双语顺序 trans_first：你好在前
    let hello_pos = text.find("你好").unwrap();
    let konnichiwa_pos = text.find("こんにちは").unwrap();
    assert!(hello_pos < konnichiwa_pos);
    // VTT 无 BOM、点号毫秒
    let vtt = std::fs::read(dir.join("bilingual.vtt")).unwrap();
    assert_ne!(&vtt[..3], &[0xEF, 0xBB, 0xBF]);
    assert!(String::from_utf8_lossy(&vtt).contains("00:00:03.000 --> 00:00:05.250"));
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------- 配置 ----------

#[test]
fn config_deep_merge_and_key_masking() {
    let root = std::env::temp_dir().join(format!("asmr-cfg-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    std::env::set_var("ASMR_HOME", &root);
    let store = asmr_core::config::ConfigStore::new(asmr_core::paths::Paths::detect().unwrap());
    // 深合并补丁
    let out = store.update(&json!({"translate": {"temperature": 0.5, "api_key": "sk-test-123"}}));
    assert_eq!(out["translate"]["temperature"], json!(0.5));
    assert_eq!(out["translate"]["model"], json!("deepseek-flash")); // 未覆盖的保留默认
    assert_eq!(out["translate"]["api_key"], json!("***"));
    assert_eq!(out["translate"]["has_api_key"], json!(true));
    // key 只进 .env 不进 config.json
    let persisted = std::fs::read_to_string(root.join("data/config.json")).unwrap();
    assert!(!persisted.contains("sk-test-123"));
    assert!(root.join(".env").exists());
    // *** 语义：不清除已存 key
    let out2 = store.update(&json!({"translate": {"api_key": "***"}}));
    assert_eq!(out2["translate"]["has_api_key"], json!(true));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn config_migrates_legacy_deepseek_chat() {
    let root = std::env::temp_dir().join(format!("asmr-mig-{}", std::process::id()));
    std::fs::create_dir_all(root.join("data")).unwrap();
    std::fs::write(
        root.join("data/config.json"),
        r#"{"translate": {"model": "deepseek-chat"}}"#,
    )
    .unwrap();
    std::env::set_var("ASMR_HOME", &root);
    let store = asmr_core::config::ConfigStore::new(asmr_core::paths::Paths::detect().unwrap());
    assert_eq!(store.get()["translate"]["model"], json!("deepseek-flash"));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn folder_import_expands_and_filters() {
    use std::fs;
    let root = std::env::temp_dir().join(format!("asmr-folder-{}", std::process::id()));
    let sub = root.join("子目录");
    fs::create_dir_all(&sub).unwrap();
    fs::write(root.join("a.mp4"), b"x").unwrap();
    fs::write(root.join("b.mp3"), b"x").unwrap();
    fs::write(root.join("note.txt"), b"x").unwrap(); // 非媒体 → skipped
    fs::write(root.join(".hidden.mp4"), b"x").unwrap(); // 隐藏 → 跳过
    fs::write(sub.join("c.mkv"), b"x").unwrap(); // 子目录递归

    std::env::set_var("ASMR_HOME", &root);
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let _guard = rt.enter();
    let paths = asmr_core::paths::Paths::detect().unwrap();
    let manager = asmr_core::tasks::TaskManager::new(paths.clone());
    let cfg = asmr_core::config::ConfigStore::new(paths.clone());
    let models = asmr_core::models::ModelStore::new(paths.clone(), tokio::runtime::Handle::current());
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let core = std::sync::Arc::new(asmr_core::pipeline::Core {
        paths,
        cfg,
        manager,
        models,
        queue_tx: tx,
    });
    let (tasks, missing, skipped) = asmr_core::services::create_tasks_from_paths(
        &core,
        &[root.to_string_lossy().to_string()],
        "auto",
        "zh",
    );
    assert_eq!(tasks.len(), 3, "a.mp4/b.mp3/子目录c.mkv 应各建一个任务");
    assert_eq!(skipped, 1, "note.txt 应计为 skipped");
    assert!(missing.is_empty());
    let _ = fs::remove_dir_all(&root);
}

// ---------- 段落编辑纯逻辑：patch / shift / merge / split ----------

fn edit_seg(index: u64, start: f64, end: f64, text: &str, translated: &str, flagged: bool) -> serde_json::Value {
    json!({
        "index": index,
        "start": start,
        "end": end,
        "text": text,
        "translated": translated,
        "flagged": flagged,
    })
}

fn edit_sample() -> Vec<serde_json::Value> {
    vec![
        edit_seg(0, 0.0, 1.0, "A", "甲", false),
        edit_seg(1, 1.0, 2.5, "B", "乙", true),
        edit_seg(2, 2.5, 4.0, "C", "丙", false),
    ]
}

#[test]
fn patch_updates_only_given_fields_and_trims() {
    use asmr_core::pipeline::patch_segment_fields_in;
    let mut segs = edit_sample();
    patch_segment_fields_in(&mut segs, 1, Some("  新原文  "), Some("  新译文 "), None, None).unwrap();
    assert_eq!(segs[1]["text"], json!("新原文"));
    assert_eq!(segs[1]["translated"], json!("新译文"));
    // 未传入的时间字段保持原值
    assert_eq!(segs[1]["start"], json!(1.0));
    assert_eq!(segs[1]["end"], json!(2.5));
    // 其他段完全不变
    assert_eq!(segs[0], edit_seg(0, 0.0, 1.0, "A", "甲", false));
    assert_eq!(segs[2], edit_seg(2, 2.5, 4.0, "C", "丙", false));
    // 空字符串 = 清空（前端允许手工删空）
    patch_segment_fields_in(&mut segs, 0, Some(""), Some(""), None, None).unwrap();
    assert_eq!(segs[0]["text"], json!(""));
    assert_eq!(segs[0]["translated"], json!(""));
}

#[test]
fn patch_enforces_min_duration_and_rounds_times() {
    use asmr_core::pipeline::patch_segment_fields_in;
    let mut segs = edit_sample();
    // 只改 start：end 不足则抬到 start+0.2
    patch_segment_fields_in(&mut segs, 0, None, None, Some(5.0), None).unwrap();
    assert_eq!(segs[0]["start"], json!(5.0));
    assert_eq!(segs[0]["end"], json!(5.2));
    // 只改 end：小于 start 时同样保证 end>start
    patch_segment_fields_in(&mut segs, 1, None, None, None, Some(0.5)).unwrap();
    assert_eq!(segs[1]["end"], json!(1.2), "end 不得小于 start+0.2");
    // 时间保留 3 位小数
    patch_segment_fields_in(&mut segs, 2, None, None, Some(1.23456), Some(2.98765)).unwrap();
    assert_eq!(segs[2]["start"], json!(1.235));
    assert_eq!(segs[2]["end"], json!(2.988));
    // start 下限 0
    patch_segment_fields_in(&mut segs, 2, None, None, Some(-3.0), None).unwrap();
    assert_eq!(segs[2]["start"], json!(0.0));
}

#[test]
fn patch_rejects_bad_index_and_non_finite() {
    use asmr_core::pipeline::patch_segment_fields_in;
    let mut segs = edit_sample();
    let err = patch_segment_fields_in(&mut segs, 9, Some("x"), None, None, None).unwrap_err();
    assert!(err.contains("越界"), "错误串应为中文越界提示: {}", err);
    let err = patch_segment_fields_in(&mut segs, 0, None, None, Some(f64::NAN), None).unwrap_err();
    assert!(err.contains("有效数字"), "{}", err);
    let err = patch_segment_fields_in(&mut segs, 0, None, None, None, Some(f64::INFINITY)).unwrap_err();
    assert!(err.contains("有效数字"), "{}", err);
}

#[test]
fn shift_moves_clamps_and_rounds_to_3dp() {
    use asmr_core::pipeline::shift_segments_in;
    let mut segs = vec![
        edit_seg(0, 0.5, 1.0, "A", "甲", false),
        edit_seg(1, 2.0, 3.0, "B", "乙", false),
    ];
    shift_segments_in(&mut segs, 0.1234).unwrap();
    assert_eq!(segs[0]["start"], json!(0.623));
    assert_eq!(segs[0]["end"], json!(1.123));
    assert_eq!(segs[1]["start"], json!(2.123));
    assert_eq!(segs[1]["end"], json!(3.123));
    // 大负偏移：start 下限 0，且保留 end >= start + 0.05
    shift_segments_in(&mut segs, -10.0).unwrap();
    for seg in &segs {
        assert_eq!(seg["start"], json!(0.0), "start 不得为负: {}", seg);
        assert_eq!(seg["end"], json!(0.05), "end 至少 start+0.05: {}", seg);
    }
}

#[test]
fn shift_keeps_min_duration_without_moving() {
    use asmr_core::pipeline::shift_segments_in;
    let mut segs = vec![
        edit_seg(0, 1.0, 1.01, "短", "短", false),
        edit_seg(1, 2.0, 2.3, "正常", "正常", false),
    ];
    shift_segments_in(&mut segs, 0.0).unwrap();
    assert_eq!(segs[0]["end"], json!(1.05));
    assert_eq!(segs[1]["end"], json!(2.3));
    // index 与顺序不变
    assert_eq!(segs[0]["index"], json!(0));
    assert_eq!(segs[1]["index"], json!(1));
}

#[test]
fn merge_joins_fields_and_reindexes() {
    use asmr_core::pipeline::merge_segments_in;
    let mut segs = vec![
        edit_seg(0, 0.0, 1.0, "A ", "甲 ", false),
        edit_seg(1, 1.0, 2.5, " B", " 乙", true),
        edit_seg(2, 2.5, 4.0, "C", "丙", false),
    ];
    merge_segments_in(&mut segs, 0).unwrap();
    assert_eq!(segs.len(), 2);
    assert_eq!(segs[0]["start"], json!(0.0), "start 取前者");
    assert_eq!(segs[0]["end"], json!(2.5), "end 取后者");
    assert_eq!(segs[0]["text"], json!("A B"), "非空文本以空格拼接并 trim");
    assert_eq!(segs[0]["translated"], json!("甲 乙"));
    assert_eq!(segs[0]["flagged"], json!(true), "flagged 取或");
    // index 连续重排
    assert_eq!(segs[0]["index"], json!(0));
    assert_eq!(segs[1]["index"], json!(1));
    assert_eq!(segs[1]["text"], json!("C"));
}

#[test]
fn merge_skips_empty_side_and_rejects_last_index() {
    use asmr_core::pipeline::merge_segments_in;
    let mut segs = vec![
        edit_seg(0, 0.0, 1.0, "A", "", false),
        edit_seg(1, 1.0, 2.0, "B", "乙", false),
    ];
    merge_segments_in(&mut segs, 0).unwrap();
    assert_eq!(segs[0]["text"], json!("A B"));
    assert_eq!(segs[0]["translated"], json!("乙"), "空串不产生多余空格");
    assert_eq!(segs[0]["flagged"], json!(false));

    let mut single = vec![edit_seg(0, 0.0, 1.0, "only", "独", false)];
    let err = merge_segments_in(&mut single, 0).unwrap_err();
    assert!(err.contains("越界"), "{}", err);
    let mut empty: Vec<serde_json::Value> = Vec::new();
    assert!(merge_segments_in(&mut empty, 0).is_err());
}

#[test]
fn split_by_ratio_inserts_and_reindexes() {
    use asmr_core::pipeline::split_segment_in;
    let mut segs = vec![
        edit_seg(0, 0.0, 10.0, "0123456789", "abcdefghij", true),
        edit_seg(1, 10.0, 12.0, "next", "下", false),
    ];
    split_segment_in(&mut segs, 0, 4.0).unwrap();
    assert_eq!(segs.len(), 3);
    // 原段保留前半
    assert_eq!(segs[0]["start"], json!(0.0));
    assert_eq!(segs[0]["end"], json!(4.0));
    assert_eq!(segs[0]["text"], json!("0123"));
    assert_eq!(segs[0]["translated"], json!("abcd"));
    // 新段在后半
    assert_eq!(segs[1]["start"], json!(4.0));
    assert_eq!(segs[1]["end"], json!(10.0));
    assert_eq!(segs[1]["text"], json!("456789"));
    assert_eq!(segs[1]["translated"], json!("efghij"));
    assert_eq!(segs[1]["flagged"], json!(true));
    // index 连续重排
    assert_eq!(segs[0]["index"], json!(0));
    assert_eq!(segs[1]["index"], json!(1));
    assert_eq!(segs[2]["index"], json!(2));
    assert_eq!(segs[2]["text"], json!("next"), "原后续段后移且内容不变");
}

#[test]
fn split_rejects_invalid_at_and_index() {
    use asmr_core::pipeline::split_segment_in;
    let mut segs = vec![edit_seg(0, 1.0, 2.0, "abcdef", "甲乙丙", false)];
    // at <= start
    assert!(split_segment_in(&mut segs, 0, 1.0).is_err());
    assert!(split_segment_in(&mut segs, 0, 0.5).is_err());
    // at >= end - 0.05（至少保留 50ms）
    let err = split_segment_in(&mut segs, 0, 1.97).unwrap_err();
    assert!(err.contains("不合法"), "{}", err);
    assert!(split_segment_in(&mut segs, 0, 2.0).is_err());
    // 非有限值 / 越界索引
    assert!(split_segment_in(&mut segs, 0, f64::NAN).is_err());
    let err = split_segment_in(&mut segs, 3, 1.5).unwrap_err();
    assert!(err.contains("越界"), "{}", err);
    assert_eq!(segs.len(), 1, "失败时不得改动段落");
}

#[test]
fn split_rounds_times_to_3dp() {
    use asmr_core::pipeline::split_segment_in;
    let mut segs = vec![edit_seg(0, 0.0, 2.0, "abcd", "甲乙", false)];
    split_segment_in(&mut segs, 0, 1.23456).unwrap();
    assert_eq!(segs[0]["end"], json!(1.235));
    assert_eq!(segs[1]["start"], json!(1.235));
    assert_eq!(segs[1]["end"], json!(2.0));
}

#[test]
fn concurrency_and_translated_count_pure_helpers() {
    use asmr_core::pipeline::{clamp_concurrency, translated_count};
    assert_eq!(clamp_concurrency(0), 1);
    assert_eq!(clamp_concurrency(3), 3);
    assert_eq!(clamp_concurrency(6), 6);
    assert_eq!(clamp_concurrency(64), 6, "批量重翻并发上限固定 6");
    let segs = vec![
        edit_seg(0, 0.0, 1.0, "a", "译文", false),
        edit_seg(1, 1.0, 2.0, "b", "", false),
        edit_seg(2, 2.0, 3.0, "c", "[译]mock", false), // mock 标记不算已翻译
    ];
    assert_eq!(translated_count(&segs), 1);
}
