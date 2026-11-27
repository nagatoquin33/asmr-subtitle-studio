//! 全局配置：默认值、加载/保存、.env 密钥管理（与 Python 版 data/config.json 完全同构）。

use std::fs;
use std::path::Path;

use anyhow::Result;
use once_cell::sync::Lazy;
use parking_lot::{Mutex, RwLock};
use serde_json::{json, Map, Value};

use crate::paths::Paths;

pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

static DEFAULT_CONFIG: Lazy<Value> = Lazy::new(|| {
    json!({
        "asr": {
            "backend": "whisper.cpp",
            "model": "small",
            "device": "auto",        // auto / cpu / gpu（gpu=whisper.cpp GPU 后端，如 vulkan）
            "compute_type": "auto",  // 兼容字段：ggml 量化由模型文件决定，仅保留 UI 兼容
            "vad": "auto"            // auto / on / off：auto 下 VAD 过滤光整段音频时自动关闭重试
        },
        "translate": {
            "base_url": "https://api.deepseek.com/v1",
            "model": "deepseek-flash",
            "temperature": 0.2,
            "concurrency": 6,
            "batch_size": 40,
            "proofread": true,
            "mock": false,
            "max_retries": 3,
            "glossary": {
                "囁き": "轻声耳语",
                "耳かき": "掏耳朵",
                "タッピング": "敲击音",
                "トリガー": "触发音"
            }
        },
        "output": {
            "bilingual": true,
            "bilingual_order": "trans_first",
            "max_line_chars": 42
        },
        "download": {
            "height": 720,
            "proxy": "",
            "cookies_from_browser": ""
        },
        "pipeline": {
            "workers": 1,
            "keep_audio": false,
            "move_video_to_output": true  // URL 下载的视频移入产物目录，与字幕同文件夹
        }
    })
});

/// 配置缓存：内存中的完整配置（含运行时注入的 api_key）。
pub struct ConfigStore {
    paths: Paths,
    cache: RwLock<Value>,
    io_lock: Mutex<()>,
}

impl ConfigStore {
    pub fn new(paths: Paths) -> Self {
        let store = Self {
            paths,
            cache: RwLock::new(Value::Null),
            io_lock: Mutex::new(()),
        };
        store.reload();
        store
    }

    pub fn reload(&self) {
        let mut cfg = DEFAULT_CONFIG.clone();
        if let Ok(stored) = fs::read_to_string(self.paths.config_path()) {
            if let Ok(v) = serde_json::from_str::<Value>(&stored) {
                cfg = deep_merge(&cfg, &v);
            } // 配置损坏时回退默认值
        }
        // DeepSeek 旧模型名迁移：deepseek-chat 已弃用（服务端临时映射随时下线）
        if let Some(model) = cfg
            .pointer_mut("/translate/model")
            .and_then(|m| m.as_str().map(|s| s.to_string()))
        {
            if model == "deepseek-chat" {
                if let Some(tr) = cfg.get_mut("translate").and_then(|t| t.as_object_mut()) {
                    tr.insert("model".into(), Value::String("deepseek-flash".into()));
                }
            }
        }
        if let Some(tr) = cfg.get_mut("translate").and_then(|t| t.as_object_mut()) {
            tr.insert("api_key".into(), Value::String(read_env_key(&self.paths)));
        }
        *self.cache.write() = cfg;
    }

    /// 完整配置（含明文 key），仅内部使用。
    pub fn get(&self) -> Value {
        self.cache.read().clone()
    }

    /// 深合并补丁并持久化；api_key 只写 .env。
    pub fn update(&self, patch: &Value) -> Value {
        let mut patch = patch.clone();
        let new_key: Option<String> = {
            let tr = patch.get_mut("translate").and_then(|t| t.as_object_mut());
            match tr {
                None => None,
                Some(obj) => match obj.remove("api_key") {
                    Some(Value::String(k)) if !k.is_empty() && k != "***" => Some(k),
                    _ => None,
                }
            }
        };
        if let Some(k) = new_key {
            let _ = write_env_key(&self.paths, &k);
        }
        let merged = {
            let cur = self.cache.read().clone();
            deep_merge(&cur, &patch)
        };
        self.persist(&merged);
        self.reload();
        // 返回脱敏副本（api_key=***）：与 Python 版 service 层契约一致，
        // 明文 key 永不出后端（内部使用走 self.get()）
        self.public()
    }

    /// 脱敏副本：api_key 以 *** 返回，附带 has_api_key。
    pub fn public(&self) -> Value {
        let mut cfg = self.get();
        if let Some(tr) = cfg.get_mut("translate").and_then(|t| t.as_object_mut()) {
            let key = tr
                .get("api_key")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            tr.insert("api_key".into(), Value::String(if key.is_empty() { String::new() } else { "***".into() }));
            tr.insert("has_api_key".into(), Value::Bool(!key.is_empty()));
        }
        cfg
    }

    fn persist(&self, cfg: &Value) {
        let _guard = self.io_lock.lock();
        let mut persist = cfg.clone();
        if let Some(tr) = persist.get_mut("translate").and_then(|t| t.as_object_mut()) {
            tr.remove("api_key"); // key 只存 .env，不进 config.json
        }
        let _ = self.paths.ensure_dirs();
        let tmp = self.paths.config_path().with_extension("json.tmp");
        if let Ok(text) = serde_json::to_string_pretty(&persist) {
            if fs::write(&tmp, text).is_ok() {
                let _ = fs::rename(&tmp, self.paths.config_path());
            }
        }
        *self.cache.write() = cfg.clone();
    }
}

// ---------- .env 读写 ----------

fn read_env_file(path: &Path) -> Map<String, Value> {
    let mut env = Map::new();
    let Ok(text) = fs::read_to_string(path) else {
        return env;
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            let v = v.trim().trim_matches('"').trim_matches('\'').to_string();
            env.insert(k.trim().to_string(), Value::String(v));
        }
    }
    env
}

fn write_env_file(path: &Path, values: &Map<String, Value>) -> Result<()> {
    let mut text = String::from("# ASMR 字幕工作室 环境变量（LLM_API_KEY，不进 git）\n\n");
    for (k, v) in values {
        text.push_str(&format!("{}={}\n", k, v.as_str().unwrap_or("")));
    }
    fs::write(path, text)?;
    Ok(())
}

fn read_env_key(paths: &Paths) -> String {
    read_env_file(&paths.env_path())
        .get("LLM_API_KEY")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

pub fn write_env_key(paths: &Paths, key: &str) -> Result<()> {
    let mut env = read_env_file(&paths.env_path());
    env.insert(
        "LLM_API_KEY".into(),
        Value::String(key.trim().to_string()),
    );
    write_env_file(&paths.env_path(), &env)
}

pub fn env_api_key(paths: &Paths) -> String {
    read_env_key(paths)
}

// ---------- 深合并 ----------

fn deep_merge(base: &Value, override_with: &Value) -> Value {
    match (base, override_with) {
        (Value::Object(b), Value::Object(o)) => {
            let mut out = b.clone();
            for (k, v) in o {
                let merged = match out.get(k) {
                    Some(bv) if bv.is_object() && v.is_object() => deep_merge(bv, v),
                    _ => v.clone(),
                };
                out.insert(k.clone(), merged);
            }
            Value::Object(out)
        }
        _ => override_with.clone(),
    }
}
