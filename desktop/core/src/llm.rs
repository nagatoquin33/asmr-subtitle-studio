//! OpenAI 兼容 LLM 客户端：chat/completions + 指数退避重试 + 连通性测试。

use std::time::Duration;

use anyhow::{anyhow, Result};
use serde_json::{json, Value};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);
const BASE_DELAY: f64 = 2.0;
const MAX_DELAY: f64 = 60.0;

pub struct LlmConfig {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub max_retries: u32,
}

pub fn client_for(_base_url: &str) -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .timeout(DEFAULT_TIMEOUT)
        .user_agent("asmr-studio/2.0")
        .build()?)
}

fn chat_url(base_url: &str) -> String {
    let base = base_url.trim().trim_end_matches('/');
    format!("{}/chat/completions", base)
}

/// 非流式 chat 请求，带退避重试；返回首个 choice 的 content（空按 ""）。
pub async fn chat_with_retry(
    client: &reqwest::Client,
    cfg: &LlmConfig,
    messages: &[Value],
    temperature: f64,
) -> Result<String> {
    let url = chat_url(&cfg.base_url);
    let mut attempt: u32 = 0;
    loop {
        let mut req = client
            .post(&url)
            .header("Content-Type", "application/json")
            .json(&json!({
                "model": cfg.model,
                "messages": messages,
                "temperature": temperature,
                "stream": false,
            }));
        if !cfg.api_key.trim().is_empty() {
            req = req.bearer_auth(cfg.api_key.trim());
        }
        match req.send().await {
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() {
                    let body: Value = resp.json().await.map_err(|e| anyhow!("解析响应失败: {}", e))?;
                    let content = body
                        .pointer("/choices/0/message/content")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    return Ok(content);
                }
                let retry_after = resp
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<f64>().ok());
                let body_text = resp.text().await.unwrap_or_default();
                let brief: String = body_text.chars().take(300).collect();
                let retriable = status.as_u16() == 429 || status.as_u16() >= 500;
                if !retriable {
                    return Err(anyhow!("HTTP {}: {}", status.as_u16(), brief));
                }
                if attempt >= cfg.max_retries {
                    return Err(anyhow!("HTTP {}（已重试 {} 次）: {}", status.as_u16(), attempt, brief));
                }
                let mut delay = BASE_DELAY * (1u64 << attempt.min(6)) as f64;
                if let Some(ra) = retry_after {
                    delay = delay.max(ra);
                }
                delay = delay.min(MAX_DELAY);
                tokio::time::sleep(Duration::from_secs_f64(delay)).await;
                attempt += 1;
            }
            Err(e) => {
                // 超时 / 连接错误 → 可重试
                if attempt >= cfg.max_retries {
                    return Err(anyhow!("请求失败（已重试 {} 次）: {}", attempt, e));
                }
                let delay = (BASE_DELAY * (1u64 << attempt.min(6)) as f64).min(MAX_DELAY);
                tokio::time::sleep(Duration::from_secs_f64(delay)).await;
                attempt += 1;
            }
        }
    }
}

/// 连通性测试：极小请求，返回 {ok, message, sample}，绝不抛异常。
pub async fn test_connection(cfg: &LlmConfig, mock: bool) -> Value {
    if mock {
        return json!({
            "ok": true,
            "message": "mock 模式：未发起网络请求，连通性测试直接通过",
            "sample": "[mock] OK",
        });
    }
    if cfg.base_url.trim().is_empty() {
        return json!({"ok": false, "message": "未配置 Base URL", "sample": ""});
    }
    let client = match client_for(&cfg.base_url) {
        Ok(c) => c,
        Err(e) => return json!({"ok": false, "message": format!("{}", e), "sample": ""}),
    };
    let mut probe = LlmConfig {
        base_url: cfg.base_url.clone(),
        api_key: cfg.api_key.clone(),
        model: cfg.model.clone(),
        max_retries: cfg.max_retries.min(2),
    };
    if probe.max_retries == 0 && cfg.max_retries > 0 {
        probe.max_retries = 1;
    }
    let probe_msgs = json!([{"role": "user", "content": "请只回复两个字符：OK"}]);
    match chat_with_retry(
        &client,
        &probe,
        probe_msgs.as_array().unwrap(),
        0.0,
    )
    .await
    {
        Ok(text) => {
            let sample = text.trim().to_string();
            if sample.is_empty() {
                json!({"ok": false, "message": "连接成功，但模型返回内容为空", "sample": ""})
            } else {
                json!({
                    "ok": true,
                    "message": format!("连接成功（model={}）", probe.model),
                    "sample": sample.chars().take(500).collect::<String>(),
                })
            }
        }
        Err(e) => json!({"ok": false, "message": format!("{:#}", e).chars().take(300).collect::<String>(), "sample": ""}),
    }
}
