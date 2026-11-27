//! 字幕翻译与审校执行器：批处理「N|译文」严格行协议（完整移植 Python 版语义）。
//!
//! - 每批解析 ^\s*(\d+)\s*\|\s?(.*)$，忽略一切不合规行，同序号保留首次出现
//! - 解析不完整 → 携带第一轮对话历史 + 纠错说明重试一次
//! - 仍失败 → 兜底对齐（序号落位、缺失置空）；行号完全不可用且行数相等 → 按输出顺序对齐
//! - 批间并发（Semaphore），输出顺序与输入严格对齐；单批失败置空不拖垮整篇
//! - 每批携带前 3 句连续「已定稿」译文作上下文

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use regex::Regex;
use serde_json::{json, Value};
use tokio::sync::Semaphore;

use crate::llm::{chat_with_retry, client_for, LlmConfig};

static LINE_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"^\s*(\d+)\s*\|\s?(.*)$").unwrap());

pub type ProgressCb = Arc<dyn Fn(u64, u64, &str) + Send + Sync>;

// ---------------------------------------------------------------------------
// 纯函数：解析 / 校验 / 兜底对齐 / mock
// ---------------------------------------------------------------------------

/// 解析为有序 (序号, 译文) 列表（同序号保留首次出现；expected_ids 白名单过滤）。
pub fn parse_reply(text: &str, expected_ids: Option<&[i64]>) -> Vec<(i64, String)> {
    let mut order: Vec<i64> = Vec::new();
    let mut map: HashMap<i64, String> = HashMap::new();
    for raw in text.lines() {
        let Some(caps) = LINE_RE.captures(raw) else { continue };
        let id: i64 = caps.get(1).unwrap().as_str().parse().unwrap_or(-1);
        if map.contains_key(&id) {
            continue; // 重复行保留首次
        }
        map.insert(id, caps.get(2).unwrap().as_str().trim().to_string());
        order.push(id);
    }
    if let Some(allowed) = expected_ids {
        let set: std::collections::HashSet<i64> = allowed.iter().copied().collect();
        order.retain(|id| set.contains(id));
    }
    order
        .into_iter()
        .map(|id| (id, map.remove(&id).unwrap_or_default()))
        .collect()
}

fn map_of(parsed: &[(i64, String)]) -> HashMap<i64, String> {
    parsed.iter().cloned().collect()
}

pub fn batch_complete(parsed: &[(i64, String)], expected: &[i64]) -> bool {
    parsed.len() == expected.len()
        && parsed.iter().all(|(id, _)| expected.contains(id))
}

/// 兜底对齐：序号能对上的落位，缺失置空；返回长度恒等于 expected.len()。
pub fn align_fallback(parsed: &[(i64, String)], expected: &[i64]) -> Vec<String> {
    let map = map_of(parsed);
    expected
        .iter()
        .map(|id| map.get(id).cloned().unwrap_or_default())
        .collect()
}

/// mock 伪翻译：[译]{原文}；空句 / 纯符号原样返回。
pub fn mock_translate_text(text: &str) -> String {
    if text.trim().is_empty() {
        return text.to_string();
    }
    if !text.chars().any(|c| c.is_alphanumeric()) {
        return text.to_string();
    }
    format!("[译]{}", text)
}

fn one_line(text: &str) -> String {
    text.replace('\r', " ").replace('\n', " ").trim().to_string()
}

// ---------------------------------------------------------------------------
// 提示词（与 Python 版逐字一致）
// ---------------------------------------------------------------------------

pub fn lang_name(lang: Option<&str>) -> String {
    match lang {
        None | Some("auto") | Some("") => "自动检测".to_string(),
        Some("zh") => "中文".to_string(),
        Some("en") => "英文".to_string(),
        Some("ja") => "日文".to_string(),
        Some(other) => other.to_string(),
    }
}

fn glossary_block(glossary: &serde_json::Map<String, Value>) -> String {
    if glossary.is_empty() {
        return String::new();
    }
    let mut lines = vec![
        String::new(),
        String::new(),
        "术语表（强制：原文出现下列词语时，必须按指定译法翻译，并保持全篇一致）：".to_string(),
    ];
    for (k, v) in glossary {
        lines.push(format!("- {} → {}", k, v.as_str().unwrap_or("")));
    }
    lines.join("\n")
}

pub fn translate_system_prompt(
    source_lang: Option<&str>,
    target_lang: &str,
    glossary: &serde_json::Map<String, Value>,
) -> String {
    format!(
        "你是一名专业的影视字幕译者，精通中文、英文、日文，尤其擅长 ASMR、助眠、耳音触发、陪伴哄睡类娱乐内容的翻译，深谙此类作品轻声细语、温柔亲密的氛围。\n\n\
任务：把用户消息中逐行给出的编号台词翻译成{target_name}。原文语言：{source_name}。\n\n\
翻译要求：\n\
1. 忠实原意，语气保持轻声细语、温柔放松，符合 ASMR／助眠氛围；译文要自然口语化，避免书面腔和翻译腔。\n\
2. 日文称呼语按中文习惯意译，例如：お兄ちゃん→哥哥、お姉ちゃん→姐姐，其余称呼按亲昵程度处理。\n\
3. 拟声词、拟态词（如 こつこつ、さらさら、ふわふわ）要转换成自然贴切的中文拟声词，不要保留假名。\n\
4. 原文中的空行、♪、纯音乐／间奏标记等，原样保留输出，不要翻译也不要删除。\n\
5. 严格逐行对应：不合并行、不拆分行、不遗漏行、不增加行，每句独立翻译。\n\
6. 不输出任何解释、注释、罗马音或原文。\n\n\
输出格式（必须严格遵守）：\n\
- 每行一条译文，格式为：N|译文\n\
- 其中 N 是该句在用户消息中的行号（从 1 开始的整数），紧跟半角竖线“|”，之后是该句译文。\n\
- 除上述输出行以外，不要输出任何其他内容。{glossary}",
        target_name = lang_name(Some(target_lang)),
        source_name = lang_name(source_lang),
        glossary = glossary_block(glossary),
    )
}

pub fn proofread_system_prompt(
    target_lang: &str,
    glossary: &serde_json::Map<String, Value>,
) -> String {
    format!(
        "你是一名资深的影视字幕审校，精通中文、英文、日文，熟悉 ASMR、助眠、娱乐向内容，负责审校字幕初译。\n\n\
任务：用户消息中将逐行给出编号台词的原文与初译，请逐句校对，输出审校后的{target_name}译文。\n\n\
审校要求：\n\
1. 修正误译、漏译、语法错误与明显不通顺之处；初译已经正确时保持原样，不要过度改写。\n\
2. 统一全篇术语与称呼的一致性；如提供术语表则必须遵守。\n\
3. 保持轻声细语、温柔放松的氛围与自然口语感，符合 ASMR／助眠场景。\n\
4. 原文为空行、♪ 或纯音乐标记时，对应译文原样保留。\n\
5. 行号与行数必须与输入完全一致：不合并、不拆分、不遗漏、不增加。\n\
6. 不输出任何解释或原文。\n\n\
输出格式（必须严格遵守）：\n\
- 每行一条译文，格式为：N|译文\n\
- 其中 N 是该句的行号（从 1 开始的整数），紧跟半角竖线“|”，之后是审校后的译文。\n\
- 除上述输出行以外，不要输出任何其他内容。{glossary}",
        target_name = lang_name(Some(target_lang)),
        glossary = glossary_block(glossary),
    )
}

fn context_header(context: &[(usize, String)]) -> Vec<String> {
    if context.is_empty() {
        return Vec::new();
    }
    let mut lines = vec!["前文译文（仅供参考，不要输出它们）：".to_string()];
    for (idx, text) in context {
        lines.push(format!("#{} 译文：{}", idx, one_line(text)));
    }
    lines.push(String::new());
    lines
}

pub fn translate_user_message(
    items: &[(i64, &str)],
    context: &[(usize, String)],
    target_lang: &str,
) -> String {
    let mut lines = context_header(context);
    lines.push(format!(
        "请把下面 {} 句台词翻译成{}，输出 {} 行，每行格式严格为 N|译文（N 为行号，从 1 开始）：",
        items.len(),
        lang_name(Some(target_lang)),
        items.len()
    ));
    for (no, text) in items {
        lines.push(format!("{}|{}", no, one_line(text)));
    }
    lines.join("\n")
}

pub fn proofread_user_message(
    items: &[(i64, &str, &str)],
    context: &[(usize, String)],
    target_lang: &str,
) -> String {
    let mut lines = context_header(context);
    lines.push(format!(
        "下面是 {} 句字幕的原文与初译，请审校成{}，输出 {} 行，每行格式严格为 N|审校后的译文（N 为行号，从 1 开始）：",
        items.len(),
        lang_name(Some(target_lang)),
        items.len()
    ));
    for (no, source, draft) in items {
        lines.push(format!("{}|{}", no, one_line(source)));
        lines.push(format!("初译：{}", one_line(draft)));
    }
    lines.join("\n")
}

pub const INCOMPLETE_REPLY_CORRECTION: &str =
    "你上次的输出行数/编号不完整，请严格按 N|译文 每行一条输出全部 N 行，不要输出任何解释或额外内容。";

pub fn correction_note(missing: &[i64]) -> String {
    if missing.is_empty() {
        return INCOMPLETE_REPLY_CORRECTION.to_string();
    }
    let list: Vec<String> = missing.iter().take(20).map(|i| i.to_string()).collect();
    format!("{}（缺失行号：{}）", INCOMPLETE_REPLY_CORRECTION, list.join(", "))
}

// ---------------------------------------------------------------------------
// 配置
// ---------------------------------------------------------------------------

pub struct TranslateConfig {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub temperature: f64,
    pub concurrency: usize,
    pub batch_size: usize,
    pub max_retries: u32,
    pub glossary: serde_json::Map<String, Value>,
    pub mock: bool,
}

impl TranslateConfig {
    pub fn from_cfg(cfg: &Value) -> Self {
        let get_str = |k: &str, d: &str| {
            cfg.get(k)
                .and_then(|v| v.as_str())
                .unwrap_or(d)
                .to_string()
        };
        let glossary = cfg
            .get("glossary")
            .and_then(|v| v.as_object())
            .cloned()
            .unwrap_or_default();
        Self {
            base_url: get_str("base_url", ""),
            api_key: get_str("api_key", ""),
            model: get_str("model", ""),
            temperature: cfg.get("temperature").and_then(|v| v.as_f64()).unwrap_or(0.2),
            concurrency: cfg
                .get("concurrency")
                .and_then(|v| v.as_u64())
                .unwrap_or(3)
                .max(1) as usize,
            batch_size: cfg
                .get("batch_size")
                .and_then(|v| v.as_u64())
                .unwrap_or(40)
                .max(1) as usize,
            max_retries: cfg.get("max_retries").and_then(|v| v.as_u64()).unwrap_or(3) as u32,
            glossary,
            mock: cfg.get("mock").and_then(|v| v.as_bool()).unwrap_or(false),
        }
    }
}

// ---------------------------------------------------------------------------
// 批处理调度
// ---------------------------------------------------------------------------

type Slots = Arc<Vec<Mutex<Option<String>>>>;

/// 收集 start 之前最多 limit 句连续「已定稿」译文。
fn collect_context(slots: &Slots, start: usize, limit: usize) -> Vec<(usize, String)> {
    let mut ctx = Vec::new();
    let mut i = start;
    while i > 0 && ctx.len() < limit {
        match slots[i - 1].lock().clone() {
            Some(t) => {
                ctx.push((i - 1, t));
                i -= 1;
            }
            None => break,
        }
    }
    ctx.reverse();
    ctx
}

/// 单批完整协议：请求 → 解析 → 纠错重试一次 → 兜底对齐。
async fn fetch_batch_aligned(
    client: &reqwest::Client,
    llm: &LlmConfig,
    temperature: f64,
    system_prompt: &str,
    user_msg: String,
    ids: &[i64],
) -> Result<Vec<String>> {
    let messages1 = json!([
        {"role": "system", "content": system_prompt},
        {"role": "user", "content": user_msg},
    ]);
    let raw1 = chat_with_retry(client, llm, &messages1.as_array().unwrap(), temperature).await?;
    let parsed1 = parse_reply(&raw1, Some(ids));
    if batch_complete(&parsed1, ids) {
        let m = map_of(&parsed1);
        return Ok(ids.iter().map(|id| m.get(id).cloned().unwrap_or_default()).collect());
    }

    let missing: Vec<i64> = ids
        .iter()
        .copied()
        .filter(|id| !map_of(&parsed1).contains_key(id))
        .collect();
    let messages2 = json!([
        messages1[0],
        messages1[1],
        {"role": "assistant", "content": raw1},
        {"role": "user", "content": correction_note(&missing)},
    ]);
    let raw2 = chat_with_retry(client, llm, &messages2.as_array().unwrap(), temperature).await?;
    let parsed2 = parse_reply(&raw2, Some(ids));
    if batch_complete(&parsed2, ids) {
        let m = map_of(&parsed2);
        return Ok(ids.iter().map(|id| m.get(id).cloned().unwrap_or_default()).collect());
    }

    // 兜底：取覆盖率更高的一次
    let best = if parsed2.len() >= parsed1.len() { parsed2 } else { parsed1 };
    let mut aligned = align_fallback(&best, ids);
    if aligned.iter().all(|s| s.is_empty()) {
        let u2 = parse_reply(&raw2, None);
        let u1 = parse_reply(&raw1, None);
        let unfiltered = if u2.len() >= u1.len() { u2 } else { u1 };
        if unfiltered.len() == ids.len() {
            aligned = unfiltered.into_iter().map(|(_, v)| v).collect();
        }
    }
    Ok(aligned)
}

/// 并发跑完所有批；build(start, indices, slots) 构建该批 (system_prompt, user_msg)。
/// 单批失败置空并记录；**全部批次失败 → 返回第一个错误**（与 Python 版一致：不让静默空译文溜走）。
async fn run_all(
    tcfg: &TranslateConfig,
    n: usize,
    label: &str,
    progress: &ProgressCb,
    build: Arc<dyn Fn(usize, &[usize], &Slots) -> (String, String) + Send + Sync>,
) -> Result<Vec<String>> {
    use parking_lot::Mutex as PLMutex;

    let bs = tcfg.batch_size.max(1);
    let batches: Vec<Vec<usize>> = (0..n).step_by(bs).map(|s| (s..(s + bs).min(n)).collect()).collect();
    let total_batches = batches.len();

    let client = client_for(&tcfg.base_url);
    let (client, client_err): (Option<Arc<reqwest::Client>>, Option<String>) = match client {
        Ok(c) => (Some(Arc::new(c)), None),
        Err(e) => (None, Some(format!("构建 HTTP 客户端失败: {:#}", e))),
    };
    let slots: Slots = Arc::new((0..n).map(|_| Mutex::new(None)).collect());
    let done_counter = Arc::new(PLMutex::new(0u64));
    let failed_batches = Arc::new(PLMutex::new(Vec::<String>::new()));
    let sem = Arc::new(Semaphore::new(tcfg.concurrency.max(1)));
    let llm_cfg = Arc::new(LlmConfig {
        base_url: tcfg.base_url.clone(),
        api_key: tcfg.api_key.clone(),
        model: tcfg.model.clone(),
        max_retries: tcfg.max_retries,
    });
    let temperature = tcfg.temperature;

    let mut handles = Vec::with_capacity(total_batches);
    for (bi, indices) in batches.into_iter().enumerate() {
        let slots = Arc::clone(&slots);
        let sem = Arc::clone(&sem);
        let done_counter = Arc::clone(&done_counter);
        let failed = Arc::clone(&failed_batches);
        let build = Arc::clone(&build);
        let client = client.clone();
        let client_err = client_err.clone();
        let llm_cfg = Arc::clone(&llm_cfg);
        let label = label.to_string();
        let progress = Arc::clone(progress);
        handles.push(tokio::spawn(async move {
            let _permit = sem.acquire().await.expect("semaphore closed");
            let start = indices[0];
            let (system_prompt, user_msg) = build(start, &indices, &slots);
            let out: Vec<String> = match (client.as_ref(), client_err.as_deref()) {
                (Some(client), _) => {
                    let ids: Vec<i64> = (1..=indices.len() as i64).collect();
                    match fetch_batch_aligned(&client, &llm_cfg, temperature, &system_prompt, user_msg, &ids).await {
                        Ok(v) => v,
                        Err(e) => {
                            // 单批失败：置空 + 记录（不拖垮整篇，但错误必须可见）
                            let msg = format!("{:#}", e);
                            eprintln!("[translate] {} 批次 {}/{} 失败: {}", label, bi + 1, total_batches, msg);
                            failed.lock().push(msg);
                            vec![String::new(); indices.len()]
                        }
                    }
                }
                (None, Some(msg)) => {
                    eprintln!("[translate] {}", msg);
                    failed.lock().push(msg.to_string());
                    vec![String::new(); indices.len()]
                }
                (None, None) => unreachable!(),
            };
            let mut out = out;
            if out.len() != indices.len() {
                out.resize(indices.len(), String::new());
            }
            for (j, text) in out.into_iter().enumerate() {
                *slots[indices[j]].lock() = Some(text);
            }
            let mut done = done_counter.lock();
            *done += indices.len() as u64;
            progress(*done, n as u64, &format!("{} {}/{}", label, bi + 1, total_batches));
        }));
    }
    for h in handles {
        let _ = h.await;
    }
    let failures = failed_batches.lock().clone();
    if !failures.is_empty() {
        eprintln!(
            "[translate] {}：{}/{} 批次失败",
            label,
            failures.len(),
            total_batches.max(failures.len())
        );
        if failures.len() >= total_batches {
            // 全军覆没：按任务失败处理（ASR 结果已落盘，重试只跑翻译）
            return Err(anyhow!(failures[0].clone()));
        }
    }
    Ok(slots
        .iter()
        .map(|s| s.lock().clone().unwrap_or_default())
        .collect())
}

/// mock 模式：离线确定性逐句变换（分批回报进度）。
async fn run_mock(
    texts: &[String],
    label: &str,
    progress: &ProgressCb,
    transform: impl Fn(&str) -> String,
) -> Vec<String> {
    let mut out = Vec::with_capacity(texts.len());
    for (i, t) in texts.iter().enumerate() {
        out.push(transform(t));
        if (i + 1) % 40 == 0 || i + 1 == texts.len() {
            progress((i + 1) as u64, texts.len() as u64, label);
        }
        tokio::task::yield_now().await;
    }
    out
}

fn require_llm(tcfg: &TranslateConfig) -> Result<()> {
    if !tcfg.api_key.trim().is_empty() && !tcfg.base_url.trim().is_empty() {
        return Ok(());
    }
    Err(anyhow!(
        "未配置翻译 API：请到 设置 → 翻译 API 填写 Base URL 与 API Key 并保存，然后在任务上点「重试」即可继续（识别结果已保留，无需重跑）；也可以先开启 Mock 模式体验全流程"
    ))
}

/// 翻译整篇（texts 为原文），返回等长译文列表。
pub async fn translate_segments(
    tcfg: &TranslateConfig,
    texts: Vec<String>,
    source_lang: Option<String>,
    target_lang: String,
    progress: ProgressCb,
) -> Result<Vec<String>> {
    if texts.is_empty() {
        return Ok(Vec::new());
    }
    if tcfg.mock {
        return Ok(run_mock(&texts, "翻译批次", &progress, mock_translate_text).await);
    }
    require_llm(tcfg)?;
    let sys = translate_system_prompt(source_lang.as_deref(), &target_lang, &tcfg.glossary);
    let texts = Arc::new(texts);
    let n = texts.len();
    let target = target_lang.clone();
    let build: Arc<dyn Fn(usize, &[usize], &Slots) -> (String, String) + Send + Sync> = {
        let sys = sys.clone();
        Arc::new(move |start, indices, slots| {
            let items: Vec<(i64, &str)> = indices
                .iter()
                .enumerate()
                .map(|(k, &i)| ((k + 1) as i64, texts[i].as_str()))
                .collect();
            let ctx = collect_context(slots, start, 3);
            let user = translate_user_message(&items, &ctx, &target);
            (sys.clone(), user)
        })
    };
    Ok(run_all(tcfg, n, "翻译批次", &progress, build).await?)
}

/// 审校整篇（原文 + 初译 → 润色译文），长度恒等于 sources.len()。
pub async fn proofread_segments(
    tcfg: &TranslateConfig,
    sources: Vec<String>,
    drafts: Vec<String>,
    target_lang: String,
    progress: ProgressCb,
) -> Result<Vec<String>> {
    let n = sources.len();
    if n == 0 {
        return Ok(Vec::new());
    }
    let mut drafts = drafts;
    drafts.resize(n, String::new());
    if tcfg.mock {
        return Ok(run_mock(&drafts, "审校批次", &progress, |t| t.to_string()).await);
    }
    let sys = proofread_system_prompt(&target_lang, &tcfg.glossary);
    let sources = Arc::new(sources);
    let drafts = Arc::new(drafts);
    let target = target_lang.clone();
    let build: Arc<dyn Fn(usize, &[usize], &Slots) -> (String, String) + Send + Sync> =
        Arc::new(move |start, indices, slots| {
            let items: Vec<(i64, &str, &str)> = indices
                .iter()
                .enumerate()
                .map(|(k, &i)| ((k + 1) as i64, sources[i].as_str(), drafts[i].as_str()))
                .collect();
            let ctx = collect_context(slots, start, 3);
            let user = proofread_user_message(&items, &ctx, &target);
            (sys.clone(), user)
        });
    Ok(run_all(tcfg, n, "审校批次", &progress, build).await?)
}

/// 单句重翻：取前 3 句已定稿译文作上下文，翻最后一句。
pub async fn retranslate_one(
    tcfg: &TranslateConfig,
    context_texts: Vec<String>, // 含目标句，取最后一句重翻
    target_lang: &str,
) -> Result<String> {
    if tcfg.mock {
        return Ok(context_texts
            .last()
            .map(|t| mock_translate_text(t))
            .unwrap_or_default());
    }
    require_llm(tcfg)?;
    let client = client_for(&tcfg.base_url)?;
    let llm = LlmConfig {
        base_url: tcfg.base_url.clone(),
        api_key: tcfg.api_key.clone(),
        model: tcfg.model.clone(),
        max_retries: tcfg.max_retries,
    };
    let items: Vec<(i64, &str)> = context_texts
        .iter()
        .enumerate()
        .map(|(k, t)| ((k + 1) as i64, t.as_str()))
        .collect();
    let user = translate_user_message(&items, &[], target_lang);
    let ids: Vec<i64> = (1..=items.len() as i64).collect();
    let sys = translate_system_prompt(None, target_lang, &tcfg.glossary);
    let out = fetch_batch_aligned(&client, &llm, tcfg.temperature, &sys, user, &ids).await?;
    Ok(out.into_iter().next().unwrap_or_default())
}
